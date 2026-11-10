//! Just enough HTTP/1.1 for Docker clients: keep-alive, Content-Length and
//! chunked bodies, streamed (chunked) responses and connection upgrades for
//! `attach`. Docker clients keep a few connections open, so the server runs
//! one thread per connection and never parses more than it needs.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;

/// API version this daemon speaks. The Docker CLI negotiates down to it.
pub const API_VERSION: &str = "1.47";
pub const MIN_API_VERSION: &str = "1.24";

/// Largest request body read into memory (container create, etc.). Archives
/// and image loads are streamed with [`Body`] instead, at any size.
const MAX_BODY: usize = 8 << 20;
/// The request line and headers together may not exceed this: an endless
/// header line must not grow memory without bound.
const MAX_HEAD: u64 = 64 << 10;

#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// Path without the `/v1.xx` prefix or the query string.
    pub path: String,
    api_version: Option<String>,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn query(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Every value of a repeated key (`names=a&names=b`).
    pub fn queries<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.query.iter().filter(move |(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Docker sends booleans as `1`/`true`/`0`/`false`.
    pub fn flag(&self, key: &str) -> bool {
        matches!(self.query(key), Some("1" | "true" | "True"))
    }

    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }

    /// A request the daemon makes of itself (the managed builder's
    /// container create), so it goes through the same code as a client's.
    pub fn internal(method: &str, path: &str, query: &[(&str, &str)], body: Vec<u8>) -> Request {
        let query = query.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        Request { method: method.to_owned(), path: path.to_owned(), api_version: None, query, headers: Vec::new(), body }
    }

    pub fn version_error(&self) -> Option<String> {
        use std::cmp::Ordering::{Greater, Less};
        let version = self.api_version.as_deref()?;
        if compare_versions(version, MIN_API_VERSION) == Less {
            Some(format!("client version {version} is too old. Minimum supported API version is {MIN_API_VERSION}, please upgrade your client to a newer version"))
        } else if compare_versions(version, API_VERSION) == Greater {
            Some(format!("client version {version} is too new. Maximum supported API version is {API_VERSION}"))
        } else {
            None
        }
    }

    /// Whether the client sent, or may have sent, a body with this request.
    pub fn has_body(&self) -> bool {
        self.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") || (k.eq_ignore_ascii_case("content-length") && v != "0"))
    }
}

/// Reads the next request on a connection. `Ok(None)` is a clean close.
/// When `stream(method, path)` is true the body stays unread: the handler
/// reads it through [`Body`].
pub fn read_request(r: &mut BufReader<UnixStream>, stream: impl Fn(&str, &str) -> bool) -> io::Result<Option<Request>> {
    // The request line and headers are read through a cap, so a client
    // cannot exhaust memory with an endless line. The body is read after.
    let (method, path, api_version, query, headers) = {
        let mut head = (&mut *r).take(MAX_HEAD);
        let mut line = String::new();
        if head.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad request line"));
        };
        let method = method.to_owned();
        let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
        let path = strip_version(raw_path);
        let api_version = (path != raw_path).then(|| raw_path[2..raw_path.len() - path.len()].to_owned());
        let path = path.to_owned();
        let query: Vec<(String, String)> = raw_query
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (percent_decode(k), percent_decode(v))
            })
            .collect();
        let mut headers = Vec::new();
        loop {
            line.clear();
            if head.read_line(&mut line)? == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "headers too large or cut short"));
            }
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.push((k.trim().to_owned(), v.trim().to_owned()));
            }
        }
        (method, path, api_version, query, headers)
    };
    let streamed = stream(&method, &path);
    let mut req = Request { method, path, api_version, query, headers, body: Vec::new() };
    // The router rejects incompatible clients and closes the connection if
    // a body is pending. Do not wait for one (possibly withheld for 100
    // Continue) first.
    if streamed || req.version_error().is_some() {
        return Ok(Some(req));
    }
    if req.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        let mut line = String::new();
        loop {
            line.clear();
            // A chunk-size line is short hex (plus an optional ;extension).
            (&mut *r).take(64).read_line(&mut line)?;
            let size = line.trim().split(';').next().unwrap_or("");
            let n = usize::from_str_radix(size, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if n > MAX_BODY || req.body.len().checked_add(n).is_none_or(|t| t > MAX_BODY) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "request body too large"));
            }
            let start = req.body.len();
            req.body.resize(start + n, 0);
            r.read_exact(&mut req.body[start..])?;
            let mut crlf = [0u8; 2];
            r.read_exact(&mut crlf)?;
            if n == 0 {
                break;
            }
        }
    } else if let Some(len) = req.header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        if len > MAX_BODY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request body too large"));
        }
        req.body.resize(len, 0);
        r.read_exact(&mut req.body)?;
    }
    Ok(Some(req))
}

