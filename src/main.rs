//! Plex Media Server source plugin for ricercar (plugin protocol 1).
//!
//! Speaks JSON-RPC over stdin/stdout with the player, and the documented
//! Plex Media Server API with the user's server. Sign-in goes through
//! plex.tv's PIN flow in the browser. Tracks play from the original file,
//! bit for bit, unless the DAC cannot take its sample rate; then the server
//! sends FLAC at a rate it does take. Also: lyrics, playlist editing,
//! details and similar items, and a radio, from what the server knows.
//!
//! Options:
//!   --server NAME|URL   the server to use when the account has several, or
//!                       an address to reach it at (instead of the ones
//!                       plex.tv knows)

mod items;
mod lyrics;
mod plex;

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use items::{Output, Plan};
use plex::{Client, Error, LIBRARY, Session};

const PROTOCOL: u64 = 1;
const PAGE: u64 = 200;
/// Favourites are read whole, per kind, up to this many.
const FAVORITES_MAX: u64 = 1000;
/// How often the plugin asks plex.tv whether the PIN was claimed.
const PIN_POLL: Duration = Duration::from_secs(2);
/// Strong PINs live 30 minutes.
const PIN_LIFE: Duration = Duration::from_secs(30 * 60);
/// Tracks of a radio action, and most asked for by `radio.next`.
const RADIO_MAX: usize = 50;
/// Entries of a playlist read to move one of them.
const PLAYLIST_MAX: u64 = 20_000;

/// `<n>` random bytes from the kernel, as hex.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

struct RpcError {
    code: i64,
    message: String,
}

fn rpc_err(code: i64, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}

type Reply = Result<Value, RpcError>;

struct Out(Mutex<std::io::Stdout>);

impl Out {
    fn send(&self, v: Value) {
        let mut out = self.0.lock().unwrap();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }
}

/// The plugin's settings, from `initialize` and `settings.changed`;
/// missing or unexpected values keep their default.
#[derive(Clone, Debug, PartialEq)]
struct Settings {
    /// Send timelines and scrobbles to the server.
    report: bool,
    /// Ask the server for FLAC at a rate the DAC takes when it cannot take
    /// the file's; `false`: always the original file.
    transcode: bool,
}

impl Settings {
    fn from_json(v: &Value) -> Settings {
        Settings {
            report: v["report_playback"].as_bool().unwrap_or(true),
            transcode: v["transcode"].as_str() != Some("never"),
        }
    }

    /// The declaration sent with `initialize`.
    fn declare(fr: bool) -> Value {
        let t = |en: &'static str, f: &'static str| if fr { f } else { en };
        let section = t("Playback", "Lecture");
        json!([
            {"key": "report_playback", "type": "bool", "section": section,
             "label": t("Report what I play", "Signaler mes écoutes"),
             "description": t(
                "Show the track on the server while it plays (its “now playing” and resume point), and count it in its play history once heard.",
                "Afficher la piste sur le serveur pendant la lecture (« en cours » et point de reprise), et la compter dans son historique une fois écoutée."),
             "default": true},
            {"key": "transcode", "type": "choice", "section": section,
             "label": t("When the DAC cannot take a file's sample rate",
                        "Quand le DAC n'accepte pas la fréquence d'un fichier"),
             "description": t(
                "The server can convert the file to FLAC at the closest rate the DAC takes, keeping its bit depth. With the original file, nothing is converted, and a bit-perfect output cannot play such tracks.",
                "Le serveur peut convertir le fichier en FLAC à la fréquence la plus proche acceptée par le DAC, sans changer sa résolution. Avec le fichier d'origine, rien n'est converti, et une sortie bit-perfect ne peut pas lire ces pistes."),
             "options": [
                {"value": "auto", "label": t("Ask the server for FLAC at a rate it takes",
                                             "Demander au serveur du FLAC à une fréquence acceptée")},
                {"value": "never", "label": t("Always send the original file",
                                              "Toujours envoyer le fichier d'origine")}
             ],
             "default": "auto"}
        ])
    }
}

/// A track being played, for the timeline and the scrobble.
struct Playing {
    duration_ms: u64,
    session_id: String,
}

struct Plugin {
    out: Arc<Out>,
    /// `--server`: a server name, or an address.
    server_hint: String,
    data_dir: Mutex<PathBuf>,
    settings: Mutex<Settings>,
    output: Mutex<Output>,
    client: Arc<Client>,
    session: Mutex<Option<Session>>,
    /// The token was refused: signed in, but it needs renewing.
    expired: Mutex<bool>,
    /// The sign-in in progress: its generation (a newer `auth.begin`
    /// stops older polls), the browser address and when it started.
    pin: Mutex<(u64, Option<(String, Instant)>)>,
    /// Why the last sign-in did not end with a usable server.
    login_error: Mutex<Option<String>>,
    playing: Mutex<HashMap<String, Playing>>,
}

impl Plugin {
    fn session(&self) -> Result<Session, RpcError> {
        let s = self.session.lock().unwrap().clone();
        match s {
            Some(s) if !*self.expired.lock().unwrap() => Ok(s),
            _ => Err(rpc_err(-32001, "sign in to Plex first")),
        }
    }

    fn fr(&self) -> bool {
        items::french()
    }

    fn apply_settings(&self, v: &Value) {
        let new = Settings::from_json(v);
        if !new.report {
            self.playing.lock().unwrap().clear();
        }
        *self.settings.lock().unwrap() = new;
    }

    fn auth_path(&self) -> PathBuf {
        self.data_dir.lock().unwrap().join("auth.json")
    }

    fn auth_status(&self) -> Value {
        match &*self.session.lock().unwrap() {
            None => json!({"state": "signed_out"}),
            Some(s) => {
                let state = if *self.expired.lock().unwrap() {
                    "expired"
                } else {
                    "signed_in"
                };
                let detail = if s.server_name.is_empty() {
                    s.server.clone()
                } else {
                    format!("{} · {}", s.server_name, s.server)
                };
                let name = if s.user.is_empty() {
                    &s.server_name
                } else {
                    &s.user
                };
                json!({"state": state, "account": {"display_name": name, "detail": detail}})
            }
        }
    }

    fn save(&self, s: &Session) {
        let path = self.auth_path();
        if let Err(e) = write_private(&path, &s.to_json().to_string()) {
            eprintln!("cannot save the session in {}: {e}", path.display());
        }
    }

    fn store(&self, s: Session) {
        self.save(&s);
        // Never the account's name or email: logs get pasted in bug reports.
        eprintln!("signed in to {} ({})", s.server_name, s.server);
        *self.session.lock().unwrap() = Some(s);
        *self.expired.lock().unwrap() = false;
        *self.login_error.lock().unwrap() = None;
        self.out.notify("auth.changed", self.auth_status());
    }

