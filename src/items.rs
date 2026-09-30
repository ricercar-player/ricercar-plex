//! Plex metadata → ricercar items, and whether the DAC takes a file.
//!
//! Refs are `<prefix>/<ratingKey>`: `t` track, `a` album, `r` artist, `p`
//! playlist; and lists about an item: `sim` similar artists or albums,
//! `sonic` sonically similar tracks, `radio` a radio. Top-level sections
//! use bare words (`albums`, `artists`…).

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::plex::Session;

/// Labels in French (the host's `locale`), for item actions.
static FRENCH: AtomicBool = AtomicBool::new(false);

pub fn set_french(on: bool) {
    FRENCH.store(on, Ordering::Relaxed);
}

pub fn french() -> bool {
    FRENCH.load(Ordering::Relaxed)
}

/// Kind prefix and rating key of a ref. Rating keys are numbers.
pub fn split_ref(r: &str) -> Option<(&str, &str)> {
    let (k, id) = r.split_once('/')?;
    let ok = matches!(k, "t" | "a" | "r" | "p" | "sim" | "sonic" | "radio")
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

/// `<prefix>/<key>` from a rating key field (`parentRatingKey`…).
fn key_ref(prefix: &str, v: &Value, k: &str) -> Option<String> {
    let id = match &v[k] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    split_ref(&format!("{prefix}/{id}")).map(|_| format!("{prefix}/{id}"))
}

/// Rated above four and a half stars (9 on Plex's 0-10 scale): what
/// `favorites.set` sets and the Favourites section lists. Unrated items
/// have no `userRating`.
fn favorite(v: &Value) -> bool {
    v["userRating"].as_f64().is_some_and(|r| r > 9.0)
}

/// Lists about an item offered in its menu: similar artists or albums,
/// sonically similar tracks, an artist radio.
pub fn actions(kind: &str, id: &str, fr: bool) -> Value {
    let t = |en: &'static str, f: &'static str| if fr { f } else { en };
    let act = |id_: &str, label: &str, prefix: &str, kind: &str| json!({"id": id_, "label": label, "ref": format!("{prefix}/{id}"), "kind": kind});
    match kind {
        "track" => json!([act(
            "sonic",
            t("Sonically similar tracks", "Titres au son proche"),
            "sonic",
            "play"
        )]),
        "album" => json!([act(
            "similar",
            t("Similar albums", "Albums similaires"),
            "sim",
            "browse"
        )]),
        "artist" => json!([
            act(
                "radio",
                t("Artist radio", "Radio de l'artiste"),
                "radio",
                "play"
            ),
            act(
                "similar",
                t("Similar artists", "Artistes similaires"),
                "sim",
                "browse"
            )
        ]),
        _ => json!([]),
    }
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
    // Listed by a playlist: the entry, for removing and moving it.
    let entry = match &v["playlistItemID"] {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
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
        "album_ref": key_ref("a", v, "parentRatingKey"),
        "artist_ref": key_ref("r", v, "grandparentRatingKey"),
        "favorite": favorite(v),
        "entry_id": entry,
        "actions": actions("track", id, french()),
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
        "artist_ref": key_ref("r", v, "parentRatingKey"),
        "favorite": favorite(v),
        "actions": actions("album", id, french()),
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
        "favorite": favorite(v),
        "actions": actions("artist", id, french()),
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
        "editable": editable(v),
    });
    let thumb = text(v, "thumb").or_else(|| text(v, "composite"));
    Some(finish(s, thumb, it))
}

