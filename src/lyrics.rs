//! Lyrics of a track: the lyric streams Plex lists on its file (sidecar
//! `.lrc` / `.txt` files, embedded tags, or the server's own provider),
//! turned into `lyrics.get` answers.

use serde_json::{Value, json};

/// The lyric streams (`streamType` 4) of a detailed track, timed ones
/// first.
pub fn streams(track: &Value) -> Vec<&Value> {
    let mut out: Vec<&Value> = track["Media"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .flat_map(|m| m["Part"].as_array().map(Vec::as_slice).unwrap_or_default())
        .flat_map(|p| {
            p["Stream"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
        })
        .filter(|s| s["streamType"] == 4 && s["key"].is_string())
        .collect();
    out.sort_by_key(|s| !(s["timed"] == true || s["timed"] == "1" || s["format"] == "lrc"));
    out
}

/// A lyric stream as the server sends it: its JSON form (lines with
/// their start in milliseconds), or the file itself (LRC or plain text).
/// `None` when it holds no words.
pub fn answer(body: &str) -> Option<Value> {
    let body = body.trim_start_matches('\u{feff}');
    if body.trim_start().starts_with('{') {
        let v: Value = serde_json::from_str(body).ok()?;
        return from_json(&v["MediaContainer"]["Lyrics"][0]);
    }
    if body.trim_start().starts_with('<') {
        return None; // XML: asked for JSON, never expected
    }
    parse_lrc(body)
}

/// `Lyrics` of the server's JSON: `Line`s with `startOffset` (ms) and
/// their text in `Span`s.
fn from_json(lyrics: &Value) -> Option<Value> {
    let lines = lyrics["Line"].as_array()?;
    let mut synced = Vec::new();
    let mut plain = Vec::new();
    for l in lines {
        let text = match l["Span"].as_array() {
            Some(spans) => spans
                .iter()
                .filter_map(|s| s["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
            None => l["text"].as_str().unwrap_or("").to_string(),
        };
        let text = text.trim().to_string();
        if let Some(t) = l["startOffset"].as_u64() {
            synced.push(json!({"time_ms": t, "text": text}));
        }
        plain.push(text);
    }
    finish(synced, plain)
}

/// `[mm:ss.xx]` (or `[mm:ss]`, `[mm:ss.xxx]`, `[mm:ss:xx]`) → milliseconds.
fn timestamp(tag: &str) -> Option<u64> {
    let (min, rest) = tag.split_once(':')?;
    let (sec, frac) = match rest.split_once(['.', ':']) {
        Some((s, f)) => (s, f),
        None => (rest, ""),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(min) || !digits(sec) || !(frac.is_empty() || digits(frac)) {
        return None;
    }
    let frac_ms = match frac.len() {
        0 => 0,
        1 => frac.parse::<u64>().ok()? * 100,
        2 => frac.parse::<u64>().ok()? * 10,
        _ => frac[..3].parse::<u64>().ok()?,
    };
    Some(min.parse::<u64>().ok()? * 60_000 + sec.parse::<u64>().ok()? * 1000 + frac_ms)
}

/// Words of an enhanced LRC line without their `<mm:ss.xx>` word times.
fn strip_word_times(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('<') {
        match rest[start..].find('>') {
            Some(end) if timestamp(&rest[start + 1..start + end]).is_some() => {
                out.push_str(&rest[..start]);
                rest = &rest[start + end + 1..];
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// An LRC file: each line starts with one or more `[mm:ss.xx]` times;
/// `[ar:…]`-style tags are left out, and `[offset:±ms]` moves every line
/// (a positive offset shows them earlier). A file without times is plain
/// text.
pub fn parse_lrc(text: &str) -> Option<Value> {
    let mut offset: i64 = 0;
    let mut timed: Vec<(u64, String)> = Vec::new();
    let mut plain = Vec::new();
    for line in text.lines() {
        let mut rest = line.trim();
        let mut times = Vec::new();
        let mut tag = false;
        while let Some(inner) = rest.strip_prefix('[') {
            let Some(end) = inner.find(']') else { break };
            let t = inner[..end].trim();
            if let Some(ms) = timestamp(t) {
                times.push(ms);
            } else {
                // `[ar:…]`, `[offset:…]`: letters, a colon, a value.
                let Some((k, v)) = t
                    .split_once(':')
                    .filter(|(k, _)| !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphabetic()))
                else {
                    break;
                };
                if k.eq_ignore_ascii_case("offset") {
                    offset = v.trim().trim_start_matches('+').parse().unwrap_or(0);
                }
                tag = true;
            }
            rest = inner[end + 1..].trim_start();
        }
        if tag && times.is_empty() {
            continue;
        }
        let words = strip_word_times(rest);
        for t in &times {
            timed.push((*t, words.clone()));
        }
        if times.is_empty() || !plain.last().is_some_and(|l: &String| *l == words) {
            plain.push(words);
        }
    }
    // Stable: lines at the same time keep the file's order.
    timed.sort_by_key(|(t, _)| *t);
    let synced: Vec<Value> = timed
        .into_iter()
        .map(|(t, text)| {
            let t = (t as i64 - offset).max(0);
            json!({"time_ms": t, "text": text})
        })
        .collect();
    let plain = if synced.is_empty() {
        plain
    } else {
        // In the order they are sung.
        synced
            .iter()
            .map(|l| l["text"].as_str().unwrap_or("").to_string())
            .collect()
    };
    finish(synced, plain)
}

fn finish(synced: Vec<Value>, plain: Vec<String>) -> Option<Value> {
    let plain = plain.join("\n").trim().to_string();
    let has_words = synced
        .iter()
        .any(|l| l["text"].as_str().is_some_and(|t| !t.is_empty()));
    let mut out = json!({});
    if has_words {
        out["synced"] = synced.into();
    }
    if !plain.is_empty() {
        out["plain"] = plain.into();
    }
    (out.as_object().is_some_and(|o| !o.is_empty())).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(v: &Value) -> Vec<(u64, &str)> {
        v["synced"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| (l["time_ms"].as_u64().unwrap(), l["text"].as_str().unwrap()))
            .collect()
    }

    #[test]
    fn lrc() {
        let v = parse_lrc(
            "\u{feff}[ar:Ensemble]\n[ti:Coda]\n[length: 03:20]\n\
             [00:01.00]First line\n[00:05.50][01:12.00] Twice \n\
             [00:09.253]Third <00:09.50>word <00:10.00>by word\n[00:11]\n",
        )
        .unwrap();
        assert_eq!(
            times(&v),
            [
                (1000, "First line"),
                (5500, "Twice"),
                (9253, "Third word by word"),
                (11_000, ""),
                (72_000, "Twice")
            ]
        );
        assert_eq!(v["plain"], "First line\nTwice\nThird word by word\n\nTwice");
    }

    #[test]
    fn lrc_offset() {
        let v = parse_lrc("[offset:+500]\n[00:00.20]a\n[00:02.00]b\n").unwrap();
        assert_eq!(times(&v), [(0, "a"), (1500, "b")]);
        let v = parse_lrc("[offset:-250]\n[00:02:00]b\n").unwrap();
        assert_eq!(times(&v), [(2250, "b")]);
    }

    #[test]
    fn plain_text() {
        let v = parse_lrc("Plain words\r\n[not a tag] kept\r\nlast\r\n").unwrap();
        assert!(v.get("synced").is_none());
        assert_eq!(v["plain"], "Plain words\n[not a tag] kept\nlast");
        assert_eq!(parse_lrc("[ar:x]\n\n[ti:y]\n"), None);
        assert_eq!(parse_lrc("[00:01.00]\n[00:02.00]  \n"), None);
    }

    #[test]
    fn server_json() {
        let body = r#"{"MediaContainer":{"Lyrics":[{"timed":true,"Line":[
            {"startOffset":1200,"Span":[{"text":"Hello "},{"text":"there"}]},
            {"startOffset":3400,"Span":[{"text":"again"}]}]}]}}"#;
        let v = answer(body).unwrap();
        assert_eq!(times(&v), [(1200, "Hello there"), (3400, "again")]);
        let v = answer(r#"{"MediaContainer":{"Lyrics":[{"Line":[{"Span":[{"text":"a"}]}]}]}}"#);
        assert_eq!(v, Some(json!({"plain": "a"})));
        assert_eq!(answer("<MediaContainer/>"), None);
        assert_eq!(answer("[00:01.00]x").unwrap()["synced"][0]["time_ms"], 1000);
    }

    #[test]
    fn stream_order() {
        let t = json!({"Media": [{"Part": [{"Stream": [
            {"streamType": 2, "key": "/audio"},
            {"streamType": 4, "key": "/library/streams/8", "format": "txt"},
            {"streamType": 4, "key": "/library/streams/9", "format": "lrc", "timed": "1"},
            {"streamType": 4}
        ]}]}]});
        let keys: Vec<&str> = streams(&t)
            .iter()
            .map(|s| s["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["/library/streams/9", "/library/streams/8"]);
        assert!(streams(&json!({})).is_empty());
    }
}
