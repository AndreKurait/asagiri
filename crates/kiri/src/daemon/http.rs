//! Just enough HTTP/1.1 for Docker clients: keep-alive, Content-Length and
//! chunked bodies, streamed (chunked) responses and connection upgrades for
//! `attach`. Docker clients keep a few connections open, so the server runs
//! one thread per connection and never parses more than it needs.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};

/// API version this daemon speaks. The Docker CLI negotiates down to it.
pub const API_VERSION: &str = "1.47";
pub const MIN_API_VERSION: &str = "1.24";

/// Largest request body read into memory (container create, etc.). Archives
/// and image loads are streamed with [`Body`] instead, at any size.
const MAX_BODY: usize = 8 << 20;
/// The request line and headers together may not exceed this: an endless
/// header line must not grow memory without bound.
const MAX_HEAD: u64 = 64 << 10;
/// A chunk-size line, extensions included, may not exceed this.
const MAX_CHUNK_LINE: u64 = 4 << 10;

/// Why a request is refused before a handler answers it. The connection
/// closes after one: what follows can no longer be told apart from its body.
#[derive(Debug)]
pub enum Refused {
    /// Framing that is malformed, or that could be read two ways.
    Malformed(&'static str),
    /// A body past `MAX_BODY` where bodies are read into memory.
    TooLarge,
    /// A transfer coding other than a single `chunked`.
    Coding,
}

impl Refused {
    pub fn status(&self) -> u16 {
        match self {
            Refused::Malformed(_) => 400,
            Refused::TooLarge => 413,
            Refused::Coding => 501,
        }
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Refused::Malformed(why) => why,
            Refused::TooLarge => "request body too large",
            Refused::Coding => "unsupported transfer-encoding",
        })
    }
}

impl std::error::Error for Refused {}

impl From<Refused> for io::Error {
    fn from(r: Refused) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, r)
    }
}

