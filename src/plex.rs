//! A small client for the Plex Media Server API, as documented at
//! https://developer.plex.tv/pms/, and for the plex.tv sign-in (PIN flow)
//! and server discovery described there.

use std::time::Duration;

use serde_json::{Value, json};

pub const PRODUCT: &str = "ricercar";
const PLEX_TV: &str = "https://clients.plex.tv";
/// Where ratings and scrobbles of library items go.
pub const LIBRARY: &str = "com.plexapp.plugins.library";

/// A signed-in user on one server.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    /// The address in use, `https://…:32400` without a trailing slash.
    pub server: String,
    /// Other addresses plex.tv gave for the same server, tried in order
    /// when `server` stops answering (a laptop leaving home).
    pub connections: Vec<String>,
    /// `friendlyName` of the server.
    pub server_name: String,
    pub machine_id: String,
    /// The server access token (`X-Plex-Token`).
    pub token: String,
    /// The plex.tv user name, when known.
    pub user: String,
}

impl Session {
    pub fn to_json(&self) -> Value {
        json!({
            "server": self.server, "connections": self.connections,
            "server_name": self.server_name, "machine_id": self.machine_id,
            "token": self.token, "user": self.user,
        })
    }

    pub fn from_json(v: &Value) -> Option<Session> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Session {
            server: s("server")?,
            token: s("token")?,
            connections: v["connections"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            server_name: s("server_name").unwrap_or_default(),
            machine_id: s("machine_id").unwrap_or_default(),
            user: s("user").unwrap_or_default(),
        })
    }

    /// `<server><path>?X-Plex-Token=…&<extra>`, for URLs the host fetches
    /// itself (streams, cover art).
    pub fn url(&self, path: &str, extra: &[(&str, &str)]) -> String {
        let mut q = vec![format!("X-Plex-Token={}", encode(&self.token))];
        q.extend(extra.iter().map(|(k, v)| format!("{k}={}", encode(v))));
        let sep = if path.contains('?') { '&' } else { '?' };
        format!("{}{path}{sep}{}", self.server, q.join("&"))
    }

    /// A 600 px rendition of a `thumb` / `composite` path, through the
    /// server's photo transcoder.
    pub fn art(&self, thumb: &str) -> String {
        self.url(
            "/photo/:/transcode",
            &[
                ("width", "600"),
                ("height", "600"),
                ("minSize", "1"),
                ("upscale", "1"),
                ("url", thumb),
            ],
        )
    }
}

/// Percent-encode a query value (RFC 3986 unreserved characters stay).
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Debug)]
pub enum Error {
    /// HTTP 401: the token was revoked or the account lost access.
    Auth,
    /// HTTP 404.
    NotFound,
    /// The server said no for another reason.
    Status(u16, String),
    /// DNS, TCP, TLS, timeouts, answers that are not the Plex API.
    Network(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Auth => write!(f, "the token was refused"),
            Error::NotFound => write!(f, "not found"),
            Error::Status(code, msg) if msg.is_empty() => write!(f, "server answered {code}"),
            Error::Status(code, msg) => write!(f, "server answered {code}: {msg}"),
            Error::Network(e) => write!(f, "{e}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// `https://host:32400/` → `https://host:32400`; a bare host gets
/// `http://` and, without a port, Plex's 32400.
pub fn normalize_server(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() || s.chars().any(char::is_whitespace) {
        return None;
    }
    let (scheme, rest) = match s.split_once("://") {
        Some((sc @ ("http" | "https"), rest)) => (sc, rest),
        Some(_) => return None,
        None => ("http", s),
    };
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.starts_with('/') {
        return None;
    }
    let host = rest.split('/').next().unwrap_or(rest);
    // `[::1]:32400`: only a colon after the brackets is a port.
    let has_port = host.rsplit_once(']').map_or(host, |(_, p)| p).contains(':');
    if has_port || scheme == "https" || rest.contains('/') {
        Some(format!("{scheme}://{rest}"))
    } else {
        Some(format!("{scheme}://{rest}:32400"))
    }
}

/// One page of a list: its items and, when the server says, the total.
pub struct Page {
    pub items: Vec<Value>,
    pub total: Option<u64>,
}

/// A server of the account, from plex.tv's resources.
#[derive(Clone, Debug)]
pub struct Resource {
    pub name: String,
    pub machine_id: String,
    pub token: String,
    pub owned: bool,
    /// Candidate addresses, best first: local, then remote, then relays.
    pub connections: Vec<String>,
}

pub struct Client {
    agent: ureq::Agent,
    /// Quick checks of candidate addresses.
    probe: ureq::Agent,
    /// `X-Plex-Client-Identifier`, kept in the data directory.
    pub client_id: std::sync::Mutex<String>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Client {
        let ua = concat!("ricercar-plex/", env!("CARGO_PKG_VERSION"));
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(8))
            .user_agent(ua)
            .build();
        let probe = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(2))
            .timeout(Duration::from_secs(3))
            .user_agent(ua)
            .build();
        Client {
            agent,
            probe,
            client_id: std::sync::Mutex::new(String::new()),
        }
    }

    pub fn id(&self) -> String {
        self.client_id.lock().unwrap().clone()
    }

    /// The `X-Plex-*` headers every request carries.
    fn headers(&self, req: ureq::Request, token: Option<&str>) -> ureq::Request {
        let mut req = req
            .set("Accept", "application/json")
            .set("X-Plex-Product", PRODUCT)
            .set("X-Plex-Version", env!("CARGO_PKG_VERSION"))
            .set("X-Plex-Client-Identifier", &self.id())
            .set("X-Plex-Platform", "Linux")
            .set("X-Plex-Device", "Linux")
            .set("X-Plex-Device-Name", PRODUCT)
            .set("X-Plex-Provides", "player");
        if let Some(t) = token {
            req = req.set("X-Plex-Token", t);
        }
        req
    }

    fn send(
        &self,
        agent: &ureq::Agent,
        method: &str,
        url: &str,
        token: Option<&str>,
        query: &[(&str, String)],
        page: Option<(u64, u64)>,
    ) -> Result<Value> {
        let text = self.fetch(agent, method, url, token, query, page)?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|_| Error::Network("not a Plex server".into()))
    }

