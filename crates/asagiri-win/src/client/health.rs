//! Update checks must finish even if the local engine stops reading or replying.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use crate::pipe::Pipe;

struct Connection { pipe: Pipe, deadline: Instant }

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "engine health probe timed out")
}

impl Connection {
    fn remaining(&self) -> io::Result<Duration> {
        self.deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero()).ok_or_else(timed_out)
    }
}

impl Read for Connection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.pipe.read_client(buffer, Some(self.remaining()?))
    }
}

impl Write for Connection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.pipe.write_client_within(buffer, self.remaining()?)
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

pub(super) fn request_at(pipe: &str, path: &str, limit: Duration) -> io::Result<super::Response> {
    if !path.starts_with('/') || !path.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid health probe path"));
    }
    let deadline = Instant::now().checked_add(limit).ok_or_else(timed_out)?;
    let mut connection = Connection { pipe: Pipe::open_within(pipe, deadline)?, deadline };
    write!(connection, "GET {path} HTTP/1.1\r\nHost: asagiri\r\nConnection: close\r\nX-Asagiri-Background: 1\r\n\r\n")?;
    super::health_response::read(connection)
}

/// Share one deadline across connection attempts, responses and retry sleeps.
pub(super) fn wait_for_ready(pipe: &str, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if request_at(pipe, "/_ping", remaining).is_ok_and(|reply| reply.ok()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::{AtomicU64, Ordering}, mpsc};

    fn server(reply: impl FnOnce(Pipe, mpsc::Receiver<()>) + Send + 'static)
        -> (String, mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(r"\\.\pipe\asagiri-health-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let pipe = Pipe::create(&name, true).unwrap();
        let (release, released) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            pipe.connect_within(Some(Duration::from_secs(3))).unwrap();
            reply(pipe, released);
        });
        (name, release, thread)
    }

    #[test]
    fn version_probe_finishes_without_waiting_for_the_pipe_to_close() {
        let (name, release, thread) = server(|pipe, released| {
            pipe.write_client_within(b"HTTP/1.1 200 OK\r\nContent-Length: 26\r\n\r\n{\"AsagiriVersion\":\"0.1.6\"}", Duration::from_secs(1)).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(2));
        });
        let response = request_at(&name, "/version", Duration::from_millis(500));
        let _ = release.send(());
        thread.join().unwrap();
        assert_eq!(super::super::engine_version(&response.unwrap().json()), Some("0.1.6"));
    }

    #[test]
    fn stalled_pipe_does_not_hold_the_update_worker() {
        let (name, release, thread) = server(|_pipe, released| {
            let _ = released.recv_timeout(Duration::from_secs(2));
        });
        let began = Instant::now();
        let response = request_at(&name, "/version", Duration::from_millis(75));
        let elapsed = began.elapsed();
        let _ = release.send(());
        thread.join().unwrap();
        assert_eq!(response.err().unwrap().kind(), io::ErrorKind::TimedOut);
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }

    #[test]
    fn readiness_recovers_when_the_second_ping_succeeds() {
        let name = format!(r"\\.\pipe\asagiri-ready-retry-{}", std::process::id());
        let first = Pipe::create(&name, true).unwrap();
        let thread = std::thread::spawn({
            let name = name.clone();
            move || {
                let limit = Duration::from_secs(3);
                first.connect_within(Some(limit)).unwrap();
                let second = Pipe::create(&name, false).unwrap();
                first.write_client_within(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\n\r\n", limit).unwrap();
                drop(first);
                second.connect_within(Some(limit)).unwrap();
                second.write_client_within(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK", limit).unwrap();
            }
        });
        let ready = wait_for_ready(&name, Duration::from_secs(3));
        thread.join().unwrap();
        assert!(ready);
    }

    #[test]
    fn readiness_retries_share_the_stalled_pipe_deadline() {
        let (name, release, thread) = server(|_pipe, released| {
            let _ = released.recv_timeout(Duration::from_secs(3));
        });
        let began = Instant::now();
        let ready = wait_for_ready(&name, Duration::from_millis(150));
        let elapsed = began.elapsed();
        drop(release);
        thread.join().unwrap();
        assert!(!ready);
        assert!(elapsed >= Duration::from_millis(140) && elapsed < Duration::from_secs(2), "{elapsed:?}");
    }

    #[test]
    fn slow_pipe_headers_do_not_renew_the_deadline() {
        let (name, release, thread) = server(|pipe, _| {
            let _ = pipe.write_client_within(b"HTTP/1.1 200 OK\r\nX-Slow: ", Duration::from_secs(1));
            for _ in 0..80 {
                if pipe.write_client_within(b"a", Duration::from_millis(100)).is_err() { break; }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let began = Instant::now();
        let response = request_at(&name, "/version", Duration::from_millis(75));
        let elapsed = began.elapsed();
        let _ = release.send(());
        thread.join().unwrap();
        assert_eq!(response.err().unwrap().kind(), io::ErrorKind::TimedOut);
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }

    #[test]
    fn empty_deadlines_and_invalid_paths_fail_before_opening_a_pipe() {
        let absent = r"\\.\pipe\asagiri-health-test-absent";
        assert_eq!(request_at(absent, "/version", Duration::ZERO).err().unwrap().kind(), io::ErrorKind::TimedOut);
        assert_eq!(request_at(absent, "/version\r\nOther: 1", Duration::from_secs(1)).err().unwrap().kind(), io::ErrorKind::InvalidInput);
    }
}