/// A streamed request body: Content-Length or chunked, read as it arrives.
pub struct Body<'a> {
    r: &'a mut BufReader<UnixStream>,
    chunked: bool,
    /// Bytes left in the body (or in the current chunk).
    left: u64,
    done: bool,
}

impl<'a> Body<'a> {
    pub fn new(req: &Request, r: &'a mut BufReader<UnixStream>) -> Body<'a> {
        let chunked = req.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
        let left = if chunked { 0 } else { req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0) };
        Body { r, chunked, left, done: !chunked && left == 0 }
    }
}

impl Read for Body<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.chunked && self.left == 0 {
            let mut line = String::new();
            self.r.read_line(&mut line)?;
            // A chunk size may carry extensions after a semicolon.
            let size = line.trim().split(';').next().unwrap_or("");
            self.left = u64::from_str_radix(size, 16).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if self.left == 0 {
                // Trailers, then the blank line.
                loop {
                    line.clear();
                    if self.r.read_line(&mut line)? == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let n = self.r.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "request body cut short"));
        }
        self.left -= n as u64;
        if self.left == 0 {
            if self.chunked {
                let mut crlf = [0u8; 2];
                self.r.read_exact(&mut crlf)?;
            } else {
                self.done = true;
            }
        }
        Ok(n)
    }
}

/// `/v1.47/containers/json` → `/containers/json`.
fn strip_version(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("/v")
        && let Some(i) = rest.find('/')
            && i > 0 && rest[..i].bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                return &rest[i..];
            }
    path
}