    /// Map a server failure; a refused token marks the session expired.
    fn fail(&self, e: Error) -> RpcError {
        match e {
            Error::Auth => {
                let was = std::mem::replace(&mut *self.expired.lock().unwrap(), true);
                if !was {
                    eprintln!("the server refused the token");
                    self.out.notify("auth.changed", self.auth_status());
                }
                rpc_err(-32001, "the server refused the stored token")
            }
            Error::NotFound => rpc_err(-32002, "not found on the server"),
            Error::Status(code @ (429 | 503), m) => {
                rpc_err(-32004, format!("server answered {code} {m}"))
            }
            Error::Status(code, m) if code >= 500 => {
                rpc_err(-32005, format!("server error {code} {m}"))
            }
            Error::Status(code, m) => rpc_err(-32603, format!("server answered {code} {m}")),
            Error::Network(m) => rpc_err(-32005, m),
        }
    }

    /// Run `f` on the session; when the server stops answering at its
    /// address, look for another one plex.tv gave (home / away) and retry.
    fn with<T>(&self, f: impl Fn(&Session) -> plex::Result<T>) -> Result<T, RpcError> {
        let s = self.session()?;
        match f(&s) {
            Err(Error::Network(e)) => match self.reconnect(&s) {
                Some(s) => f(&s).map_err(|e| self.fail(e)),
                None => Err(self.fail(Error::Network(e))),
            },
            r => r.map_err(|e| self.fail(e)),
        }
    }

    /// Another address of the same server that answers, made current.
    fn reconnect(&self, s: &Session) -> Option<Session> {
        let uri = s.connections.iter().filter(|u| **u != s.server).find(|u| {
            self.client
                .identity(u, &s.token)
                .is_ok_and(|id| s.machine_id.is_empty() || id == s.machine_id)
        })?;
        eprintln!("{} does not answer, now using {uri}", s.server);
        let mut s = s.clone();
        s.server = uri.clone();
        self.save(&s);
        *self.session.lock().unwrap() = Some(s.clone());
        Some(s)
    }

    fn get(&self, path: &str, q: &[(&str, String)]) -> Result<(Session, Value), RpcError> {
        self.with(|s| Ok((s.clone(), self.client.get(s, path, q)?)))
    }

    /// Like `get`, for lists a server may lack (older versions, features
    /// of Plex Pass): "not found" and "forbidden" are an empty answer.
    fn get_optional(&self, path: &str, q: &[(&str, String)]) -> Result<(Session, Value), RpcError> {
        self.with(|s| match self.client.get(s, path, q) {
            Err(Error::NotFound | Error::Status(403, _)) => Ok((s.clone(), Value::Null)),
            r => Ok((s.clone(), r?)),
        })
    }

    fn page(
        &self,
        path: &str,
        q: &[(&str, String)],
        offset: u64,
        limit: u64,
    ) -> Result<(Session, plex::Page), RpcError> {
        self.with(|s| Ok((s.clone(), self.client.page(s, path, q, offset, limit)?)))
    }

    // ---------------------------------------------------------------- setup