/// The status that answers a request refused with `e`; `None` when the
/// connection itself failed and there is nobody to answer.
pub fn refusal(e: &io::Error) -> Option<u16> {
    match e.get_ref().and_then(|e| e.downcast_ref::<Refused>()) {
        Some(r) => Some(r.status()),
        // Bytes that are not text where a line of the head belongs.
        None => (e.kind() == io::ErrorKind::InvalidData).then_some(400),
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// Path without the `/v1.xx` prefix or the query string.
    pub path: String,
    api_version: Option<String>,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The client sent `Expect: 100-continue` and has not been told to
    /// send the body yet.
    awaits_continue: AtomicBool,
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
        Request { method: method.to_owned(), path: path.to_owned(), api_version: None, query, headers: Vec::new(), body, awaits_continue: AtomicBool::new(false) }
    }

    /// Tells a client waiting on `Expect: 100-continue` to send the body.
    /// [`Body`] does it on its first read, so a request refused before then
    /// gets no interim response; a handler that starts its answer before it
    /// reads the body calls this first.
    pub fn accept_body(&self, w: &mut impl Write) -> io::Result<()> {
        if self.awaits_continue.swap(false, Ordering::Relaxed) {
            w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            w.flush()?;
        }
        Ok(())
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
    // Until its request line is read, a request refused here is nobody's.
    serving("", "");
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
            return Err(Refused::Malformed("bad request line").into());
        };
        let method = method.to_owned();
        let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
        // Decoded before the version is stripped, as Docker routes: an
        // escaped dot or slash in the prefix still makes it the prefix.
        let decoded = escapes_are_valid(raw_path).then(|| percent_decode(raw_path, false));
        let (path, api_version) = match &decoded {
            Some(decoded) => {
                let path = strip_version(decoded);
                (Ok(path.to_owned()), (path != decoded.as_str()).then(|| decoded[2..decoded.len() - path.len()].to_owned()))
            }
            None => (Err(raw_path.to_owned()), None),
        };
        serving(&method, match &path {
            Ok(path) | Err(path) => path,
        });
        let query: Vec<(String, String)> = raw_query
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (percent_decode(k, true), percent_decode(v, true))
            })
            .collect();
        let mut headers = Vec::new();
        loop {
            line.clear();
            head.read_line(&mut line)?;
            let Some(l) = line.strip_suffix('\n') else {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "headers too large or cut short"));
            };
            let l = l.strip_suffix('\r').unwrap_or(l);
            if l.is_empty() {
                break;
            }
            // A name is a token with the colon right after it: a folded
            // line or a padded name must not pass for a framing header.
            let Some((k, v)) = l.split_once(':').filter(|(k, _)| is_token(k)) else {
                return Err(Refused::Malformed("malformed header line").into());
            };
            headers.push((k.to_owned(), v.trim_matches([' ', '\t']).to_owned()));
        }
        (method, path, api_version, query, headers)
    };
    let path = match path {
        Ok(path) => path,
        Err(raw) => {
            // Go's HTTP server, and so Docker, answers a malformed escape
            // itself with a plain 400 and closes the connection.
            serving(&method, &raw);
            respond_with(r.get_mut(), 400, "text/plain; charset=utf-8", "Connection: close\r\n", b"400 Bad Request")?;
            return Ok(None);
        }
    };
    let streamed = stream(&method, &path);
    let mut req = Request { method, path, api_version, query, headers, body: Vec::new(), awaits_continue: AtomicBool::new(false) };
    validate_framing(&req)?;
    let length = req.header("content-length").and_then(|v| v.parse::<u64>().ok());
    if !streamed && length.is_some_and(|n| n > MAX_BODY as u64) {
        return Err(Refused::TooLarge.into());
    }
    let has_body = req.header("transfer-encoding").is_some() || length.is_some_and(|n| n != 0);
    req.awaits_continue = AtomicBool::new(has_body && req.header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")));
    // The router rejects incompatible clients and closes the connection if
    // a body is pending. Do not wait for one (possibly withheld for 100
    // Continue) first.
    if streamed || req.version_error().is_some() {
        return Ok(Some(req));
    }
    let mut body = Vec::new();
    Body::new(&req, r).take(MAX_BODY as u64 + 1).read_to_end(&mut body)?;
    if body.len() > MAX_BODY {
        return Err(Refused::TooLarge.into());
    }
    req.body = body;
    Ok(Some(req))
}

/// A header name: one or more of HTTP's token characters.
fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Refuses framing that leaves the end of the body open to interpretation.
fn validate_framing(req: &Request) -> io::Result<()> {
    let values = |name: &'static str| req.headers.iter().filter(move |(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
    let lengths: Vec<_> = values("content-length").collect();
    let codings: Vec<_> = values("transfer-encoding").collect();
    // Docker answers 501 to anything but one `chunked`, a repeat included.
    if codings.len() > 1 || codings.first().is_some_and(|v| !v.eq_ignore_ascii_case("chunked")) {
        return Err(Refused::Coding.into());
    }
    if lengths.windows(2).any(|pair| pair[0] != pair[1]) || (!lengths.is_empty() && !codings.is_empty()) {
        return Err(Refused::Malformed("ambiguous request framing").into());
    }
    // Digits only: `parse` alone would take a sign.
    if lengths.first().is_some_and(|v| v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) || v.parse::<u64>().is_err()) {
        return Err(Refused::Malformed("invalid content-length").into());
    }
    Ok(())
}

/// A streamed request body: Content-Length or chunked, read as it arrives.
pub struct Body<'a> {
    req: &'a Request,
    r: &'a mut BufReader<UnixStream>,
    chunked: bool,
    /// Bytes left in the body (or in the current chunk).
    left: u64,
    done: bool,
}

impl<'a> Body<'a> {
    pub fn new(req: &'a Request, r: &'a mut BufReader<UnixStream>) -> Body<'a> {
        let chunked = req.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
        let left = if chunked { 0 } else { req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0) };
        Body { req, r, chunked, left, done: !chunked && left == 0 }
    }
}

