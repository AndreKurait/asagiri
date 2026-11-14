//! Bounded, cancellable requests over the native local engine connection.
//! No TCP endpoint, proxy environment, redirects, or automatic engine start.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
#[path = "../../asagiri-win/src/pipe.rs"]
#[allow(dead_code)]
mod pipe;

const HEADER_LIMIT: usize = 16 * 1024;
pub const JSON_LIMIT: usize = 4 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(200);

#[derive(Debug)]
enum Abort {
    #[cfg(unix)]
    Socket(UnixStream),
    #[cfg(windows)]
    Pipe(Arc<pipe::Pipe>),
}

impl Abort {
    fn cancel(&self) {
        match self {
            #[cfg(unix)]
            Self::Socket(stream) => { let _ = stream.shutdown(std::net::Shutdown::Both); }
            #[cfg(windows)]
            Self::Pipe(pipe) => pipe.cancel(),
        }
    }
}

#[derive(Debug)]
pub struct Control {
    cancelled: AtomicBool,
    deadline: Instant,
    connection: Mutex<Option<Abort>>,
}

impl Control {
    pub fn new(timeout: Duration) -> Arc<Self> {
        Arc::new(Self { cancelled: AtomicBool::new(false), deadline: Instant::now() + timeout, connection: Mutex::new(None) })
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(connection) = &*self.connection.lock().unwrap_or_else(|e| e.into_inner()) { connection.cancel(); }
    }

    pub fn check(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            // Buffered std readers retry Interrupted automatically. An
            // explicit cancellation must terminate those reads instead.
            Err(io::Error::new(io::ErrorKind::ConnectionAborted, "request cancelled"))
        } else if Instant::now() >= self.deadline {
            Err(io::Error::new(io::ErrorKind::TimedOut, "engine request timed out"))
        } else { Ok(()) }
    }

    pub fn expired(&self) -> bool { Instant::now() >= self.deadline }
    fn remaining(&self) -> Duration { self.deadline.saturating_duration_since(Instant::now()) }
}

#[derive(Clone, Debug)]
pub struct Client {
    #[cfg(unix)]
    path: std::path::PathBuf,
    #[cfg(windows)]
    path: String,
}

impl Client {
    pub fn current() -> io::Result<Self> {
        #[cfg(unix)]
        let path = crate::client::socket()?;
        #[cfg(windows)]
        let path = std::env::var("ASAGIRI_PIPE").unwrap_or_else(|_| r"\\.\pipe\asagiri".into());
        #[cfg(windows)]
        if !path.starts_with(r"\\.\pipe\") || path.contains(['\0', '\r', '\n']) {
            return Err(io::Error::other("ASAGIRI_PIPE must name a local Windows pipe"));
        }
        Ok(Self { path })
    }

    #[cfg(all(test, unix))]
    pub fn at(path: std::path::PathBuf) -> Self { Self { path } }

    pub fn request(&self, method: &str, path: &str, control: Arc<Control>, limit: usize, truncate: bool) -> io::Result<Response> {
        if !matches!(method, "GET" | "POST") || !path.starts_with('/')
            || path.bytes().any(|b| b <= 0x20 || b >= 0x7f)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid local API request"));
        }
        control.check()?;
        #[cfg(unix)]
        let stream = {
            let stream = connect_socket(&self.path, &control)?;
            stream.set_read_timeout(Some(POLL))?;
            stream.set_write_timeout(Some(POLL))?;
            stream
        };
        #[cfg(windows)]
        let stream = {
            let stream = Arc::new(pipe::Pipe::open(&self.path)?);
            *control.connection.lock().unwrap_or_else(|e| e.into_inner()) = Some(Abort::Pipe(stream.clone()));
            stream
        };
        let mut connection = Connection { stream, control };
        connection.control.check()?;
        let background = if method == "GET" { "X-Asagiri-Background: 1\r\n" } else { "" };
        write!(connection, "{method} {path} HTTP/1.1\r\nHost: asagiri\r\nConnection: close\r\n{background}Content-Length: 0\r\n\r\n")?;
        let response = read_response(BufReader::new(&mut connection), limit, truncate)?;
        connection.control.check()?;
        Ok(response)
    }
}