    fn initialize(&self, p: &Value) -> Reply {
        let data_dir = p["data_dir"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&data_dir);
        items::set_french(p["locale"].as_str().is_some_and(|l| l.starts_with("fr")));
        *self.output.lock().unwrap() = Output::from_json(&p["output"]);
        self.apply_settings(&p["settings"]);
        // Plex lists each client identifier among the account's devices:
        // keep one per installation.
        let id_path = data_dir.join("client_id");
        let id = std::fs::read_to_string(&id_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let id = format!("ricercar-{}", random_hex(12));
                let _ = std::fs::write(&id_path, &id);
                id
            });
        *self.client.client_id.lock().unwrap() = id;
        *self.data_dir.lock().unwrap() = data_dir;
        *self.session.lock().unwrap() = std::fs::read_to_string(self.auth_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .and_then(|v| Session::from_json(&v));

        let proto = p["protocol"].as_u64().unwrap_or(0);
        if proto != PROTOCOL {
            eprintln!("host speaks protocol {proto}, this plugin {PROTOCOL}");
        }
        Ok(json!({
            "protocol": PROTOCOL,
            "plugin": {"id": "plex", "name": "Plex", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {
                "auth": true, "browse": true, "search": true, "resolve": true,
                "favorites": true, "reporting": true, "remote_control": false,
                "library": true, "lyrics": true, "playlist_edit": true,
                "details": true, "radio": true
            },
            "settings": Settings::declare(self.fr())
        }))
    }

    // ----------------------------------------------------------------- auth

    /// A new plex.tv PIN, and a thread that waits for the user to claim it
    /// in the browser, then picks the server and signs in.
    fn auth_begin(self: &Arc<Self>) -> Reply {
        let french = self.fr();
        let (id, code) = self.client.pin_create().map_err(|e| {
            rpc_err(
                -32005,
                format!("cannot reach plex.tv to start the sign-in: {e}"),
            )
        })?;
        let url = format!(
            "https://app.plex.tv/auth#?clientID={}&code={}&context%5Bdevice%5D%5Bproduct%5D={}",
            plex::encode(&self.client.id()),
            plex::encode(&code),
            plex::PRODUCT
        );
        let generation = {
            let mut pin = self.pin.lock().unwrap();
            pin.0 += 1;
            pin.1 = Some((url.clone(), Instant::now()));
            pin.0
        };
        *self.login_error.lock().unwrap() = None;
        let me = self.clone();
        std::thread::spawn(move || me.wait_for_pin(generation, id));
        let instructions = if french {
            "Connectez-vous à votre compte Plex dans la page qui s'ouvre. ricercar utilisera votre serveur Plex Media Server. Depuis un autre appareil, collez ici « adresse-du-serveur jeton » (X-Plex-Token)."
        } else {
            "Sign in to your Plex account on the page that opens. ricercar will use your Plex Media Server. From another device, paste “server-address token” (X-Plex-Token) here."
        };
        Ok(json!({"url": url, "instructions": instructions, "expects_input": false}))
    }

    fn wait_for_pin(&self, generation: u64, id: u64) {
        let started = Instant::now();
        while started.elapsed() < PIN_LIFE {
            std::thread::sleep(PIN_POLL);
            if self.pin.lock().unwrap().0 != generation {
                return; // a newer sign-in took over
            }
            match self.client.pin_check(id) {
                Ok(None) => {}
                Ok(Some(token)) => {
                    if let Err(e) = self.connect(&token) {
                        eprintln!("signed in to plex.tv, but: {e}");
                        *self.login_error.lock().unwrap() = Some(e);
                    }
                    self.pin.lock().unwrap().1 = None;
                    return;
                }
                Err(Error::NotFound) => break,
                Err(e) => eprintln!("plex.tv: {e}"),
            }
        }
        eprintln!("the sign-in PIN expired");
        self.pin.lock().unwrap().1 = None;
    }

    /// With an account token: find the server to use and reach it.
    fn connect(&self, account_token: &str) -> Result<(), String> {
        let user = self.client.user(account_token).unwrap_or_default();
        let hint = self.server_hint.trim();
        // `--server <address>`: use it with the account token.
        if let Some(addr) = plex::normalize_server(hint).filter(|_| hint.contains(['.', ':'])) {
            let mut s = self
                .client
                .sign_in_direct(&addr, account_token)
                .map_err(|e| format!("{addr}: {e}"))?;
            s.user = user;
            self.store(s);
            return Ok(());
        }
        let servers = self
            .client
            .resources(account_token)
            .map_err(|e| format!("cannot list your servers on plex.tv: {e}"))?;
        let names: Vec<&str> = servers.iter().map(|r| r.name.as_str()).collect();
        eprintln!("servers of the account: {names:?}");
        let chosen: Vec<&plex::Resource> = if hint.is_empty() {
            servers.iter().collect()
        } else {
            servers
                .iter()
                .filter(|r| r.name.eq_ignore_ascii_case(hint) || r.machine_id == hint)
                .collect()
        };
        if chosen.is_empty() {
            return Err(if hint.is_empty() {
                "no Plex Media Server on this account".to_string()
            } else {
                format!("no server named “{hint}” on this account (found: {names:?})")
            });
        }
        let mut errors = Vec::new();
        for r in chosen {
            match self.client.reach(r) {
                Ok(uri) => {
                    self.store(Session {
                        server: uri,
                        connections: r.connections.clone(),
                        server_name: r.name.clone(),
                        machine_id: r.machine_id.clone(),
                        token: r.token.clone(),
                        user: user.clone(),
                    });
                    return Ok(());
                }
                Err(e) => errors.push(format!("{}: {e}", r.name)),
            }
        }
        Err(format!("no server answers ({})", errors.join("; ")))
    }

    /// The paste field: `<server address> <token>`, or nothing (the browser
    /// sign-in may have finished, or be about to).
    fn auth_complete(&self, p: &Value) -> Reply {
        let input = p["input"].as_str().unwrap_or("").trim();
        let mut words = input.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some(addr), Some(token), None) => {
                let server = plex::normalize_server(addr)
                    .ok_or_else(|| rpc_err(-32602, "expected “server-address token”"))?;
                match self.client.sign_in_direct(&server, token) {
                    Ok(s) => self.store(s),
                    Err(Error::Auth) => {}
                    Err(e) => return Err(self.fail(e)),
                }
            }
            (None, ..) => {
                // Give a PIN claimed a moment ago the time to be seen.
                let waiting = self.pin.lock().unwrap().1.is_some();
                if waiting && self.session.lock().unwrap().is_none() {
                    std::thread::sleep(PIN_POLL + Duration::from_millis(500));
                }
                if let Some(e) = self.login_error.lock().unwrap().clone() {
                    return Err(rpc_err(-32005, e));
                }
            }
            _ => return Err(rpc_err(-32602, "expected “server-address token”")),
        }
        Ok(self.auth_status())
    }

    fn sign_out(&self) -> Reply {
        *self.session.lock().unwrap() = None;
        *self.expired.lock().unwrap() = false;
        self.pin.lock().unwrap().0 += 1;
        let _ = std::fs::remove_file(self.auth_path());
        Ok(Value::Null)
    }

    // --------------------------------------------------------------- browse

    fn root(&self) -> Reply {
        self.session()?;
        let fr = self.fr();
        let t = |en: &'static str, f: &'static str| if fr { f } else { en };
        let sections = [
            ("recent", t("Recently added", "Ajouts récents")),
            ("albums", t("Albums", "Albums")),
            ("artists", t("Artists", "Artistes")),
            ("playlists", t("Playlists", "Listes de lecture")),
            ("favorites", t("Favourites", "Favoris")),
            ("frequent", t("Most played", "Les plus écoutés")),
        ];
        let sections: Vec<Value> = sections
            .iter()
            .map(
                |(r, title)| json!({"ref": r, "kind": "folder", "title": title, "browsable": true}),
            )
            .collect();
        // Shelves of the host's Home page: albums, across every music
        // library of the server (`/library/all` merges and sorts them).
        let home = [
            ("recent", t("Recently added", "Ajouts récents")),
            ("played", t("Recently played", "Écoutés récemment")),
            ("top", t("Most played", "Les plus écoutés")),
        ];
        let home: Vec<Value> = home
            .iter()
            .map(
                |(r, title)| json!({"ref": r, "kind": "folder", "title": title, "browsable": true}),
            )
            .collect();
        Ok(json!({ "sections": sections, "home": home }))
    }

    /// The user's audio playlists (smart ones included).
    fn playlists(&self, offset: u64, limit: u64) -> Reply {
        let q = [("playlistType", "audio".to_string())];
        self.listing("/playlists", &q, offset, limit, items::playlist)
    }

    /// Artists without a picture of their own (no online metadata agent)
    /// get the cover of one of their albums, in one request per page.
    fn artist_art(&self, s: &Session, list: &mut [Value]) {
        let bare: Vec<&str> = list
            .iter()
            .filter(|a| a["type"] == "artist" && !a["thumb"].is_string())
            .filter_map(|a| a["ratingKey"].as_str())
            .collect();
        if bare.is_empty() {
            return;
        }
        let q = [("type", "9".to_string()), ("artist.id", bare.join(","))];
        let albums = match self.client.page(s, "/library/all", &q, 0, 5000) {
            Ok(pg) => pg.items,
            Err(e) => {
                eprintln!("artist covers: {e}");
                return;
            }
        };
        for a in list.iter_mut().filter(|a| !a["thumb"].is_string()) {
            let thumb = albums
                .iter()
                .filter(|al| al["parentRatingKey"] == a["ratingKey"])
                .find_map(|al| al["thumb"].as_str());
            if let Some(t) = thumb {
                a["thumb"] = t.into();
            }
        }
    }

    /// One page of a server list, mapped with `f`.
    fn listing(
        &self,
        path: &str,
        q: &[(&str, String)],
        offset: u64,
        limit: u64,
        f: fn(&Session, &Value) -> Option<Value>,
    ) -> Reply {
        let (s, mut pg) = self.page(path, q, offset, limit)?;
        self.artist_art(&s, &mut pg.items);
        let got = pg.items.len() as u64;
        let list = items::many(&s, &pg.items, f);
        let has_more = match pg.total {
            Some(t) => offset + got < t,
            None => got == limit,
        };
        let mut r = json!({"items": list, "has_more": has_more});
        if let Some(t) = pg.total {
            r["total"] = t.into();
        }
        Ok(r)
    }

    /// Music of every library section: `/library/all` with a type
    /// (8 artists, 9 albums, 10 tracks) and a query.
    fn all(
        &self,
        kind: u8,
        extra: &[(&str, String)],
        offset: u64,
        limit: u64,
        f: fn(&Session, &Value) -> Option<Value>,
    ) -> Reply {
        let mut q = vec![("type", kind.to_string())];
        q.extend(extra.iter().cloned());
        self.listing("/library/all", &q, offset, limit, f)
    }

    /// The whole list of `kind` matching `extra`, up to `max`.
    fn all_of(&self, kind: u8, extra: &[(&str, String)], max: u64) -> Result<Vec<Value>, RpcError> {
        let mut q = vec![("type", kind.to_string())];
        q.extend(extra.iter().cloned());
        let (s, mut pg) = self.page("/library/all", &q, 0, max)?;
        self.artist_art(&s, &mut pg.items);
        Ok(items::many(&s, &pg.items, items::any))
    }

    fn list(&self, p: &Value) -> Reply {
        let r = p["ref"].as_str().unwrap_or("");
        let offset = p["offset"].as_u64().unwrap_or(0);
        let limit = p["limit"].as_u64().unwrap_or(PAGE).clamp(1, PAGE);
        let by_title = || ("sort", "titleSort".to_string());
        match r {
            "recent" => {
                return self.all(
                    9,
                    &[("sort", "addedAt:desc".into())],
                    offset,
                    limit,
                    items::album,
                );
            }
            "albums" => return self.all(9, &[by_title()], offset, limit, items::album),
            "artists" => return self.all(8, &[by_title()], offset, limit, items::artist),
            "played" | "top" => {
                let sort = if r == "played" {
                    "lastViewedAt:desc"
                } else {
                    "viewCount:desc"
                };
                let q = [("viewCount>>", "0".to_string()), ("sort", sort.to_string())];
                return self.all(9, &q, offset, limit, items::album);
            }
            "frequent" => {
                let q = [
                    ("sort", "viewCount:desc".to_string()),
                    ("viewCount>>", "0".to_string()),
                ];
                return self.all(10, &q, offset, limit, items::track);
            }
            "playlists" => return self.playlists(offset, limit),
            "favorites" => {
                // Rated five stars (10 on Plex's 0-10 scale). In Plex
                // queries, `field>>=value` means "greater than".
                let q = [("userRating>>", "9".to_string()), by_title()];
                let mut all = self.all_of(8, &q, FAVORITES_MAX)?;
                all.extend(self.all_of(9, &q, FAVORITES_MAX)?);
                all.extend(self.all_of(10, &q, FAVORITES_MAX)?);
                return Ok(page(all, offset, limit));
            }
            _ => {}
        }
        let (kind, id) = items::split_ref(r).ok_or_else(|| rpc_err(-32002, "no such list"))?;
        match kind {
            "a" => self.listing(
                &format!("/library/metadata/{id}/children"),
                &[],
                offset,
                limit,
                items::track,
            ),
            "r" => {
                let (s, v) = self.get(&format!("/library/metadata/{id}/children"), &[])?;
                let mut albums: Vec<Value> = v["Metadata"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .to_vec();
                // Newest first, like a discography.
                albums.sort_by_key(|a| std::cmp::Reverse(a["year"].as_i64().unwrap_or(0)));
                Ok(page(items::many(&s, &albums, items::album), offset, limit))
            }
            "p" => self.listing(
                &format!("/playlists/{id}/items"),
                &[],
                offset,
                limit,
                items::track,
            ),
            "sim" => Ok(page(self.similar(id)?, offset, limit)),
            "sonic" => Ok(page(self.sonic(id, RADIO_MAX)?, offset, limit)),
            "radio" => Ok(page(self.radio(id, &[], RADIO_MAX)?, offset, limit)),
            _ => Err(rpc_err(-32002, "no such list")),
        }
    }

    fn search(&self, p: &Value) -> Reply {
        let query = p["query"].as_str().unwrap_or("").trim().to_string();
        let offset = p["offset"].as_u64().unwrap_or(0);
        let limit = p["limit"].as_u64().unwrap_or(50).clamp(1, PAGE);
        let wanted: Vec<String> = p["kinds"]
            .as_array()
            .map(|k| {
                k.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_else(|| {
                ["artist", "album", "playlist", "track"]
                    .map(String::from)
                    .to_vec()
            });
        self.session()?;
        if query.is_empty() {
            return Ok(json!({ "groups": [] }));
        }
        let mut groups = Vec::new();
        for kind in ["artist", "album", "playlist", "track"] {
            if !wanted.iter().any(|w| w == kind) {
                continue;
            }
            // `title=` matches a part of the title, whatever the case.
            let q = [("title", query.clone()), ("sort", "titleSort".to_string())];
            let mut g = match kind {
                "artist" => self.all(8, &q, offset, limit, items::artist)?,
                "album" => self.all(9, &q, offset, limit, items::album)?,
                "track" => self.all(10, &q, offset, limit, items::track)?,
                _ => {
                    let (s, pg) =
                        self.page("/playlists", &[("playlistType", "audio".into())], 0, 1000)?;
                    let needle = query.to_lowercase();
                    let all: Vec<Value> = items::many(&s, &pg.items, items::playlist)
                        .into_iter()
                        .filter(|p| {
                            p["title"]
                                .as_str()
                                .is_some_and(|t| t.to_lowercase().contains(&needle))
                        })
                        .collect();
                    page(all, offset, limit)
                }
            };
            g["kind"] = kind.into();
            groups.push(g);
        }
        Ok(json!({ "groups": groups }))
    }

    fn item_get(&self, p: &Value) -> Reply {
        let (kind, id) = items::split_ref(p["ref"].as_str().unwrap_or(""))
            .filter(|(k, _)| matches!(*k, "t" | "a" | "r" | "p"))
            .ok_or_else(|| rpc_err(-32002, "no such item"))?;
        let path = if kind == "p" {
            format!("/playlists/{id}")
        } else {
            format!("/library/metadata/{id}")
        };
        let (s, v) = self.get(&path, &[])?;
        v["Metadata"][0]
            .as_object()
            .and_then(|_| items::any(&s, &v["Metadata"][0]))
            .ok_or_else(|| rpc_err(-32002, "not a music item"))
    }

    fn favorite(&self, p: &Value) -> Reply {
        let (kind, id) = items::split_ref(p["ref"].as_str().unwrap_or(""))
            .filter(|(k, _)| matches!(*k, "t" | "a" | "r" | "p"))
            .ok_or_else(|| rpc_err(-32002, "no such item"))?;
        if kind == "p" {
            return Err(rpc_err(-32003, "playlists cannot be rated"));
        }
        let rating = if p["on"].as_bool().unwrap_or(false) {
            "10"
        } else {
            "-1"
        };
        let q = [
            ("identifier", LIBRARY.to_string()),
            ("key", id.to_string()),
            ("rating", rating.to_string()),
        ];
        self.with(|s| self.client.call(s, "PUT", "/:/rate", &q))
            .map(|_| Value::Null)
    }

    // -------------------------------------------------------------- library

    fn library(&self, method: &str, p: &Value) -> Reply {
        let offset = p["offset"].as_u64().unwrap_or(0);
        let limit = p["limit"].as_u64().unwrap_or(PAGE).clamp(1, PAGE);
        let q = [("sort", "titleSort".to_string())];
        match method {
            "library.albums" => self.all(9, &q, offset, limit, items::album),
            "library.artists" => self.all(8, &q, offset, limit, items::artist),
            "library.playlists" => self.playlists(offset, limit),
            _ => self.all(10, &q, offset, limit, items::track),
        }
    }

    // -------------------------------------------------------------- resolve

    fn resolve(&self, p: &Value) -> Reply {
        let r = p["ref"].as_str().unwrap_or("");
        let Some(("t", id)) = items::split_ref(r) else {
            return Err(rpc_err(-32002, "not a track"));
        };
        let (s, v) = self.get(&format!("/library/metadata/{id}"), &[])?;
        let track = &v["Metadata"][0];
        let media = items::media(track).ok_or_else(|| rpc_err(-32003, "no file for this track"))?;
        let stream = items::audio_stream(media);
        let format = items::format(track).unwrap_or(json!({}));
        let rate = format["sample_rate"].as_u64().map(|r| r as u32);
        let bits = format["bits"].as_u64().map(|b| b as u8);
        let what = match bits {
            Some(b) => format!("{} Hz / {b} bits", rate.unwrap_or(0)),
            None => format!("{} Hz", rate.unwrap_or(0)),
        };
        let plan = match items::plan(&self.output.lock().unwrap(), rate, bits) {
            Plan::Flac { .. } if !self.settings.lock().unwrap().transcode => Plan::Direct,
            plan => plan,
        };
        let (url, format) = match plan {
            Plan::Direct => {
                let key = media["Part"][0]["key"].as_str().unwrap_or("");
                (s.url(key, &[]), format)
            }
            Plan::Unfit => {
                eprintln!("{r}: {what} does not fit this output");
                return Err(rpc_err(
                    -32003,
                    format!("the DAC cannot take {what}, and Plex keeps the bit depth"),
                ));
            }
            Plan::Flac { rate: to } => {
                let url = self.transcode(&s, id, to).map_err(|why| {
                    eprintln!("{r}: {what} does not fit this output, no transcode: {why}");
                    rpc_err(-32003, format!("the DAC cannot take {what}: {why}"))
                })?;
                eprintln!("{r}: {to} Hz transcode for this output");
                let mut f = json!({"sample_rate": to, "codec": "flac"});
                if let Some(b) = bits {
                    f["bits"] = b.into();
                }
                if let Some(c) = format.get("channels") {
                    f["channels"] = c.clone();
                }
                (url, f)
            }
        };
        let mut res = json!({
            "url": url,
            "duration_ms": track["duration"].as_i64(),
            "format": format,
            "live": false,
        });
        if let Some(rg) = stream.and_then(items::replaygain) {
            res["replaygain"] = rg;
        }
        if let Some(o) = res.as_object_mut() {
            o.retain(|_, v| !v.is_null());
        }
        Ok(res)
    }

    /// A FLAC stream of track `id` at `rate` through the universal
    /// transcoder: the decision first (the server only starts sessions it
    /// has decided on), then the stream URL with the same parameters.
    fn transcode(&self, s: &Session, id: &str, rate: u32) -> Result<String, String> {
        let extra = [
            "add-transcode-target(type=musicProfile&context=streaming&protocol=http\
             &container=flac&audioCodec=flac&replace=true)"
                .to_string(),
            format!(
                "add-limitation(scope=musicCodec&scopeName=flac&type=upperBound\
                 &name=audio.samplingRate&value={rate}&isRequired=true)"
            ),
        ]
        .join("+");
        let session_id = random_hex(12);
        let path = format!("/library/metadata/{id}");
        let q: Vec<(&str, String)> = vec![
            ("path", path),
            ("protocol", "http".into()),
            ("mediaIndex", "0".into()),
            ("partIndex", "0".into()),
            ("directPlay", "0".into()),
            ("directStream", "0".into()),
            ("directStreamAudio", "0".into()),
            ("transcodeSessionId", session_id.clone()),
            ("X-Plex-Session-Identifier", session_id),
            ("X-Plex-Client-Profile-Name", "Generic".into()),
            ("X-Plex-Client-Profile-Extra", extra),
        ];
        let d = self
            .client
            .get(s, "/music/:/transcode/universal/decision", &q)
            .map_err(|e| match e {
                e @ (Error::Auth | Error::Network(_)) => self.fail(e).message,
                e => format!("the server cannot transcode: {e}"),
            })?;
        let code = d["generalDecisionCode"].as_u64().unwrap_or(0);
        if !(1000..2000).contains(&code) {
            let why = d["generalDecisionText"]
                .as_str()
                .or(d["transcodeDecisionText"].as_str())
                .unwrap_or("the server declined to transcode");
            return Err(why.to_string());
        }
        let mut url = s.url("/music/:/transcode/universal/start.flac", &[]);
        for (k, v) in &q {
            url.push('&');
            url.push_str(k);
            url.push('=');
            url.push_str(&plex::encode(v));
        }
        url.push_str("&X-Plex-Client-Identifier=");
        url.push_str(&plex::encode(&self.client.id()));
        url.push_str("&X-Plex-Product=");
        url.push_str(plex::PRODUCT);
        Ok(url)
    }

    // --------------------------------------------------------------- lyrics

    /// The first lyric stream of the track with words in it: sidecar
    /// `.lrc` / `.txt` files, embedded tags, or the server's provider.
    fn lyrics(&self, p: &Value) -> Reply {
        let Some(("t", id)) = items::split_ref(p["ref"].as_str().unwrap_or("")) else {
            return Err(rpc_err(-32002, "not a track"));
        };
        let (_, v) = self.get(&format!("/library/metadata/{id}"), &[])?;
        for st in lyrics::streams(&v["Metadata"][0]) {
            let key = st["key"].as_str().unwrap_or("");
            match self.with(|s| self.client.text(s, key)) {
                Ok(body) => {
                    if let Some(l) = lyrics::answer(&body) {
                        return Ok(l);
                    }
                }
                Err(e) => eprintln!("lyrics of t/{id}: {}", e.message),
            }
        }
        Err(rpc_err(-32002, "no lyrics for this track"))
    }

    // -------------------------------------------------------------- details

    /// Artists or albums like `id`, as raw metadata: the server's similar
    /// items, else those of its related hubs (albums have no `similar`
    /// list).
    fn similar_raw(&self, id: &str) -> Result<(Session, Vec<Value>), RpcError> {
        let (s, v) = self.get_optional(&format!("/library/metadata/{id}/similar"), &[])?;
        let list = v["Metadata"].as_array().cloned().unwrap_or_default();
        if !list.is_empty() {
            return Ok((s, list));
        }
        let q = [("count", "20".to_string())];
        let (s, hubs) = self.get_optional(&format!("/hubs/metadata/{id}/related"), &q)?;
        let mut seen = HashSet::new();
        let list = hubs["Hub"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .flat_map(|h| {
                h["Metadata"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
            })
            .filter(|m| m["type"] == "album" || m["type"] == "artist")
            .filter(|m| seen.insert(m["ratingKey"].to_string()))
            .cloned()
            .collect();
        Ok((s, list))
    }

    fn similar(&self, id: &str) -> Result<Vec<Value>, RpcError> {
        let (s, mut list) = self.similar_raw(id)?;
        self.artist_art(&s, &mut list);
        Ok(items::many(&s, &list, items::any))
    }

    /// Tracks that sound like track `id` (sonic analysis: Plex Pass, and
    /// a server that ran it); none otherwise.
    fn sonic(&self, id: &str, limit: usize) -> Result<Vec<Value>, RpcError> {
        let q = [("limit", limit.to_string())];
        let (s, v) = self.get_optional(&format!("/library/metadata/{id}/nearest"), &q)?;
        let list = v["Metadata"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        Ok(items::many(&s, list, items::track))
    }

    /// Biography (the server's summary), shelves from its hubs, and facts.
    fn details(&self, p: &Value) -> Reply {
        let (kind, id) = items::split_ref(p["ref"].as_str().unwrap_or(""))
            .filter(|(k, _)| matches!(*k, "t" | "a" | "r" | "p"))
            .ok_or_else(|| rpc_err(-32002, "no such item"))?;
        let path = if kind == "p" {
            format!("/playlists/{id}")
        } else {
            format!("/library/metadata/{id}")
        };
        let (_, v) = self.get(&path, &[])?;
        let m = &v["Metadata"][0];
        if !m.is_object() {
            return Err(rpc_err(-32002, "no such item"));
        }
        let fr = self.fr();
        let mut out = json!({});
        let bio = items::plain_text(m["summary"].as_str().unwrap_or(""));
        if !bio.is_empty() {
            out["biography"] = json!({"text": bio, "source": "Plex"});
        }
        // An artist's hubs are on its own key, an album's or a track's
        // under `related`.
        let hubs = match kind {
            "r" => format!("/hubs/metadata/{id}"),
            "a" | "t" => format!("/hubs/metadata/{id}/related"),
            _ => String::new(),
        };
        let mut related = Vec::new();
        if !hubs.is_empty() {
            match self.get_optional(&hubs, &[("count", "20".to_string())]) {
                // The artist's albums are its page already.
                Ok((s, h)) => related = items::shelves(&s, &h, &["artist.albums"]),
                Err(e) => eprintln!("hubs of {kind}/{id}: {}", e.message),
            }
        }
        if kind == "r" && related.len() < 2 {
            let has_similar = related.iter().any(|r| {
                r["items"]
                    .as_array()
                    .is_some_and(|i| i.iter().any(|x| x["kind"] == "artist"))
            });
            if !has_similar {
                match self.similar(id) {
                    Ok(list) if !list.is_empty() => related.push(json!({
                        "title": if fr { "Artistes similaires" } else { "Similar artists" },
                        "items": list,
                    })),
                    Ok(_) => {}
                    Err(e) => eprintln!("similar to r/{id}: {}", e.message),
                }
            }
        }
        if !related.is_empty() {
            out["related"] = related.into();
        }
        let facts = items::facts(m, fr);
        if !facts.is_empty() {
            out["facts"] = facts.into();
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- radio

    /// Tracks to follow `id` (a track, album or artist), none of `exclude`
    /// (refs): for a track, those that sound like it first; then, at
    /// random, tracks of its artist and of similar artists.
    fn radio(&self, id: &str, exclude: &[String], limit: usize) -> Result<Vec<Value>, RpcError> {
        let (_, v) = self.get(&format!("/library/metadata/{id}"), &[])?;
        let m = &v["Metadata"][0];
        let mut seen: HashSet<String> = exclude.iter().cloned().collect();
        let mut out = Vec::new();
        let mut take = |list: Vec<Value>, out: &mut Vec<Value>| {
            for t in list {
                let r = t["ref"].as_str().unwrap_or("").to_string();
                if out.len() < limit && t["playable"] == true && seen.insert(r) {
                    out.push(t);
                }
            }
        };
        let artist = match m["type"].as_str() {
            Some("track") => {
                let want = limit + exclude.len() + 1;
                let mut near = self.sonic(id, want.min(PAGE as usize))?;
                near.retain(|t| t["ref"] != format!("t/{id}"));
                take(near, &mut out);
                &m["grandparentRatingKey"]
            }
            Some("album") => &m["parentRatingKey"],
            Some("artist") => &m["ratingKey"],
            _ => return Ok(out),
        };
        let Some(artist) = artist.as_str().filter(|_| out.len() < limit) else {
            return Ok(out);
        };
        let mut ids = vec![artist.to_string()];
        let (_, similar) = self.similar_raw(artist)?;
        ids.extend(
            similar
                .iter()
                .filter(|a| a["type"] == "artist")
                .filter_map(|a| a["ratingKey"].as_str().map(str::to_string)),
        );
        let q = [
            ("type", "10".to_string()),
            ("artist.id", ids.join(",")),
            ("sort", "random".to_string()),
        ];
        let size = (limit + exclude.len() + 1).min(PAGE as usize) as u64;
        let (s, pg) = self.page("/library/all", &q, 0, size)?;
        let mut list = items::many(&s, &pg.items, items::track);
        if m["type"] == "track" {
            list.retain(|t| t["ref"] != format!("t/{id}"));
        }
        take(list, &mut out);
        Ok(out)
    }

    fn radio_next(&self, p: &Value) -> Reply {
        let (_, id) = items::split_ref(p["seed"].as_str().unwrap_or(""))
            .ok_or_else(|| rpc_err(-32602, "bad seed"))?;
        let exclude: Vec<String> = p["exclude"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let limit = p["limit"].as_u64().unwrap_or(20).clamp(1, RADIO_MAX as u64) as usize;
        Ok(json!({ "items": self.radio(id, &exclude, limit)? }))
    }

    // ------------------------------------------------------------ playlists

    /// `server://<machine>/…/library/metadata/<ids>`: items of this server,
    /// as playlists take them.
    fn items_uri(&self, ids: &[&str]) -> Result<String, RpcError> {
        let s = self.session()?;
        let machine = if s.machine_id.is_empty() {
            self.with(|s| self.client.identity(&s.server, &s.token))?
        } else {
            s.machine_id
        };
        Ok(format!(
            "server://{machine}/{LIBRARY}/library/metadata/{}",
            ids.join(",")
        ))
    }

    /// The rating key of playlist `p["ref"]`, when the user may edit it.
    fn editable_playlist(&self, p: &Value) -> Result<String, RpcError> {
        let Some(("p", id)) = items::split_ref(p["ref"].as_str().unwrap_or("")) else {
            return Err(rpc_err(-32602, "not a playlist"));
        };
        let (_, v) = self.get(&format!("/playlists/{id}"), &[])?;
        if !items::editable(&v["Metadata"][0]) {
            return Err(rpc_err(-32602, "this playlist cannot be edited"));
        }
        Ok(id.to_string())
    }

    fn name(p: &Value) -> Result<String, RpcError> {
        let name = p["name"].as_str().unwrap_or("").trim();
        if name.is_empty() {
            return Err(rpc_err(-32602, "a playlist needs a name"));
        }
        Ok(name.to_string())
    }

    /// A change on the server, without an answer.
    fn edit(&self, method: &str, path: &str, q: &[(&str, String)]) -> Reply {
        self.with(|s| self.client.call(s, method, path, q))
            .map(|_| Value::Null)
    }

    /// An empty audio playlist (Plex playlists are the user's own: no
    /// public ones), with its description.
    fn playlist_create(&self, p: &Value) -> Reply {
        let name = Self::name(p)?;
        let q = [
            ("type", "audio".to_string()),
            ("title", name),
            ("smart", "0".to_string()),
            ("uri", self.items_uri(&[])?),
        ];
        let (s, v) =
            self.with(|s| Ok((s.clone(), self.client.request(s, "POST", "/playlists", &q)?)))?;
        let mut pl = v["Metadata"][0].clone();
        let id = pl["ratingKey"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| rpc_err(-32603, "the server did not create the playlist"))?;
        if let Some(d) = p["description"]
            .as_str()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            let q = [("summary", d.to_string())];
            match self.edit("PUT", &format!("/playlists/{id}"), &q) {
                Ok(_) => pl["summary"] = d.into(),
                Err(e) => eprintln!("description of p/{id}: {}", e.message),
            }
        }
        items::playlist(&s, &pl)
            .ok_or_else(|| rpc_err(-32603, "the server did not create the playlist"))
    }

    fn playlist_rename(&self, p: &Value) -> Reply {
        let name = Self::name(p)?;
        let id = self.editable_playlist(p)?;
        self.edit("PUT", &format!("/playlists/{id}"), &[("title", name)])
    }

    fn playlist_delete(&self, p: &Value) -> Reply {
        let id = self.editable_playlist(p)?;
        self.edit("DELETE", &format!("/playlists/{id}"), &[])
    }

    /// Tracks (or whole albums) at the end of the playlist.
    fn playlist_add(&self, p: &Value) -> Reply {
        let refs = p["items"].as_array().map(Vec::as_slice).unwrap_or_default();
        let ids = refs
            .iter()
            .map(|r| match items::split_ref(r.as_str().unwrap_or("")) {
                Some(("t" | "a", id)) => Ok(id),
                _ => Err(rpc_err(-32602, "only tracks and albums go in a playlist")),
            })
            .collect::<Result<Vec<&str>, RpcError>>()?;
        if ids.is_empty() {
            return Err(rpc_err(-32602, "nothing to add"));
        }
        let id = self.editable_playlist(p)?;
        let q = [("uri", self.items_uri(&ids)?)];
        self.edit("PUT", &format!("/playlists/{id}/items"), &q)
    }

    /// Entries are `playlistItemID`s, as `entry_id` gives them.
    fn playlist_remove(&self, p: &Value) -> Reply {
        let entries = entries(&p["entries"])?;
        let id = self.editable_playlist(p)?;
        for e in entries {
            self.edit("DELETE", &format!("/playlists/{id}/items/{e}"), &[])?;
        }
        Ok(Value::Null)
    }

    /// Plex moves an entry after another one: the one before index `to`.
    fn playlist_move(&self, p: &Value) -> Reply {
        let entry = entries(&json!([p["entry"]]))?.remove(0);
        let to = p["to"]
            .as_u64()
            .ok_or_else(|| rpc_err(-32602, "no target position"))? as usize;
        let id = self.editable_playlist(p)?;
        let (_, pg) = self.page(&format!("/playlists/{id}/items"), &[], 0, PLAYLIST_MAX)?;
        let list: Vec<String> = pg
            .items
            .iter()
            .filter_map(|t| match &t["playlistItemID"] {
                Value::Number(n) => Some(n.to_string()),
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        let after = items::move_after(&list, &entry, to)
            .ok_or_else(|| rpc_err(-32002, "no such entry in the playlist"))?;
        let q: Vec<(&str, String)> = after.into_iter().map(|a| ("after", a)).collect();
        self.edit("PUT", &format!("/playlists/{id}/items/{entry}/move"), &q)
    }

    // ------------------------------------------------------------ reporting

    /// The timeline while a track plays (the server's "now playing" and
    /// resume point), and `/:/scrobble` once it counts as played: to its
    /// end, or half of it, or four minutes.
    fn report(&self, method: &str, p: &Value) {
        if !self.settings.lock().unwrap().report {
            return;
        }
        let Ok(s) = self.session() else {
            return;
        };
        let r = p["ref"].as_str().unwrap_or("");
        let Some(("t", id)) = items::split_ref(r) else {
            return;
        };
        let timeline = |state: &str, pos: u64, pl: &Playing| {
            let q = [
                ("ratingKey", id.to_string()),
                ("key", format!("/library/metadata/{id}")),
                ("state", state.to_string()),
                ("time", pos.to_string()),
                ("duration", pl.duration_ms.to_string()),
                ("X-Plex-Session-Identifier", pl.session_id.clone()),
            ];
            if let Err(e) = self.client.call(&s, "POST", "/:/timeline", &q) {
                eprintln!("{method}: timeline: {e}");
            }
        };
        match method {
            "playback.started" => {
                let duration_ms = self
                    .client
                    .get(&s, &format!("/library/metadata/{id}"), &[])
                    .ok()
                    .and_then(|v| v["Metadata"][0]["duration"].as_u64())
                    .unwrap_or(0);
                let pl = Playing {
                    duration_ms,
                    session_id: random_hex(8),
                };
                timeline("playing", 0, &pl);
                self.playing.lock().unwrap().insert(r.to_string(), pl);
            }
            "playback.progress" => {
                if let Some(pl) = self.playing.lock().unwrap().get(r) {
                    timeline("playing", p["pos_ms"].as_u64().unwrap_or(0), pl);
                }
            }
            "playback.ended" => {
                let Some(pl) = self.playing.lock().unwrap().remove(r) else {
                    return;
                };
                let listened = p["listened_ms"].as_u64().unwrap_or(0);
                timeline("stopped", listened.min(pl.duration_ms), &pl);
                let played = p["reason"] == "ended"
                    || listened >= 240_000
                    || (pl.duration_ms > 0 && listened * 2 >= pl.duration_ms);
                if played {
                    let q = [("identifier", LIBRARY.to_string()), ("key", id.to_string())];
                    if let Err(e) = self.client.call(&s, "PUT", "/:/scrobble", &q) {
                        eprintln!("{method}: scrobble: {e}");
                    }
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------- dispatch

    fn handle(self: &Arc<Self>, method: &str, p: &Value) -> Reply {
        match method {
            "initialize" => self.initialize(p),
            "auth.status" => Ok(self.auth_status()),
            "auth.begin" => self.auth_begin(),
            "auth.complete" => self.auth_complete(p),
            "auth.sign_out" => self.sign_out(),
            "browse.root" => self.root(),
            "browse.list" => self.list(p),
            "search" => self.search(p),
            "item.get" => self.item_get(p),
            "favorites.set" => self.favorite(p),
            "library.albums" | "library.artists" | "library.tracks" | "library.playlists" => {
                self.library(method, p)
            }
            "track.resolve" => self.resolve(p),
            "lyrics.get" => self.lyrics(p),
            "item.details" => self.details(p),
            "radio.next" => self.radio_next(p),
            "playlists.create" => self.playlist_create(p),
            "playlists.rename" => self.playlist_rename(p),
            "playlists.delete" => self.playlist_delete(p),
            "playlists.add" => self.playlist_add(p),
            "playlists.remove" => self.playlist_remove(p),
            "playlists.move" => self.playlist_move(p),
            _ => Err(rpc_err(-32601, format!("method not found: {method}"))),
        }
    }
}

/// Playlist entries (`playlistItemID`s, numbers) from the host.
fn entries(v: &Value) -> Result<Vec<String>, RpcError> {
    let list: Vec<String> = v
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|e| match e {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        })
        .collect();
    let ok = |e: &String| !e.is_empty() && e.len() <= 20 && e.bytes().all(|b| b.is_ascii_digit());
    if list.is_empty() || !list.iter().all(ok) {
        return Err(rpc_err(-32602, "bad playlist entries"));
    }
    Ok(list)
}

/// Write `text` to `path` with mode 600, atomically.
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(text.as_bytes())?;
    std::fs::rename(&tmp, path)
}

fn page(all: Vec<Value>, offset: u64, limit: u64) -> Value {
    let total = all.len() as u64;
    let items: Vec<Value> = all
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();
    json!({"items": items, "total": total, "has_more": offset + limit < total})
}

fn main() {
    let mut server_hint = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--server" => server_hint = args.next().unwrap_or_default(),
            "--version" => {
                println!("ricercar-plex {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => eprintln!("unknown option {a}"),
        }
    }
    let out = Arc::new(Out(Mutex::new(std::io::stdout())));
    let plugin = Arc::new(Plugin {
        out: out.clone(),
        server_hint,
        data_dir: Mutex::new(std::env::temp_dir()),
        settings: Mutex::new(Settings::from_json(&Value::Null)),
        output: Mutex::new(Output::default()),
        client: Arc::new(Client::new()),
        session: Mutex::new(None),
        expired: Mutex::new(false),
        pin: Mutex::new((0, None)),
        login_error: Mutex::new(None),
        playing: Mutex::new(HashMap::new()),
    });

    for line in BufReader::new(std::io::stdin()).lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = msg["method"].as_str().map(str::to_string) else {
            continue; // an answer; this plugin sends no requests
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = msg.get("id").cloned() else {
            // Notifications.
            match method.as_str() {
                "output.changed" => {
                    *plugin.output.lock().unwrap() = Output::from_json(&params["output"]);
                }
                "settings.changed" => plugin.apply_settings(&params["settings"]),
                m if m.starts_with("playback.") => {
                    let plugin = plugin.clone();
                    std::thread::spawn(move || plugin.report(&method, &params));
                }
                _ => {}
            }
            continue;
        };
        if method == "shutdown" {
            out.send(json!({"jsonrpc": "2.0", "id": id, "result": null}));
            return;
        }
        let first = method == "initialize";
        let run = {
            let plugin = plugin.clone();
            let out = out.clone();
            move || {
                let reply = match plugin.handle(&method, &params) {
                    Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
                    Err(e) => json!({"jsonrpc": "2.0", "id": id,
                                     "error": {"code": e.code, "message": e.message}}),
                };
                out.send(reply);
            }
        };
        // The handshake first, in order; everything else may overlap.
        if first {
            run();
        } else {
            std::thread::spawn(run);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        let d = Settings::from_json(&Value::Null);
        assert!(d.report && d.transcode);
        let s = Settings::from_json(&json!({"report_playback": false, "transcode": "never",
                                            "gone": 1}));
        assert!(!s.report && !s.transcode);
        let odd = Settings::from_json(&json!({"report_playback": "no", "transcode": 3}));
        assert_eq!(odd, d);
        let decl = Settings::declare(true);
        assert_eq!(decl[0]["key"], "report_playback");
        assert_eq!(decl[0]["default"], true);
        assert_eq!(decl[1]["default"], "auto");
        assert_eq!(decl[1]["section"], "Lecture");
    }

    #[test]
    fn playlist_entries() {
        assert_eq!(
            entries(&json!(["12", 13])).ok(),
            Some(vec!["12".into(), "13".into()])
        );
        assert!(entries(&json!([])).is_err());
        assert!(entries(&json!(["1/2"])).is_err());
        assert!(entries(&json!([null])).is_err());
    }
}