impl Read for Body<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        self.req.accept_body(self.r.get_mut())?;
        if self.chunked && self.left == 0 {
            let mut line = String::new();
            (&mut *self.r).take(MAX_CHUNK_LINE).read_line(&mut line)?;
            // The line ends in CRLF and nothing else: a lone LF, or space
            // around the size, would let two parsers disagree on the chunk.
            let Some(size) = line.strip_suffix("\r\n") else {
                return Err(Refused::Malformed("invalid chunk-size line").into());
            };
            // A chunk size may carry extensions after a semicolon.
            let size = size.split(';').next().unwrap_or("");
            // Hex digits only: `from_str_radix` alone would take a sign.
            if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Refused::Malformed("bad chunk size").into());
            }
            self.left = u64::from_str_radix(size, 16).map_err(|_| Refused::Malformed("bad chunk size"))?;
            if self.left == 0 {
                // Trailers, then the blank line.
                let mut trailers = (&mut *self.r).take(MAX_HEAD);
                loop {
                    line.clear();
                    trailers.read_line(&mut line)?;
                    if !line.ends_with("\r\n") {
                        return Err(Refused::Malformed("trailers too large or cut short").into());
                    }
                    if line == "\r\n" {
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
                if crlf != *b"\r\n" {
                    return Err(Refused::Malformed("invalid chunk terminator").into());
                }
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

/// Whether every `%` in a path starts a two-digit hex escape.
fn escapes_are_valid(path: &str) -> bool {
    let b = path.as_bytes();
    (0..b.len()).all(|i| b[i] != b'%' || (i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit()))
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

fn percent_decode(s: &str, form: bool) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' if form => out.push(b' '),
            b'%' if i + 2 < b.len() && hex(b[i + 1]).is_some() && hex(b[i + 2]).is_some() => {
                out.push(hex(b[i + 1]).unwrap_or(0) << 4 | hex(b[i + 2]).unwrap_or(0));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
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
        413 => "Request Entity Too Large",
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

/// docker-java attaches without upgrading. Like Docker, upgrade on the
/// presence of the header; otherwise the stream ends with the connection.
pub fn raw_stream(req: &Request, w: &mut impl Write) -> io::Result<()> {
    if req.header("Upgrade").is_some() {
        return upgrade(w);
    }
    head(w, 200, "Content-Type: application/vnd.docker.raw-stream\r\nConnection: close\r\n")?;
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

    fn input(bytes: &[u8]) -> BufReader<UnixStream> {
        let (mut client, server) = UnixStream::pair().unwrap();
        let bytes = bytes.to_vec();
        std::thread::spawn(move || {
            let _ = client.write_all(&bytes);
            let _ = client.shutdown(std::net::Shutdown::Write);
        });
        BufReader::new(server)
    }

    fn body(r: &mut BufReader<UnixStream>, streamed: bool) -> io::Result<Vec<u8>> {
        let req = read_request(r, |_, _| streamed)?.unwrap();
        if !streamed {
            return Ok(req.body);
        }
        let mut out = Vec::new();
        Body::new(&req, r).read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn chunked_trailers_preserve_the_next_request() {
        for streamed in [false, true] {
            let mut r = input(b"POST /auth HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2;ext=yes\r\n{}\r\n0\r\nX-Checksum: abc\r\n\r\nGET /_ping HTTP/1.1\r\n\r\n");
            assert_eq!(body(&mut r, streamed).unwrap(), b"{}");
            assert_eq!(read_request(&mut r, |_, _| false).unwrap().unwrap().path, "/_ping");
        }
    }

    #[test]
    fn rejects_ambiguous_or_invalid_framing_before_reading_a_body() {
        for streamed in [false, true] {
            for (headers, status) in [
                ("Content-Length: 0\r\nTransfer-Encoding: chunked", 400),
                ("Transfer-Encoding: chunked\r\nContent-Length: 2", 400),
                ("Content-Length: 0\r\nContent-Length: 2", 400),
                ("Content-Length: 2\r\nContent-Length: 02", 400),
                ("Content-Length: -1", 400),
                ("Content-Length: xyz", 400),
                ("Content-Length: +2", 400),
                ("Content-Length: 0x2", 400),
                ("Content-Length: 2, 2", 400),
                ("Content-Length: 2 2", 400),
                ("Content-Length: 18446744073709551616", 400),
                ("Content-Length:", 400),
                ("Transfer-Encoding: gzip", 501),
                ("Transfer-Encoding: identity", 501),
                ("Transfer-Encoding: gzip, chunked", 501),
                ("Transfer-Encoding: chunked, chunked", 501),
                ("Transfer-Encoding:", 501),
                ("Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked", 501),
                ("Transfer-Encoding: gzip\r\nContent-Length: 2", 501),
            ] {
                let mut r = input(format!("POST /auth HTTP/1.1\r\n{headers}\r\n\r\n").as_bytes());
                assert_eq!(refusal(&read_request(&mut r, |_, _| streamed).unwrap_err()), Some(status), "{headers}");
            }
        }
    }

    #[test]
    fn rejects_header_lines_that_could_pass_for_framing() {
        for headers in [
            "X-A: b\r\n Content-Length: 2",
            "X-A: b\r\n\tTransfer-Encoding: chunked",
            "X-A: b\r\n \r\nContent-Length: 2",
            " Content-Length: 2",
            "Content-Length : 2",
            "Content-Length\t: 2",
            "Content Length: 2",
            "Content-Length\r: 2",
            ": 2",
            "Content-Length",
        ] {
            let mut r = input(format!("POST /auth HTTP/1.1\r\n{headers}\r\n\r\n{{}}").as_bytes());
            assert_eq!(refusal(&read_request(&mut r, |_, _| false).unwrap_err()), Some(400), "{headers:?}");
        }
        // Only spaces and tabs around a value are not part of it.
        for (headers, status) in [("Content-Length: 2\x0b", 400), ("Content-Length: 2\r", 400), ("Transfer-Encoding: \x0cchunked", 501)] {
            let mut r = input(format!("POST /auth HTTP/1.1\r\n{headers}\r\n\r\n{{}}").as_bytes());
            assert_eq!(refusal(&read_request(&mut r, |_, _| false).unwrap_err()), Some(status), "{headers:?}");
        }
        let mut r = input(b"POST /auth HTTP/1.1\r\nContent-Length: \t 2 \t\r\n\r\n{}GET /_ping HTTP/1.1\n\n");
        assert_eq!(body(&mut r, false).unwrap(), b"{}");
        assert_eq!(read_request(&mut r, |_, _| false).unwrap().unwrap().path, "/_ping");
    }

    #[test]
    fn a_connection_that_fails_is_not_a_refusal() {
        for wire in ["POST /auth HTTP/1.1\r\nContent-Length: 2\r\n\r\n{", "POST /auth HTTP/1.1\r\nContent-Length: 2\r\n", "POST /auth HTTP/1.1\r\nContent-Len"] {
            let mut r = input(wire.as_bytes());
            assert_eq!(refusal(&read_request(&mut r, |_, _| false).unwrap_err()), None, "{wire:?}");
        }
    }

    #[test]
    fn identical_content_lengths_use_one_body() {
        for streamed in [false, true] {
            let mut r = input(b"POST /auth HTTP/1.1\r\nContent-Length: 2\r\ncontent-length: 2\r\n\r\n{}GET /_ping HTTP/1.1\r\n\r\n");
            assert_eq!(body(&mut r, streamed).unwrap(), b"{}");
            assert_eq!(read_request(&mut r, |_, _| false).unwrap().unwrap().path, "/_ping");
        }
    }

    #[test]
    fn rejects_broken_chunk_delimiters_and_sizes() {
        for streamed in [false, true] {
            for wire in [
                "1\r\nxXX0\r\n\r\n", "1\r\nx\n0\r\n\r\n", "0\r\nX: y\r\n", "0\r\n\n", "1\r\n", "0\r\n",
                // The size line: CRLF, hex digits, nothing around them.
                "1\nx\r\n0\r\n\r\n", "1\r\r\nx\r\n0\r\n\r\n", "1 \r\nx\r\n0\r\n\r\n", "1\t\r\nx\r\n0\r\n\r\n", " 1\r\nx\r\n0\r\n\r\n",
                "1 ;a=b\r\nx\r\n0\r\n\r\n", "1;a\nb\r\nx\r\n0\r\n\r\n", "+1\r\nx\r\n0\r\n\r\n", "-1\r\nx\r\n0\r\n\r\n", "0x1\r\nx\r\n0\r\n\r\n",
                "1g\r\nx\r\n0\r\n\r\n", ";a=b\r\nx\r\n0\r\n\r\n", "\r\n0\r\n\r\n", "10000000000000000\r\nx\r\n0\r\n\r\n",
            ] {
                let mut r = input(format!("POST /auth HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{wire}").as_bytes());
                assert!(body(&mut r, streamed).is_err(), "{wire:?}");
            }
        }
    }

    #[test]
    fn a_broken_chunk_is_answered_400() {
        for wire in ["1\nx\r\n0\r\n\r\n", "1 \r\nx\r\n0\r\n\r\n", "g\r\n", "1\r\nxXX", "0\r\n\n"] {
            let mut r = input(format!("POST /auth HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{wire}").as_bytes());
            assert_eq!(refusal(&body(&mut r, false).unwrap_err()), Some(400), "{wire:?}");
        }
    }

    #[test]
    fn bounds_chunk_size_lines_and_trailers() {
        for streamed in [false, true] {
            for wire in [
                format!("1;{}\r\nx\r\n0\r\n\r\n", "x".repeat(MAX_CHUNK_LINE as usize)),
                format!("0\r\nX: {}\r\n\r\n", "x".repeat(MAX_HEAD as usize)),
            ] {
                let mut r = input(format!("POST /auth HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{wire}").as_bytes());
                assert_eq!(refusal(&body(&mut r, streamed).unwrap_err()), Some(400));
            }
            // The longest size line that fits: `1;` + extension + CRLF.
            let mut r = input(format!("POST /auth HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1;{}\r\nx\r\n0\r\n\r\n", "x".repeat(MAX_CHUNK_LINE as usize - 4)).as_bytes());
            assert_eq!(body(&mut r, streamed).unwrap(), b"x");
        }
    }

    #[test]
    fn buffered_body_limit_covers_chunked_requests() {
        let payload = "x".repeat(MAX_BODY + 1);
        let mut r = input(format!("POST /auth HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{payload}\r\n0\r\n\r\n", payload.len()).as_bytes());
        assert_eq!(refusal(&body(&mut r, false).unwrap_err()), Some(413));
    }

    #[test]
    fn content_length_limit_does_not_limit_streamed_archives() {
        let head = format!("POST /images/load HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert!(read_request(&mut input(head.as_bytes()), |_, _| true).is_ok());
        assert_eq!(refusal(&read_request(&mut input(head.as_bytes()), |_, _| false).unwrap_err()), Some(413));
    }

    /// What the client was sent once the server side is done with `r`.
    fn sent(mut client: UnixStream, r: BufReader<UnixStream>) -> Vec<u8> {
        drop(r);
        let mut answer = Vec::new();
        client.read_to_end(&mut answer).unwrap();
        answer
    }

    #[test]
    fn refused_requests_get_no_interim_response() {
        for (streamed, headers, status) in [
            (false, format!("Content-Length: {}", MAX_BODY + 1), 413),
            (false, "Content-Length: 1\r\nContent-Length: 2".to_owned(), 400),
            (true, "Content-Length: 1\r\nTransfer-Encoding: chunked".to_owned(), 400),
            (true, "Transfer-Encoding: gzip".to_owned(), 501),
        ] {
            let (mut client, server) = UnixStream::pair().unwrap();
            client.write_all(format!("POST /auth HTTP/1.1\r\n{headers}\r\nExpect: 100-continue\r\n\r\n").as_bytes()).unwrap();
            let mut r = BufReader::new(server);
            assert_eq!(refusal(&read_request(&mut r, |_, _| streamed).unwrap_err()), Some(status), "{headers}");
            // Nothing before the status `serve` answers with.
            assert!(sent(client, r).is_empty(), "{headers}");
        }
    }

    #[test]
    fn streamed_uploads_continue_on_the_first_read_only() {
        let upload = b"PUT /containers/c/archive HTTP/1.1\r\nContent-Length: 2\r\nExpect: 100-continue\r\n\r\n{}";
        // A handler that answers without reading the body: no interim response.
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(upload).unwrap();
        let mut r = BufReader::new(server);
        let req = read_request(&mut r, |_, _| true).unwrap().unwrap();
        let _ = Body::new(&req, &mut r);
        assert!(sent(client, r).is_empty());
        // One that reads it, or says so before it answers: exactly one.
        for early in [false, true] {
            let (mut client, server) = UnixStream::pair().unwrap();
            client.write_all(upload).unwrap();
            let mut r = BufReader::new(server);
            let req = read_request(&mut r, |_, _| true).unwrap().unwrap();
            if early {
                req.accept_body(r.get_mut()).unwrap();
            }
            let mut out = Vec::new();
            Body::new(&req, &mut r).read_to_end(&mut out).unwrap();
            assert_eq!(out, b"{}");
            assert_eq!(sent(client, r), b"HTTP/1.1 100 Continue\r\n\r\n");
        }
    }

    #[test]
    fn expect_continue_unblocks_fixed_and_chunked_uploads() {
        for streamed in [false, true] {
            for (header, wire) in [("Content-Length: 2", &b"{}"[..]), ("Transfer-Encoding: chunked", &b"2\r\n{}\r\n0\r\n\r\n"[..])] {
                let (mut client, server) = UnixStream::pair().unwrap();
                client.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
                let worker = std::thread::spawn(move || body(&mut BufReader::new(server), streamed));
                client.write_all(format!("POST /auth HTTP/1.1\r\n{header}\r\nExpect: 100-continue\r\n\r\n").as_bytes()).unwrap();
                let mut interim = [0; 25];
                client.read_exact(&mut interim).unwrap();
                assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
                client.write_all(wire).unwrap();
                assert_eq!(worker.join().unwrap().unwrap(), b"{}");
            }
        }
    }

    #[test]
    fn empty_requests_do_not_receive_an_interim_response() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"POST /auth HTTP/1.1\r\nContent-Length: 0\r\nExpect: 100-continue\r\n\r\n").unwrap();
        let mut r = BufReader::new(server);
        assert!(read_request(&mut r, |_, _| false).unwrap().unwrap().body.is_empty());
        r.get_mut().shutdown(std::net::Shutdown::Write).unwrap();
        let mut answer = Vec::new();
        client.read_to_end(&mut answer).unwrap();
        assert!(answer.is_empty());
    }

    #[test]
    fn raw_stream_respects_requested_upgrade() {
        for (headers, upgraded) in [
            (vec![], false),
            (vec![("Connection", "Upgrade")], false),
            (vec![("Upgrade", "tcp")], true),
            (vec![("upgrade", "other")], true),
            (vec![("Upgrade", "")], true),
            (vec![("Upgrade", "TCP"), ("Connection", "keep-alive, Upgrade")], true),
            (vec![("Upgrade", "tcp"), ("Connection", "keep-alive"), ("Connection", "Upgrade")], true),
        ] {
            let mut req = Request::internal("POST", "/containers/id/attach", &[], Vec::new());
            req.headers = headers.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
            let mut out = Vec::new();
            raw_stream(&req, &mut out).unwrap();
            let response = String::from_utf8(out).unwrap();
            assert!(response.starts_with(if upgraded { "HTTP/1.1 101 " } else { "HTTP/1.1 200 " }), "{response}");
            assert!(response.contains("Content-Type: application/vnd.docker.raw-stream\r\n"));
            assert!(response.contains(if upgraded { "Connection: Upgrade\r\nUpgrade: tcp\r\n" } else { "Connection: close\r\n" }));
            assert!(!response.contains("Content-Length:"));
            assert!(!response.contains("Transfer-Encoding:"));
            assert!(response.ends_with("\r\n\r\n"));
        }
    }

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
        assert_eq!(percent_decode("docker.io%2Flibrary%2Falpine", true), "docker.io/library/alpine");
        assert_eq!(percent_decode("a+b", true), "a b");
        assert_eq!(percent_decode("100%", true), "100%");
        assert_eq!(percent_decode("%7B%22a%22%3A1%7D", true), "{\"a\":1}");
    }

    #[test]
    fn paths_decode_escapes_without_form_plus_conversion() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"GET /v1.47/images/registry%3A5000%2Frepository:tag+suffix/json?name=a+b HTTP/1.1\r\n\r\n").unwrap();
        let request = read_request(&mut BufReader::new(server), |_, _| false).unwrap().unwrap();
        assert_eq!(request.path, "/images/registry:5000/repository:tag+suffix/json");
        assert_eq!(request.query("name"), Some("a b"));
        assert_eq!(percent_decode("/%E2%82%AC+%2B", false), "/€++");
        assert_eq!(percent_decode("/%FF", false), "/\u{fffd}");
    }

    #[test]
    fn version_prefix_is_stripped_after_decoding() {
        for (target, path) in [
            ("/v1%2E47/_ping", "/_ping"),
            ("/%761.47/_ping", "/_ping"),
            ("/v1.47%2Fcontainers/web/json", "/containers/web/json"),
            ("/v1.47/%5fping", "/_ping"),
            ("/v1.47/containers/a%252Fb/json", "/containers/a%2Fb/json"),
            ("/images/v1.2%2Fapp/json", "/images/v1.2/app/json"),
        ] {
            let (mut client, server) = UnixStream::pair().unwrap();
            client.write_all(format!("GET {target} HTTP/1.1\r\n\r\n").as_bytes()).unwrap();
            let request = read_request(&mut BufReader::new(server), |_, _| false).unwrap().unwrap();
            assert_eq!(request.path, path, "{target}");
        }
    }

    #[test]
    fn malformed_path_escapes_are_a_bad_request() {
        for target in ["/containers/%zz/json", "/containers/web%/json", "/networks/net%a", "/v1.47/%4", "/v1%2/_ping"] {
            let (mut client, server) = UnixStream::pair().unwrap();
            client.write_all(format!("GET {target}?name=100% HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes()).unwrap();
            let mut reader = BufReader::new(server);
            assert!(read_request(&mut reader, |_, _| false).unwrap().is_none(), "{target}");
            drop(reader);
            let mut reply = String::new();
            client.read_to_string(&mut reply).unwrap();
            assert!(reply.starts_with("HTTP/1.1 400 Bad Request\r\n") && reply.contains("\r\nConnection: close\r\n") && reply.ends_with("\r\n\r\n400 Bad Request"), "{target}: {reply}");
        }
        // A query keeps its lenient decoding.
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"GET /info?name=100% HTTP/1.1\r\n\r\n").unwrap();
        let request = read_request(&mut BufReader::new(server), |_, _| false).unwrap().unwrap();
        assert_eq!(request.query("name"), Some("100%"));
    }
}