#[cfg(unix)]
fn connect_socket(path: &std::path::Path, control: &Control) -> io::Result<UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: initialized below; sockaddr_un contains integers and bytes.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid engine socket path"));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (target, source) in address.sun_path.iter_mut().zip(bytes) { *target = *source as _; }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    #[cfg(target_os = "macos")]
    { address.sun_len = length as _; }
    // SAFETY: socket returns a new descriptor, immediately given one owner.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: fd is a new socket and has no other owner.
    let stream = UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
    // SAFETY: fd is the socket we own; FD_CLOEXEC prevents inheritance.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 { return Err(io::Error::last_os_error()); }
    stream.set_nonblocking(true)?;
    *control.connection.lock().unwrap_or_else(|e| e.into_inner()) = Some(Abort::Socket(stream.try_clone()?));
    let connect = (|| {
        loop {
            control.check()?;
            // SAFETY: address is initialized and length fits sockaddr_un.
            if unsafe { libc::connect(stream.as_raw_fd(), (&address as *const libc::sockaddr_un).cast(), length as _) } == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EAGAIN) => { std::thread::sleep(POLL.min(control.remaining())); continue; }
                Some(libc::EINPROGRESS | libc::EALREADY) => {}
                Some(libc::EISCONN) => return Ok(()),
                Some(libc::EINTR) => continue,
                _ => return Err(error),
            }
            loop {
                control.check()?;
                let mut poll = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
                // SAFETY: poll is a valid descriptor record; bounded timeout.
                let ready = unsafe { libc::poll(&mut poll, 1, POLL.min(control.remaining()).as_millis() as _) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted { continue; }
                    return Err(error);
                }
                if ready == 0 { continue; }
                let mut error = 0;
                let mut size = std::mem::size_of_val(&error) as libc::socklen_t;
                // SAFETY: getsockopt writes only to our integer and length.
                if unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ERROR, (&mut error as *mut i32).cast(), &mut size) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                return if error == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(error)) };
            }
        }
    })();
    if let Err(error) = connect {
        control.connection.lock().unwrap_or_else(|e| e.into_inner()).take();
        return Err(error);
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

struct Connection {
    #[cfg(unix)]
    stream: UnixStream,
    #[cfg(windows)]
    stream: Arc<pipe::Pipe>,
    control: Arc<Control>,
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.control.check()?;
            #[cfg(unix)]
            let result = self.stream.read(buf);
            #[cfg(windows)]
            let result = self.stream.read_client(buf, Some(POLL));
            match result {
                Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                result => return result,
            }
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            self.control.check()?;
            #[cfg(unix)]
            let result = self.stream.write(buf);
            #[cfg(windows)]
            let result = self.stream.write_client_within(buf, self.control.remaining());
            match result {
                Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.control.connection.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    pub truncated: bool,
}

fn bad(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

fn line(reader: &mut impl BufRead, remaining: &mut usize) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader.take(*remaining as u64 + 1).read_until(b'\n', &mut bytes)?;
    if bytes.len() > *remaining || !bytes.ends_with(b"\r\n") {
        return Err(bad("invalid or oversized engine response header"));
    }
    *remaining -= bytes.len();
    bytes.truncate(bytes.len() - 2);
    String::from_utf8(bytes).map_err(|_| bad("invalid engine response header"))
}

fn read_response(mut reader: impl BufRead, limit: usize, truncate: bool) -> io::Result<Response> {
    let mut remaining = HEADER_LIMIT;
    let first = line(&mut reader, &mut remaining)?;
    let mut words = first.split_whitespace();
    if !matches!(words.next(), Some("HTTP/1.1" | "HTTP/1.0")) { return Err(bad("invalid engine HTTP version")); }
    let status: u16 = words.next().and_then(|s| s.parse().ok()).filter(|n| (200..600).contains(n))
        .ok_or_else(|| bad("invalid engine HTTP status"))?;
    let (mut length, mut chunked) = (None, false);
    loop {
        let header = line(&mut reader, &mut remaining)?;
        if header.is_empty() { break; }
        let (key, value) = header.split_once(':').ok_or_else(|| bad("invalid engine response header"))?;
        match key.to_ascii_lowercase().as_str() {
            "content-length" => {
                if length.is_some() { return Err(bad("duplicate engine response length")); }
                length = Some(value.trim().parse::<usize>().map_err(|_| bad("invalid engine response length"))?);
            }
            "transfer-encoding" => {
                if chunked || !value.trim().eq_ignore_ascii_case("chunked") { return Err(bad("unsupported engine response encoding")); }
                chunked = true;
            }
            _ => {}
        }
    }
    if chunked && length.is_some() { return Err(bad("ambiguous engine response framing")); }
    let mut result = Response { status, body: Vec::new(), truncated: false };
    if matches!(status, 204 | 304) { return Ok(result); }
    let mut append = |reader: &mut dyn Read, size: usize| -> io::Result<bool> {
        let left = limit.saturating_sub(result.body.len());
        if size > left && !truncate { return Err(bad("engine response exceeds the integration output limit")); }
        let count = size.min(left);
        let start = result.body.len();
        result.body.resize(start + count, 0);
        reader.read_exact(&mut result.body[start..])?;
        result.truncated = size > left;
        Ok(!result.truncated)
    };
    if chunked {
        loop {
            let mut chunk_header_limit = 1024;
            let header = line(&mut reader, &mut chunk_header_limit)?;
            let size = usize::from_str_radix(header.split(';').next().unwrap_or(""), 16).map_err(|_| bad("invalid engine response chunk"))?;
            if size == 0 {
                let mut trailers = HEADER_LIMIT;
                while !line(&mut reader, &mut trailers)?.is_empty() {}
                break;
            }
            if !append(&mut reader, size)? { break; }
            let mut end = [0; 2];
            reader.read_exact(&mut end)?;
            if end != *b"\r\n" { return Err(bad("invalid engine response chunk end")); }
        }
    } else if let Some(size) = length {
        append(&mut reader, size)?;
    } else {
        reader.take(limit as u64 + 1).read_to_end(&mut result.body)?;
        if result.body.len() > limit {
            if !truncate { return Err(bad("engine response exceeds the integration output limit")); }
            result.body.truncate(limit);
            result.truncated = true;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_http_rejects_ambiguous_oversized_and_incomplete_bodies() {
        for bytes in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}xx0\r\n\r\n",
        ] { assert!(read_response(bytes, 16, false).is_err()); }
        let mut oversized = b"HTTP/1.1 200 OK\r\nX: ".to_vec();
        oversized.extend(vec![b'a'; HEADER_LIMIT]);
        assert!(read_response(oversized.as_slice(), 16, false).is_err());
        let body = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n0\r\n\r\n";
        assert!(read_response(body.as_slice(), 3, false).is_err());
        let short = read_response(body.as_slice(), 3, true).unwrap();
        assert_eq!(short.body, b"abc");
        assert!(short.truncated);
        assert_eq!(read_response(body.as_slice(), 4, false).unwrap().body, b"abcd");
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_closes_an_engine_that_never_answers() {
        use std::os::unix::net::UnixListener;
        let path = std::env::temp_dir().join(format!("asagiri-mcp-cancel-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        let control = Control::new(Duration::from_secs(5));
        let cancel = control.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let client = Client::at(path.clone());
        let worker = std::thread::spawn(move || sent.send(client.request("GET", "/info", control, 1024, false).is_err()).unwrap());
        let (mut peer, _) = listener.accept().unwrap();
        let mut buffer = [0; 1024];
        assert!(peer.read(&mut buffer).unwrap() > 0);
        cancel.cancel();
        assert!(received.recv_timeout(Duration::from_secs(1)).unwrap());
        worker.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_full_socket_backlog_does_not_bypass_the_deadline() {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixListener;
        let path = std::env::temp_dir().join(format!("asagiri-mcp-backlog-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        // SAFETY: reducing the backlog of our private test listener.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let queued = UnixStream::connect(&path).unwrap();
        let control = Control::new(Duration::from_millis(50));
        let started = Instant::now();
        assert_eq!(Client::at(path.clone()).request("GET", "/info", control, 1024, false).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(queued);
        std::fs::remove_file(path).unwrap();
    }
}