    /// The body of an answer, as text.
    fn fetch(
        &self,
        agent: &ureq::Agent,
        method: &str,
        url: &str,
        token: Option<&str>,
        query: &[(&str, String)],
        page: Option<(u64, u64)>,
    ) -> Result<String> {
        let mut req = self.headers(agent.request(method, url), token);
        for (k, v) in query {
            req = req.query(k, v);
        }
        if let Some((start, size)) = page {
            req = req
                .set("X-Plex-Container-Start", &start.to_string())
                .set("X-Plex-Container-Size", &size.to_string());
        }
        match req.call() {
            Ok(r) => r.into_string().map_err(|e| Error::Network(e.to_string())),
            Err(ureq::Error::Status(401, _)) => Err(Error::Auth),
            Err(ureq::Error::Status(404, _)) => Err(Error::NotFound),
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                let msg: String = strip_tags(&text).chars().take(200).collect();
                Err(Error::Status(code, msg.trim().to_string()))
            }
            Err(ureq::Error::Transport(t)) => Err(Error::Network(transport(&t))),
        }
    }

    // ------------------------------------------------------------- server

    /// `GET <path>` on the server: its `MediaContainer`.
    pub fn get(&self, s: &Session, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let url = format!("{}{path}", s.server);
        let v = self.send(&self.agent, "GET", &url, Some(&s.token), query, None)?;
        Ok(v["MediaContainer"].clone())
    }

    /// One page of a list endpoint (`X-Plex-Container-Start` / `-Size`).
    pub fn page(
        &self,
        s: &Session,
        path: &str,
        query: &[(&str, String)],
        offset: u64,
        limit: u64,
    ) -> Result<Page> {
        let url = format!("{}{path}", s.server);
        let v = self.send(
            &self.agent,
            "GET",
            &url,
            Some(&s.token),
            query,
            Some((offset, limit)),
        )?;
        let mc = &v["MediaContainer"];
        let items = mc["Metadata"].as_array().cloned().unwrap_or_default();
        Ok(Page {
            total: mc["totalSize"].as_u64(),
            items,
        })
    }

    /// PUT or POST without a body (ratings, scrobbles, timelines).
    pub fn call(
        &self,
        s: &Session,
        method: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<()> {
        self.request(s, method, path, query).map(|_| ())
    }

    /// Any method without a body: the `MediaContainer` of the answer
    /// (`Null` when the server sends nothing back).
    pub fn request(
        &self,
        s: &Session,
        method: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value> {
        let url = format!("{}{path}", s.server);
        let v = self.send(&self.agent, method, &url, Some(&s.token), query, None)?;
        Ok(v["MediaContainer"].clone())
    }

    /// `GET <path>` as text: a lyrics file, as stored next to the music or
    /// as the server's JSON.
    pub fn text(&self, s: &Session, path: &str) -> Result<String> {
        let url = format!("{}{path}", s.server);
        self.fetch(&self.agent, "GET", &url, Some(&s.token), &[], None)
    }

    /// `/identity` of a candidate address: its machine identifier, when it
    /// answers quickly and takes the token.
    pub fn identity(&self, server: &str, token: &str) -> Result<String> {
        // `/identity` is public; `/` checks the token and names the server.
        let root = self.send(
            &self.probe,
            "GET",
            &format!("{server}/"),
            Some(token),
            &[],
            None,
        )?;
        let mc = &root["MediaContainer"];
        mc["machineIdentifier"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Network("not a Plex Media Server".into()))
    }

    /// A session on `server` with `token`, checked against the server.
    pub fn sign_in_direct(&self, server: &str, token: &str) -> Result<Session> {
        let root = self.send(
            &self.agent,
            "GET",
            &format!("{server}/"),
            Some(token),
            &[],
            None,
        )?;
        let mc = &root["MediaContainer"];
        let machine_id = mc["machineIdentifier"]
            .as_str()
            .ok_or_else(|| Error::Network("not a Plex Media Server".into()))?;
        let user = self.user(token).unwrap_or_default();
        Ok(Session {
            server: server.to_string(),
            connections: Vec::new(),
            server_name: mc["friendlyName"].as_str().unwrap_or("").to_string(),
            machine_id: machine_id.to_string(),
            token: token.to_string(),
            user,
        })
    }

    // ------------------------------------------------------------ plex.tv

    /// A new strong PIN: `(id, code)`.
    pub fn pin_create(&self) -> Result<(u64, String)> {
        let v = self.send(
            &self.agent,
            "POST",
            &format!("{PLEX_TV}/api/v2/pins"),
            None,
            &[("strong", "true".into())],
            None,
        )?;
        match (v["id"].as_u64(), v["code"].as_str()) {
            (Some(id), Some(code)) => Ok((id, code.to_string())),
            _ => Err(Error::Network("unexpected answer from plex.tv".into())),
        }
    }

    /// The account token once the user has claimed the PIN; `None` while
    /// waiting. `NotFound` once it expired.
    pub fn pin_check(&self, id: u64) -> Result<Option<String>> {
        let v = self.send(
            &self.agent,
            "GET",
            &format!("{PLEX_TV}/api/v2/pins/{id}"),
            None,
            &[],
            None,
        )?;
        Ok(v["authToken"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(str::to_string))
    }

    /// The plex.tv user name of an account token.
    pub fn user(&self, token: &str) -> Result<String> {
        let v = self.send(
            &self.agent,
            "GET",
            &format!("{PLEX_TV}/api/v2/user"),
            Some(token),
            &[],
            None,
        )?;
        Ok(v["username"]
            .as_str()
            .or(v["title"].as_str())
            .unwrap_or("")
            .to_string())
    }

    /// The servers the account can use.
    pub fn resources(&self, token: &str) -> Result<Vec<Resource>> {
        let v = self.send(
            &self.agent,
            "GET",
            &format!("{PLEX_TV}/api/v2/resources"),
            Some(token),
            &[
                ("includeHttps", "1".into()),
                ("includeRelay", "1".into()),
                ("includeIPv6", "1".into()),
            ],
            None,
        )?;
        Ok(resources(&v, token))
    }

    /// The first address of `r` that answers with the right server.
    pub fn reach(&self, r: &Resource) -> Result<String> {
        let mut last = Error::Network("the server has no address".into());
        for uri in &r.connections {
            match self.identity(uri, &r.token) {
                Ok(id) if r.machine_id.is_empty() || id == r.machine_id => return Ok(uri.clone()),
                Ok(_) => last = Error::Network(format!("{uri} is another server")),
                Err(e @ Error::Auth) => return Err(e),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

/// Servers from a plex.tv `resources` answer, owned ones first, with their
/// addresses in order of preference.
pub fn resources(v: &Value, account_token: &str) -> Vec<Resource> {
    let mut out: Vec<Resource> = v
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            r["provides"]
                .as_str()
                .is_some_and(|p| p.split(',').any(|x| x == "server"))
        })
        .map(|r| {
            let mut conns: Vec<(u8, String)> = r["connections"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|c| {
                    let uri = c["uri"].as_str()?.trim_end_matches('/').to_string();
                    let rank = if c["relay"] == true {
                        2
                    } else if c["local"] == true {
                        0
                    } else {
                        1
                    };
                    Some((rank, uri))
                })
                .collect();
            conns.sort_by_key(|(rank, _)| *rank);
            Resource {
                name: r["name"].as_str().unwrap_or("").to_string(),
                machine_id: r["clientIdentifier"].as_str().unwrap_or("").to_string(),
                token: r["accessToken"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .unwrap_or(account_token)
                    .to_string(),
                owned: r["owned"] == true,
                connections: conns.into_iter().map(|(_, u)| u).collect(),
            }
        })
        .collect();
    out.sort_by_key(|r| !r.owned);
    out
}

/// A transport failure without the query string of its URL (search terms
/// end up in logs and in the host's messages otherwise).
fn transport(t: &ureq::Transport) -> String {
    let mut out = String::new();
    if let Some(u) = t.url() {
        out.push_str(&format!(
            "{}{}: ",
            u.origin().ascii_serialization(),
            u.path()
        ));
    }
    out.push_str(&t.kind().to_string());
    if let Some(m) = t.message() {
        out.push_str(": ");
        out.push_str(m);
    }
    if let Some(src) = std::error::Error::source(t) {
        out.push_str(&format!(": {src}"));
    }
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for c in s.chars() {
        match c {
            '<' => inside = true,
            '>' => {
                inside = false;
                out.push(' ');
            }
            _ if !inside => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_addresses() {
        let n = |s: &str| normalize_server(s);
        assert_eq!(n("nas.lan").as_deref(), Some("http://nas.lan:32400"));
        assert_eq!(
            n("192.168.1.5").as_deref(),
            Some("http://192.168.1.5:32400")
        );
        assert_eq!(n("nas.lan:32401/").as_deref(), Some("http://nas.lan:32401"));
        assert_eq!(
            n(" https://1-2-3-4.abc.plex.direct:32400/ ").as_deref(),
            Some("https://1-2-3-4.abc.plex.direct:32400")
        );
        assert_eq!(
            n("https://plex.example.org").as_deref(),
            Some("https://plex.example.org")
        );
        assert_eq!(
            n("http://example.org/plex").as_deref(),
            Some("http://example.org/plex")
        );
        assert_eq!(n("ftp://x"), None);
        assert_eq!(n("http://"), None);
        assert_eq!(n("a b"), None);
    }

    #[test]
    fn sessions_round_trip() {
        let s = Session {
            server: "https://a.plex.direct:32400".into(),
            connections: vec!["http://10.0.0.2:32400".into()],
            server_name: "Attic".into(),
            machine_id: "m1".into(),
            token: "tok".into(),
            user: "ann".into(),
        };
        assert_eq!(Session::from_json(&s.to_json()), Some(s));
        assert_eq!(Session::from_json(&json!({"server": "x"})), None);
    }

    #[test]
    fn urls() {
        let s = Session {
            server: "http://pms:32400".into(),
            connections: vec![],
            server_name: String::new(),
            machine_id: String::new(),
            token: "t&1".into(),
            user: String::new(),
        };
        assert_eq!(
            s.url("/library/parts/1/2/file.flac", &[]),
            "http://pms:32400/library/parts/1/2/file.flac?X-Plex-Token=t%261"
        );
        assert_eq!(
            s.art("/library/metadata/1/thumb/9"),
            "http://pms:32400/photo/:/transcode?X-Plex-Token=t%261&width=600&height=600\
             &minSize=1&upscale=1&url=%2Flibrary%2Fmetadata%2F1%2Fthumb%2F9"
        );
    }

    #[test]
    fn resource_order() {
        let v = json!([
            {"name": "Phone", "provides": "client,player", "connections": []},
            {"name": "Friend", "provides": "server", "owned": false, "accessToken": "shared",
             "clientIdentifier": "f1",
             "connections": [{"uri": "https://f.plex.direct:32400", "local": false}]},
            {"name": "Home", "provides": "server,player", "owned": true, "accessToken": "",
             "clientIdentifier": "h1",
             "connections": [
                {"uri": "https://r.plex.direct:8443", "local": false, "relay": true},
                {"uri": "https://w.plex.direct:32400", "local": false},
                {"uri": "https://l.plex.direct:32400/", "local": true}
             ]}
        ]);
        let r = resources(&v, "account");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "Home");
        assert_eq!(r[0].token, "account");
        assert_eq!(
            r[0].connections,
            [
                "https://l.plex.direct:32400",
                "https://w.plex.direct:32400",
                "https://r.plex.direct:8443"
            ]
        );
        assert_eq!(r[1].token, "shared");
    }

    #[test]
    fn tags() {
        assert_eq!(
            strip_tags("<html><body><h1>400 Bad Request</h1></body></html>"),
            "400 Bad Request"
        );
    }
}
