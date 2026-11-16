//! Short local health probes with one deadline for connect, headers and body.
//! A stuck or slowly answering engine must not hold an update worker forever.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "engine health probe timed out")
}

fn wait(stream: &UnixStream, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.checked_duration_since(Instant::now()).ok_or_else(timed_out)?;
        let millis = remaining.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd { fd: stream.as_raw_fd(), events, revents: 0 };
        // SAFETY: descriptor is one initialized pollfd, valid for this call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready > 0 {
            if descriptor.revents & libc::POLLNVAL != 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid engine socket"));
            }
            return Ok(());
        }
        if ready == 0 {
            return Err(timed_out());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub(super) fn connect(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    if Instant::now() >= deadline {
        return Err(timed_out());
    }
    let name = path.as_os_str().as_bytes();
    // SAFETY: sockaddr_un consists entirely of integer fields and arrays.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if name.is_empty() || name.contains(&0) || name.len() >= address.sun_path.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid engine socket path"));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (to, from) in address.sun_path.iter_mut().zip(name) {
        *to = *from as libc::c_char;
    }
    let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + name.len() + 1) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    { address.sun_len = length as u8; }
    #[cfg(target_os = "linux")]
    let flags = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    #[cfg(not(target_os = "linux"))]
    let flags = libc::SOCK_STREAM;
    // SAFETY: a local stream socket; no pointers or inherited descriptor.
    let descriptor = unsafe { libc::socket(libc::AF_UNIX, flags, 0) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ownership of the freshly returned descriptor transfers here.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    #[cfg(not(target_os = "linux"))]
    {
        // Darwin has no SOCK_CLOEXEC/SOCK_NONBLOCK socket flags.
        // SAFETY: both fcntl operations use the owned, open descriptor.
        if unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0
            || unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(target_os = "macos")]
    {
        let enabled: libc::c_int = 1;
        // SAFETY: enabled is an int-sized option value for this open socket.
        if unsafe { libc::setsockopt(descriptor.as_raw_fd(), libc::SOL_SOCKET, libc::SO_NOSIGPIPE,
            (&enabled as *const libc::c_int).cast(), std::mem::size_of_val(&enabled) as _) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let stream = UnixStream::from(descriptor);
    // SAFETY: address is initialized and length fits its pathname and storage.
    if unsafe { libc::connect(stream.as_raw_fd(), (&address as *const libc::sockaddr_un).cast(), length) } < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        wait(&stream, libc::POLLOUT, deadline)?;
        if let Some(error) = stream.take_error()? {
            return Err(error);
        }
    }
    Ok(stream)
}

struct Connection {
    stream: UnixStream,
    deadline: Instant,
}

impl Read for Connection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if Instant::now() >= self.deadline { return Err(timed_out()); }
            match self.stream.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => wait(&self.stream, libc::POLLIN, self.deadline)?,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            if Instant::now() >= self.deadline { return Err(timed_out()); }
            match self.stream.write(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => wait(&self.stream, libc::POLLOUT, self.deadline)?,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

pub(super) fn request_at(socket: &Path, path: &str, limit: Duration) -> io::Result<super::Response> {
    if !path.starts_with('/') || !path.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid health probe path"));
    }
    let deadline = Instant::now().checked_add(limit).ok_or_else(timed_out)?;
    let mut connection = Connection { stream: connect(socket, deadline)?, deadline };
    write!(connection, "GET {path} HTTP/1.1\r\nHost: asagiri\r\nConnection: close\r\nX-Asagiri-Background: 1\r\n\r\n")?;
    super::health_response::read(connection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = PathBuf::from("/tmp").join(format!("asagiri-probe-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    fn server(write: impl FnOnce(UnixStream, mpsc::Receiver<()>) + Send + 'static)
        -> (Fixture, PathBuf, mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let fixture = Fixture::new();
        let path = fixture.0.join("sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (release, released) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => { write(stream, released); break; }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("test listener: {error}"),
                }
            }
        });
        (fixture, path, release, thread)
    }

    #[test]
    fn version_probe_finishes_without_waiting_for_the_engine_to_close() {
        let (_fixture, path, release, thread) = server(|mut stream, released| {
            let body = br#"{"AsagiriVersion":"0.1.6","Version":"27.5.1-asagiri"}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
            stream.write_all(body).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(2));
        });
        let response = request_at(&path, "/version", Duration::from_millis(250));
        let _ = release.send(());
        thread.join().unwrap();
        assert_eq!(super::super::engine_version(&response.unwrap().json()), Some("0.1.6"));
    }

    #[test]
    fn stalled_engine_obeys_the_entire_probe_deadline() {
        let (_fixture, path, release, thread) = server(|_stream, released| {
            let _ = released.recv_timeout(Duration::from_secs(2));
        });
        let began = Instant::now();
        let response = request_at(&path, "/version", Duration::from_millis(60));
        let elapsed = began.elapsed();
        let _ = release.send(());
        thread.join().unwrap();
        assert_eq!(response.err().unwrap().kind(), io::ErrorKind::TimedOut);
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }

    #[test]
    fn slow_headers_do_not_renew_the_deadline() {
        let (_fixture, path, release, thread) = server(|mut stream, _| {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nX-Slow: ");
            for _ in 0..80 {
                if stream.write_all(b"a").is_err() { break; }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let began = Instant::now();
        let response = request_at(&path, "/version", Duration::from_millis(60));
        let elapsed = began.elapsed();
        let _ = release.send(());
        thread.join().unwrap();
        assert_eq!(response.err().unwrap().kind(), io::ErrorKind::TimedOut);
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }

    #[test]
    fn health_probes_decode_chunks_and_reject_unbounded_bodies() {
        for (wire, valid) in [
            (&b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nOK\r\n0\r\n\r\n"[..], true),
            (&b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n"[..], false),
            (&b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\n"[..], false),
            (&b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nOKxx0\r\n\r\n"[..], false),
        ] {
            let (_fixture, path, release, thread) = server(move |mut stream, released| {
                let _ = stream.write_all(wire);
                let _ = released.recv_timeout(Duration::from_secs(2));
            });
            let response = request_at(&path, "/_ping", Duration::from_millis(250));
            let _ = release.send(());
            thread.join().unwrap();
            if valid {
                assert_eq!(response.unwrap().body, b"OK");
            } else {
                assert_eq!(response.err().unwrap().kind(), io::ErrorKind::InvalidData);
            }
        }
    }

    #[test]
    fn empty_deadlines_and_invalid_paths_fail_before_connecting() {
        let path = Path::new("/tmp/asagiri-probe-absent");
        assert_eq!(request_at(path, "/version", Duration::ZERO).err().unwrap().kind(), io::ErrorKind::TimedOut);
        assert_eq!(request_at(path, "/version\r\nExtra: value", Duration::from_secs(1)).err().unwrap().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(connect(Path::new(""), Instant::now() + Duration::from_secs(1)).err().unwrap().kind(), io::ErrorKind::InvalidInput);
    }
}
