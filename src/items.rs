//! Plex metadata → ricercar items, and whether the DAC takes a file.
//!
//! Refs are `<prefix>/<ratingKey>`: `t` track, `a` album, `r` artist, `p`
//! playlist. Top-level sections use bare words (`albums`, `artists`…).

use serde_json::{Value, json};

use crate::plex::Session;

/// Kind prefix and rating key of a ref. Rating keys are numbers.
pub fn split_ref(r: &str) -> Option<(&str, &str)> {
    let (k, id) = r.split_once('/')?;
    let ok = matches!(k, "t" | "a" | "r" | "p")
        && !id.is_empty()
        && id.len() <= 20
        && id.bytes().all(|b| b.is_ascii_digit());
    ok.then_some((k, id))
}

fn text(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn num(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(Value::as_i64).filter(|n| *n > 0)
}

fn join(parts: &[Option<String>]) -> Option<String> {
    let v: Vec<&str> = parts.iter().flatten().map(String::as_str).collect();
    (!v.is_empty()).then(|| v.join(" · "))
}

fn genre(v: &Value) -> Option<String> {
    v["Genre"][0]["tag"].as_str().map(str::to_string)
}

/// The audio stream of a media part (`streamType` 2), when the answer
/// details streams (single-item requests do, lists do not).
pub fn audio_stream(media: &Value) -> Option<&Value> {
    media["Part"][0]["Stream"]
        .as_array()?
        .iter()
        .filter(|s| s["streamType"] == 2)
        .find(|s| s["selected"] == true)
        .or_else(|| {
            media["Part"][0]["Stream"]
                .as_array()?
                .iter()
                .find(|s| s["streamType"] == 2)
        })
}

/// The first version of an item that has a file.
pub fn media(v: &Value) -> Option<&Value> {
    v["Media"]
        .as_array()?
        .iter()
        .find(|m| m["Part"][0]["key"].is_string())
}

/// `{sample_rate, bits, channels, codec}` of the file as stored.
pub fn format(v: &Value) -> Option<Value> {
    let m = media(v)?;
    let mut f = serde_json::Map::new();
    let st = audio_stream(m);
    if let Some(r) = st.and_then(|s| num(s, "samplingRate")) {
        f.insert("sample_rate".into(), r.into());
    }
    if let Some(b) = st.and_then(|s| num(s, "bitDepth")) {
        f.insert("bits".into(), b.into());
    }
    if let Some(c) = num(m, "audioChannels").or_else(|| st.and_then(|s| num(s, "channels"))) {
        f.insert("channels".into(), c.into());
    }
    if let Some(c) = text(m, "audioCodec").or_else(|| st.and_then(|s| text(s, "codec"))) {
        f.insert("codec".into(), c.to_lowercase().into());
    }
    (!f.is_empty()).then_some(Value::Object(f))
}

fn finish(s: &Session, thumb: Option<String>, mut it: Value) -> Value {
    if let Some(t) = thumb {
        it["art"] = s.art(&t).into();
    }
    // Leave optional fields out rather than send nulls.
    if let Some(o) = it.as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
    it
}

pub fn track(s: &Session, v: &Value) -> Option<Value> {
    if v["type"] != "track" {
        return None;
    }
    let id = v["ratingKey"].as_str()?;
    let title = text(v, "title").unwrap_or_else(|| "?".into());
    let album_artist = text(v, "grandparentTitle");
    // `originalTitle` holds the track artist when it differs from the
    // album's (compilations, guests).
    let artist = text(v, "originalTitle").or_else(|| album_artist.clone());
    let album = text(v, "parentTitle");
    let it = json!({
        "ref": format!("t/{id}"),
        "kind": "track",
        "title": title,
        "subtitle": join(&[artist.clone(), album.clone()]),
        "artist": artist,
        "album": album,
        "album_artist": album_artist,
        "track_no": num(v, "index"),
        "disc_no": num(v, "parentIndex"),
        "year": num(v, "parentYear").or_else(|| num(v, "year")),
        "genre": genre(v),
        "duration_ms": num(v, "duration"),
        "format": format(v),
        "playable": media(v).is_some(),
    });
    let thumb = text(v, "parentThumb")
        .or_else(|| text(v, "thumb"))
        .or_else(|| text(v, "grandparentThumb"));
    Some(finish(s, thumb, it))
}

pub fn album(s: &Session, v: &Value) -> Option<Value> {
    if v["type"] != "album" {
        return None;
    }
    let id = v["ratingKey"].as_str()?;
    let title = text(v, "title");
    let artist = text(v, "parentTitle");
    let year = num(v, "year");
    let it = json!({
        "ref": format!("a/{id}"),
        "kind": "album",
        "title": title.clone().unwrap_or_else(|| "?".into()),
        "subtitle": join(&[artist.clone(), year.map(|y| y.to_string())]),
        "artist": artist,
        "album": title,
        "year": year,
        "genre": genre(v),
        "track_count": num(v, "leafCount"),
        "browsable": true,
    });
    Some(finish(s, text(v, "thumb"), it))
}

pub fn artist(s: &Session, v: &Value) -> Option<Value> {
    if v["type"] != "artist" {
        return None;
    }
    let id = v["ratingKey"].as_str()?;
    let name = text(v, "title");
    let it = json!({
        "ref": format!("r/{id}"),
        "kind": "artist",
        "title": name.clone().unwrap_or_else(|| "?".into()),
        "subtitle": num(v, "childCount").map(|n| format!("{n} ◫")),
        "artist": name,
        "genre": genre(v),
        "browsable": true,
    });
    Some(finish(s, text(v, "thumb"), it))
}

pub fn playlist(s: &Session, v: &Value) -> Option<Value> {
    if v["type"] != "playlist" || v["playlistType"] != "audio" {
        return None;
    }
    let id = v["ratingKey"].as_str()?;
    let it = json!({
        "ref": format!("p/{id}"),
        "kind": "playlist",
        "title": text(v, "title").unwrap_or_else(|| "?".into()),
        "subtitle": v["leafCount"].as_i64().map(|n| format!("{n} ♪")),
        "track_count": v["leafCount"].as_i64(),
        "browsable": true,
    });
    let thumb = text(v, "thumb").or_else(|| text(v, "composite"));
    Some(finish(s, thumb, it))
}

/// Any music item, by its `type`.
pub fn any(s: &Session, v: &Value) -> Option<Value> {
    match v["type"].as_str()? {
        "track" => track(s, v),
        "album" => album(s, v),
        "artist" => artist(s, v),
        "playlist" => playlist(s, v),
        _ => None,
    }
}

pub fn many(s: &Session, list: &[Value], f: fn(&Session, &Value) -> Option<Value>) -> Vec<Value> {
    list.iter().filter_map(|x| f(s, x)).collect()
}

/// Track gain and peak from Plex's loudness analysis, when it ran.
pub fn replaygain(stream: &Value) -> Option<Value> {
    let g = stream["gain"].as_f64()?;
    let mut rg = json!({ "track_gain": g });
    if let Some(p) = stream["peak"].as_f64().filter(|p| *p > 0.0) {
        rg["track_peak"] = p.into();
    }
    Some(rg)
}

/// What the DAC takes natively, from `initialize` / `output.changed`.
#[derive(Clone, Debug, Default)]
pub struct Output {
    pub bit_perfect: bool,
    pub max_rate: Option<u32>,
    pub max_bits: Option<u8>,
    pub rates: Vec<u32>,
}

impl Output {
    pub fn from_json(v: &Value) -> Output {
        Output {
            bit_perfect: v["bit_perfect"].as_bool().unwrap_or(false),
            max_rate: v["max_rate"].as_u64().map(|r| r as u32),
            max_bits: v["max_bits"].as_u64().map(|b| b as u8),
            rates: v["rates"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|r| r.as_u64())
                        .map(|r| r as u32)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn takes_rate(&self, rate: u32) -> bool {
        if self.rates.is_empty() {
            self.max_rate.is_none_or(|m| rate <= m)
        } else {
            self.rates.contains(&rate)
        }
    }
}

/// How to fetch a track so that the engine plays it without converting it.
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// The original file, byte for byte.
    Direct,
    /// A FLAC transcode at a rate the DAC accepts, same bit depth.
    Flac { rate: u32 },
    /// Nothing fits: the DAC takes fewer bits than the file has, and the
    /// Plex transcoder keeps the depth of its source.
    Unfit,
}

/// Keep the original unless the DAC cannot take its rate; then ask for
/// FLAC at the closest rate the DAC takes, preferring the same family
/// (44.1 kHz or 48 kHz multiples) and never going up. Unknown rates keep
/// the original: the engine has the last word.
pub fn plan(out: &Output, rate: Option<u32>, bits: Option<u8>) -> Plan {
    let Some(rate) = rate else {
        return Plan::Direct;
    };
    if !out.bit_perfect {
        return Plan::Direct;
    }
    if bits.is_some_and(|b| out.max_bits.is_some_and(|m| b > m)) {
        return Plan::Unfit;
    }
    if out.takes_rate(rate) {
        return Plan::Direct;
    }
    let mut candidates: Vec<u32> = if out.rates.is_empty() {
        [
            44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
        ]
        .into_iter()
        .filter(|r| out.takes_rate(*r))
        .collect()
    } else {
        out.rates.clone()
    };
    candidates.sort_unstable();
    let family = |r: u32| r % 11_025 == 0;
    let target = candidates
        .iter()
        .rev()
        .find(|r| **r <= rate && family(**r) == family(rate))
        .or_else(|| candidates.iter().rev().find(|r| **r <= rate))
        .or_else(|| candidates.first())
        .copied()
        .unwrap_or(rate);
    Plan::Flac { rate: target }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session {
            server: "http://pms:32400".into(),
            connections: vec![],
            server_name: String::new(),
            machine_id: String::new(),
            token: "k".into(),
            user: String::new(),
        }
    }

    fn dac(rates: &[u32], bits: u8) -> Output {
        Output {
            bit_perfect: true,
            max_rate: rates.iter().max().copied(),
            max_bits: Some(bits),
            rates: rates.to_vec(),
        }
    }

    #[test]
    fn refs() {
        assert_eq!(split_ref("t/123"), Some(("t", "123")));
        assert_eq!(split_ref("p/9"), Some(("p", "9")));
        assert_eq!(split_ref("x/1"), None);
        assert_eq!(split_ref("t/"), None);
        assert_eq!(split_ref("t/1/../x"), None);
        assert_eq!(split_ref("albums"), None);
    }

    #[test]
    fn track_mapping() {
        // A detailed answer (`/library/metadata/<id>`).
        let v = json!({
            "ratingKey": "3", "type": "track", "title": "Coda",
            "grandparentTitle": "Ensemble", "originalTitle": "Ensemble feat. Guest",
            "parentTitle": "Sessions", "index": 3, "parentIndex": 1, "parentYear": 2021,
            "duration": 245000, "parentThumb": "/library/metadata/2/thumb/7",
            "Genre": [{"tag": "Jazz"}],
            "Media": [{"audioChannels": 2, "audioCodec": "flac", "Part": [{
                "key": "/library/parts/1/1/file.flac",
                "Stream": [{"streamType": 2, "selected": true, "codec": "flac",
                            "samplingRate": 96000, "bitDepth": 24, "gain": -7.2, "peak": 0.98}]
            }]}]
        });
        let it = track(&session(), &v).unwrap();
        assert_eq!(it["ref"], "t/3");
        assert_eq!(it["artist"], "Ensemble feat. Guest");
        assert_eq!(it["album_artist"], "Ensemble");
        assert_eq!(it["subtitle"], "Ensemble feat. Guest · Sessions");
        assert_eq!(it["duration_ms"], 245_000);
        assert_eq!(it["year"], 2021);
        assert_eq!(it["genre"], "Jazz");
        assert_eq!(it["playable"], true);
        assert_eq!(
            it["format"],
            json!({"sample_rate": 96000, "bits": 24, "channels": 2, "codec": "flac"})
        );
        assert!(
            it["art"]
                .as_str()
                .unwrap()
                .contains("url=%2Flibrary%2Fmetadata%2F2%2Fthumb%2F7")
        );
        let st = audio_stream(media(&v).unwrap()).unwrap();
        assert_eq!(
            replaygain(st),
            Some(json!({"track_gain": -7.2, "track_peak": 0.98}))
        );
    }

    #[test]
    fn list_track() {
        // Lists carry no streams: codec and channels only.
        let v = json!({"ratingKey": "4", "type": "track", "title": "T",
                       "grandparentTitle": "A",
                       "Media": [{"audioCodec": "mp3", "audioChannels": 2,
                                  "Part": [{"key": "/library/parts/2/1/file.mp3"}]}]});
        let it = track(&session(), &v).unwrap();
        assert_eq!(it["artist"], "A");
        assert_eq!(it["format"], json!({"codec": "mp3", "channels": 2}));
        assert!(it.get("art").is_none());
        assert!(track(&session(), &json!({"ratingKey": "1", "type": "movie"})).is_none());
        let none = track(&session(), &json!({"ratingKey": "5", "type": "track"})).unwrap();
        assert_eq!(none["playable"], false);
    }

    #[test]
    fn album_artist_playlist() {
        let s = session();
        let a = album(
            &s,
            &json!({"ratingKey": "2", "type": "album", "title": "Sessions",
                    "parentTitle": "Ensemble", "year": 2021, "leafCount": 9}),
        )
        .unwrap();
        assert_eq!(a["subtitle"], "Ensemble · 2021");
        assert_eq!(a["track_count"], 9);
        assert_eq!(a["browsable"], true);
        let r = artist(
            &s,
            &json!({"ratingKey": "1", "type": "artist", "title": "Ensemble", "childCount": 2}),
        )
        .unwrap();
        assert_eq!(r["ref"], "r/1");
        assert_eq!(r["subtitle"], "2 ◫");
        let p = playlist(
            &s,
            &json!({"ratingKey": "12", "type": "playlist", "playlistType": "audio",
                    "title": "Mix", "leafCount": 12, "composite": "/playlists/12/composite/1"}),
        )
        .unwrap();
        assert_eq!(p["subtitle"], "12 ♪");
        assert_eq!(p["track_count"], 12);
        assert!(p["art"].is_string());
        let video = json!({"ratingKey": "13", "type": "playlist", "playlistType": "video"});
        assert!(playlist(&s, &video).is_none());
        assert_eq!(
            any(&s, &json!({"ratingKey": "2", "type": "album"})).unwrap()["kind"],
            "album"
        );
    }

    #[test]
    fn plans() {
        let usb = dac(&[44_100, 48_000, 88_200, 96_000], 24);
        assert_eq!(plan(&usb, Some(96_000), Some(24)), Plan::Direct);
        assert_eq!(plan(&usb, Some(44_100), None), Plan::Direct);
        assert_eq!(plan(&usb, None, None), Plan::Direct);
        assert_eq!(
            plan(&usb, Some(176_400), Some(24)),
            Plan::Flac { rate: 88_200 }
        );
        assert_eq!(plan(&usb, Some(192_000), Some(32)), Plan::Unfit);
        let cd = dac(&[44_100], 16);
        assert_eq!(
            plan(&cd, Some(48_000), Some(16)),
            Plan::Flac { rate: 44_100 }
        );
        assert_eq!(plan(&cd, Some(48_000), Some(24)), Plan::Unfit);
        // The null sink, a PipeWire default: the engine converts.
        assert_eq!(
            plan(&Output::default(), Some(384_000), Some(32)),
            Plan::Direct
        );
    }
}