/// Orders two versions the way Docker does: number by number between the
/// dots, a missing or empty one counting as 0, so `1.47.0` is `1.47` and
/// `1.` is `1.0`.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    // A prefix holds digits and dots only: a number that does not fit is
    // simply larger than any real version.
    let number = |part: Option<&str>| part.filter(|p| !p.is_empty()).map_or(0, |p| p.parse::<u64>().unwrap_or(u64::MAX));
    let (mut a, mut b) = (a.split('.'), b.split('.'));
    loop {
        let (x, y) = (a.next(), b.next());
        if x.is_none() && y.is_none() {
            return std::cmp::Ordering::Equal;
        }
        let order = number(x).cmp(&number(y));
        if order.is_ne() {
            return order;
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() && hex(b[i + 1]).is_some() && hex(b[i + 2]).is_some() => {
                out.push(hex(b[i + 1]).unwrap_or(0) << 4 | hex(b[i + 2]).unwrap_or(0));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn reason(status: u16) -> &'static str {
    match status {
        101 => "UPGRADED",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "",
    }
}

thread_local! {
    /// The request this connection thread is answering, for the log.
    static SERVING: std::cell::RefCell<(String, String)> = const { std::cell::RefCell::new((String::new(), String::new())) };
}

/// Notes the request being answered: a failure status written for it is
/// logged with its method and path (never its query or body).
pub fn serving(method: &str, path: &str) {
    SERVING.with(|s| *s.borrow_mut() = (method.to_owned(), path.to_owned()));
}

/// Engine API failures in the log: 5xx as errors, 4xx as warnings, except
/// 404s, which clients use to ask (does this image exist yet?).
fn note_failure(status: u16) {
    SERVING.with(|s| {
        let (method, path) = &*s.borrow();
        match status {
            404 => crate::diag::debug!("request failed", method = method, path = path, status = status),
            400..=499 => crate::diag::warn!("request failed", method = method, path = path, status = status),
            _ => crate::diag::error!("request failed", method = method, path = path, status = status),
        }
    });
}

fn head(w: &mut impl Write, status: u16, extra: &str) -> io::Result<()> {
    if status >= 400 {
        note_failure(status);
    }
    write!(
        w,
        "HTTP/1.1 {status} {}\r\nApi-Version: {API_VERSION}\r\nDocker-Experimental: false\r\nOstype: linux\r\nServer: Kiri/{} (linux)\r\n{extra}\r\n",
        reason(status),
        env!("CARGO_PKG_VERSION")
    )
}

pub fn respond(w: &mut impl Write, status: u16, content_type: &str, body: &[u8]) -> io::Result<()> {
    let extra = if status == 204 || status == 304 {
        String::new()
    } else {
        format!("Content-Type: {content_type}\r\nContent-Length: {}\r\n", body.len())
    };
    head(w, status, &extra)?;
    w.write_all(body)?;
    w.flush()
}

/// `respond` plus extra header lines, each ending in CRLF.
pub fn respond_with(w: &mut impl Write, status: u16, content_type: &str, extra: &str, body: &[u8]) -> io::Result<()> {
    head(w, status, &format!("Content-Type: {content_type}\r\nContent-Length: {}\r\n{extra}", body.len()))?;
    w.write_all(body)?;
    w.flush()
}

pub fn json(w: &mut impl Write, status: u16, body: &str) -> io::Result<()> {
    respond(w, status, "application/json", body.as_bytes())
}

pub fn no_content(w: &mut impl Write) -> io::Result<()> {
    respond(w, 204, "", b"")
}

/// Docker's error shape: `{"message": "..."}`.
pub fn error(w: &mut impl Write, status: u16, message: &str) -> io::Result<()> {
    json(w, status, &serde_json::json!({ "message": message }).to_string())
}

/// Starts a streamed response; follow with [`chunk`] calls and [`end`].
pub fn begin_stream(w: &mut impl Write, status: u16, content_type: &str) -> io::Result<()> {
    begin_stream_with(w, status, content_type, "")
}

/// `begin_stream` plus extra header lines, each ending in CRLF.
pub fn begin_stream_with(w: &mut impl Write, status: u16, content_type: &str, extra: &str) -> io::Result<()> {
    head(w, status, &format!("Content-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n{extra}"))?;
    w.flush()
}

/// Headers only, for HEAD: `extra` lines end in CRLF.
pub fn head_only(w: &mut impl Write, status: u16, extra: &str) -> io::Result<()> {
    head(w, status, &format!("Content-Length: 0\r\n{extra}"))?;
    w.flush()
}

pub fn chunk(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    write!(w, "{:x}\r\n", data.len())?;
    w.write_all(data)?;
    w.write_all(b"\r\n")?;
    w.flush()
}

pub fn end(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"0\r\n\r\n")?;
    w.flush()
}

/// Switches the connection to Docker's raw stream for `attach`. After this
/// the connection carries multiplexed output frames until it closes.
pub fn upgrade(w: &mut impl Write) -> io::Result<()> {
    head(w, 101, "Content-Type: application/vnd.docker.raw-stream\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n")?;
    w.flush()
}

/// Docker's multiplexed stream framing: `[stream, 0, 0, 0, len u32 BE] data`.
pub fn mux_frame(w: &mut impl Write, stream: u8, data: &[u8]) -> io::Result<()> {
    let mut header = [0u8; 8];
    header[0] = stream;
    header[4..].copy_from_slice(&(data.len() as u32).to_be_bytes());
    w.write_all(&header)?;
    w.write_all(data)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_api_version() {
        assert_eq!(strip_version("/v1.47/containers/json"), "/containers/json");
        assert_eq!(strip_version("/_ping"), "/_ping");
        assert_eq!(strip_version("/volumes"), "/volumes");
        for path in ["/v1/info", "/v1./info", "/v.47/info", "/v./info", "/v1..47/info", "/v1.47.0/info"] {
            assert_eq!(strip_version(path), "/info");
        }
        for path in ["/v/info", "/v1.x/info", "/V1.47/info", "/v+1.47/info", "/v1.47"] {
            assert_eq!(strip_version(path), path);
        }
    }

    #[test]
    fn versions_compare_number_by_number() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        for (a, b, order) in [
            ("1.47", "1.47", Equal),
            ("1.47.0", "1.47", Equal),
            ("1.047", "1.47", Equal),
            ("01.47", "1.47", Equal),
            ("1.47.1", "1.47", Greater),
            ("1.100", "1.47", Greater),
            ("2", "1.47", Greater),
            ("1.99999999999999999999", "1.47", Greater),
            ("99999999999999999999.1", "1.47", Greater),
            ("1.9", "1.24", Less),
            ("1", "1.24", Less),
            ("1.", "1.24", Less),
            (".47", "1.24", Less),
            (".", "1.24", Less),
            ("1..47", "1.24", Less),
        ] {
            assert_eq!(compare_versions(a, b), order, "{a} against {b}");
            assert_eq!(compare_versions(b, a), order.reverse(), "{b} against {a}");
        }
    }

    #[test]
    fn rejects_outside_the_advertised_version_range_without_reading_a_body() {
        for (version, rejected) in [
            ("1.23", true),
            ("1.24", false),
            (API_VERSION, false),
            ("1.47.0", false),
            ("1.24.0", false),
            ("1.47.1", true),
            ("1.48", true),
            ("2.0", true),
            ("1", true),
            ("2", true),
            ("1.", true),
            (".47", true),
            (".", true),
            ("1..47", true),
            ("1.99999999999999999999", true),
        ] {
            let (mut client, server) = UnixStream::pair().unwrap();
            let body = if rejected { "Content-Length: 100\r\nExpect: 100-continue\r\n" } else { "" };
            client.write_all(format!("GET /v{version}/info HTTP/1.1\r\n{body}\r\n").as_bytes()).unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
            let request = read_request(&mut BufReader::new(server), |_, _| false).unwrap().unwrap();
            assert_eq!(request.path, "/info");
            assert_eq!(request.api_version.as_deref(), Some(version));
            assert_eq!(request.version_error().is_some(), rejected, "{version}");
        }
        assert!(Request::internal("GET", "/info", &[], Vec::new()).version_error().is_none());
    }

    #[test]
    fn version_errors_read_as_dockers_do() {
        let error = |target: &str| {
            let (mut client, server) = UnixStream::pair().unwrap();
            client.write_all(format!("GET {target} HTTP/1.1\r\n\r\n").as_bytes()).unwrap();
            read_request(&mut BufReader::new(server), |_, _| false).unwrap().unwrap().version_error()
        };
        assert_eq!(error("/v1.23/info").as_deref(), Some("client version 1.23 is too old. Minimum supported API version is 1.24, please upgrade your client to a newer version"));
        assert_eq!(error("/v1./info").as_deref(), Some("client version 1. is too old. Minimum supported API version is 1.24, please upgrade your client to a newer version"));
        assert_eq!(error("/v2/info").as_deref(), Some("client version 2 is too new. Maximum supported API version is 1.47"));
        assert_eq!(error("/v1.47.1/info").as_deref(), Some("client version 1.47.1 is too new. Maximum supported API version is 1.47"));
        // Not a version prefix at all: the router answers these with 404.
        for target in ["/v/info", "/v1.x/info", "/V1.47/info", "/info"] {
            assert_eq!(error(target), None, "{target}");
        }
    }

    #[test]
    fn a_body_is_expected_from_its_framing_headers() {
        let request = |headers: &str| {
            let (mut client, server) = UnixStream::pair().unwrap();
            client.write_all(format!("GET /v9.99/info HTTP/1.1\r\n{headers}\r\n").as_bytes()).unwrap();
            read_request(&mut BufReader::new(server), |_, _| false).unwrap().unwrap()
        };
        assert!(!request("").has_body());
        assert!(!request("Content-Length: 0\r\n").has_body());
        for headers in ["Content-Length: 100\r\n", "Content-Length: +0\r\n", "Content-Length: 0, 5\r\n", "Content-Length: 0\r\nContent-Length: 5\r\n", "Transfer-Encoding: chunked\r\n", "Content-Length: 0\r\nTransfer-Encoding: identity\r\n"] {
            assert!(request(headers).has_body(), "{headers}");
        }
    }

    #[test]
    fn decodes_query_values() {
        assert_eq!(percent_decode("docker.io%2Flibrary%2Falpine"), "docker.io/library/alpine");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%7B%22a%22%3A1%7D"), "{\"a\":1}");
    }
}