/// An audio playlist of the user's own making: smart ones follow their
/// rules and cannot be edited by hand.
pub fn editable(v: &Value) -> bool {
    v["type"] == "playlist"
        && v["playlistType"] == "audio"
        && !(v["smart"] == true || v["smart"] == 1 || v["smart"] == "1")
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

/// Text without markup: tags dropped, common entities decoded, blank
/// runs cut to one empty line.
pub fn plain_text(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    // Only `<` followed by a letter, `/` or `!` opens a tag: "a < b" stays.
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let tag = after
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!');
        match after.find('>').filter(|_| tag) {
            Some(end) => {
                let name = after[..end].trim_start_matches('/').to_ascii_lowercase();
                if name.starts_with("br") || name.starts_with('p') {
                    out.push('\n');
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    let out = out
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("\r\n", "\n");
    let mut lines: Vec<&str> = Vec::new();
    for l in out.lines().map(str::trim) {
        if !(l.is_empty() && lines.last().is_none_or(|p| p.is_empty())) {
            lines.push(l);
        }
    }
    lines.join("\n").trim().to_string()
}

/// Tags of a kind (`Genre`, `Style`, `Mood`…) as one line.
fn tags(v: &Value, k: &str) -> Option<String> {
    let t: Vec<&str> = v[k]
        .as_array()?
        .iter()
        .filter_map(|t| t["tag"].as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    (!t.is_empty()).then(|| t.join(", "))
}

/// Facts shown with an artist, album or track: label, release, tags.
pub fn facts(v: &Value, fr: bool) -> Vec<Value> {
    let t = |en: &'static str, f: &'static str| if fr { f } else { en };
    let released = text(v, "originallyAvailableAt")
        .or_else(|| num(v, "year").map(|y| y.to_string()))
        .filter(|_| v["type"] != "artist");
    let list = [
        (t("Label", "Label"), text(v, "studio")),
        (t("Released", "Sortie"), released),
        (t("Genres", "Genres"), tags(v, "Genre")),
        (t("Styles", "Styles"), tags(v, "Style")),
        (t("Moods", "Ambiances"), tags(v, "Mood")),
        (t("Country", "Pays"), tags(v, "Country")),
    ];
    list.into_iter()
        .filter_map(|(label, value)| Some(json!({"label": label, "value": value?})))
        .collect()
}

/// Shelves from the server's hubs about an item: the music items of each
/// hub that has some, except hubs listed in `skip` (by `hubIdentifier`).
pub fn shelves(s: &Session, hubs: &Value, skip: &[&str]) -> Vec<Value> {
    hubs["Hub"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|h| !skip.iter().any(|k| h["hubIdentifier"] == *k))
        .filter_map(|h| {
            let title = text(h, "title")?;
            let list = many(
                s,
                h["Metadata"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
                any,
            );
            (!list.is_empty()).then(|| json!({"title": title, "items": list}))
        })
        .collect()
}

/// `playlists.move` asks for an index, Plex for the entry to follow.
/// With `entries` the playlist in order: the entry that precedes `entry`
/// once it sits at index `to` (clamped to the end); `Some(None)` for the
/// top, `None` when `entry` is not in the playlist.
pub fn move_after(entries: &[String], entry: &str, to: usize) -> Option<Option<String>> {
    let from = entries.iter().position(|e| e == entry)?;
    let mut rest: Vec<&String> = entries.iter().collect();
    rest.remove(from);
    let to = to.min(rest.len());
    Some(to.checked_sub(1).map(|i| rest[i].clone()))
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
        assert_eq!(split_ref("sim/5"), Some(("sim", "5")));
        assert_eq!(split_ref("radio/x"), None);
    }

    #[test]
    fn links_and_state() {
        let s = session();
        // A track listed by a playlist, rated five stars.
        let t = track(
            &s,
            &json!({"ratingKey": "3", "type": "track", "title": "Coda",
                    "parentRatingKey": "2", "grandparentRatingKey": "1",
                    "userRating": 10.0, "playlistItemID": 41}),
        )
        .unwrap();
        assert_eq!(t["album_ref"], "a/2");
        assert_eq!(t["artist_ref"], "r/1");
        assert_eq!(t["favorite"], true);
        assert_eq!(t["entry_id"], "41");
        assert_eq!(
            t["actions"],
            json!([{"id": "sonic", "label": "Sonically similar tracks",
                    "ref": "sonic/3", "kind": "play"}])
        );
        let t = track(
            &s,
            &json!({"ratingKey": "4", "type": "track", "userRating": 6.0,
                    "parentRatingKey": "../x"}),
        )
        .unwrap();
        assert_eq!(t["favorite"], false);
        assert!(t.get("album_ref").is_none() && t.get("entry_id").is_none());
        let a = album(
            &s,
            &json!({"ratingKey": "2", "type": "album", "parentRatingKey": "1"}),
        )
        .unwrap();
        assert_eq!(a["artist_ref"], "r/1");
        assert_eq!(a["favorite"], false);
        assert_eq!(a["actions"][0]["ref"], "sim/2");
        assert_eq!(a["actions"][0]["kind"], "browse");
        let fr = actions("artist", "1", true);
        assert_eq!(fr[0]["label"], "Radio de l'artiste");
        assert_eq!(fr[0]["ref"], "radio/1");
        assert_eq!(fr[1]["label"], "Artistes similaires");
        let pl = |smart: Value| {
            playlist(
                &s,
                &json!({"ratingKey": "12", "type": "playlist", "playlistType": "audio",
                        "smart": smart}),
            )
            .unwrap()
        };
        assert_eq!(pl(json!(false))["editable"], true);
        assert_eq!(pl(json!(true))["editable"], false);
        assert_eq!(pl(json!("1"))["editable"], false);
        assert!(pl(json!(false)).get("favorite").is_none());
    }

    #[test]
    fn details_text() {
        assert_eq!(
            plain_text("  First &amp; <i>best</i>.<br/>Next line<p>New\r\n\r\n\r\npara</p> 1 < 2 "),
            "First & best.\nNext line\nNew\n\npara\n1 < 2"
        );
        let v = json!({"type": "album", "studio": "Blue Label",
                       "originallyAvailableAt": "2021-03-05", "year": 2021,
                       "Genre": [{"tag": "Jazz"}, {"tag": "Soul"}], "Mood": [{"tag": " "}]});
        assert_eq!(
            facts(&v, false),
            [
                json!({"label": "Label", "value": "Blue Label"}),
                json!({"label": "Released", "value": "2021-03-05"}),
                json!({"label": "Genres", "value": "Jazz, Soul"})
            ]
        );
        let v = json!({"type": "artist", "year": 1999, "Country": [{"tag": "France"}]});
        assert_eq!(
            facts(&v, true),
            [json!({"label": "Pays", "value": "France"})]
        );
    }

    #[test]
    fn hub_shelves() {
        let hubs = json!({"Hub": [
            {"hubIdentifier": "artist.mostpopulartracks", "title": "Most Popular Tracks",
             "Metadata": [{"ratingKey": "3", "type": "track", "title": "Coda"},
                          {"ratingKey": "9", "type": "clip", "title": "Video"}]},
            {"hubIdentifier": "artist.albums", "title": "Albums",
             "Metadata": [{"ratingKey": "2", "type": "album"}]},
            {"hubIdentifier": "artist.mostplayedtracks", "title": "Most Played", "size": 0},
            {"hubIdentifier": "music.videos", "title": "Videos",
             "Metadata": [{"ratingKey": "9", "type": "clip"}]}
        ]});
        let sh = shelves(&session(), &hubs, &["artist.albums"]);
        assert_eq!(sh.len(), 1);
        assert_eq!(sh[0]["title"], "Most Popular Tracks");
        assert_eq!(sh[0]["items"].as_array().unwrap().len(), 1);
        assert_eq!(sh[0]["items"][0]["ref"], "t/3");
    }

    #[test]
    fn move_index_to_after() {
        let e: Vec<String> = ["1", "2", "3", "4"].map(String::from).to_vec();
        let after = |entry: &str, to| move_after(&e, entry, to);
        assert_eq!(after("3", 0), Some(None));
        assert_eq!(after("1", 1), Some(Some("2".into())));
        assert_eq!(after("1", 3), Some(Some("4".into())));
        assert_eq!(after("1", 99), Some(Some("4".into())));
        assert_eq!(after("4", 1), Some(Some("1".into())));
        assert_eq!(after("2", 1), Some(Some("1".into())));
        assert_eq!(after("9", 0), None);
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
