//! Identify the engine through its socket and recheck its UID, name and start
//! time before each signal. Stop signalling when that recorded identity changes.

use std::ffi::OsStr;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    pid: libc::pid_t,
    uid: libc::uid_t,
    started: (u64, u64),
    name: Vec<u8>,
}

fn information(pid: libc::pid_t) -> io::Result<Option<libc::proc_bsdinfo>> {
    let mut value = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: value points to size bytes of writable proc_bsdinfo storage.
    let read = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, value.as_mut_ptr().cast(), size) };
    if read == size {
        // SAFETY: proc_pidinfo initialized exactly the expected structure.
        return Ok(Some(unsafe { value.assume_init() }));
    }
    let error = io::Error::last_os_error();
    if read == 0 && matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)) {
        return Ok(None);
    }
    Err(if read > 0 {
        io::Error::new(io::ErrorKind::InvalidData, "incomplete engine process identity")
    } else { error })
}

fn identity(info: &libc::proc_bsdinfo) -> Identity {
    let name = if info.pbi_name[0] != 0 { &info.pbi_name[..] } else { &info.pbi_comm[..] };
    Identity {
        pid: info.pbi_pid as libc::pid_t, uid: info.pbi_uid,
        started: (info.pbi_start_tvsec, info.pbi_start_tvusec),
        name: name.iter().take_while(|byte| **byte != 0).map(|byte| *byte as u8).collect(),
    }
}

/// Darwin's public sys/proc.h process status. A zombie has exited even when
/// its original UI parent has not reaped it yet; kill(pid, 0) still succeeds.
const ZOMBIE: u32 = 5;

pub(super) struct Engine {
    identity: Identity,
    // Retain the original connection while stopping its identified peer.
    _connection: UnixStream,
}

impl Engine {
    pub(super) fn connect() -> io::Result<Option<Self>> {
        let connection = match crate::client::probe_connection(Duration::from_secs(1)) {
            Ok(connection) => connection,
            Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => return Ok(None),
            Err(error) => return Err(error),
        };
        Self::from_socket(connection, &[OsStr::new("asagiri"), OsStr::new("asagiri-dev")])
    }

    fn from_socket(connection: UnixStream, names: &[&OsStr]) -> io::Result<Option<Self>> {
        let mut pid: libc::pid_t = 0;
        let mut length = std::mem::size_of_val(&pid) as libc::socklen_t;
        // SAFETY: the socket is open, and pid/length are initialized buffers.
        let read = unsafe { libc::getsockopt(connection.as_raw_fd(), libc::SOL_LOCAL, libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(), &mut length) };
        if read != 0 { return Err(io::Error::last_os_error()); }
        if length as usize != std::mem::size_of_val(&pid) || pid <= 1 || pid as u32 == std::process::id() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the engine socket has an unexpected process"));
        }
        let Some(info) = information(pid)? else { return Ok(None) };
        let observed = identity(&info);
        // Kernel process names survive an app replacement/unlink, unlike
        // proc_pidpath. UID and creation time bind later checks to this process.
        // SAFETY: geteuid has no preconditions.
        if observed.pid != pid || observed.uid != unsafe { libc::geteuid() }
            || observed.started.0 == 0 || !names.iter().any(|name| name.as_bytes() == observed.name) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the engine socket is not served by this user's Asagiri"));
        }
        if info.pbi_status == ZOMBIE { return Ok(None); }
        Ok(Some(Self { identity: observed, _connection: connection }))
    }

    pub(super) fn pid(&self) -> libc::pid_t { self.identity.pid }

    pub(super) fn running(&self) -> io::Result<bool> {
        Ok(information(self.pid())?.is_some_and(|info| info.pbi_status != ZOMBIE && identity(&info) == self.identity))
    }

    fn signal(&self, signal: libc::c_int) -> io::Result<bool> {
        if !self.running()? { return Ok(false); }
        // SAFETY: pid is positive and was obtained from this socket's peer.
        // Darwin kill still takes a PID, not an atomic process handle; check
        // UID, name and start time immediately before every signal.
        if unsafe { libc::kill(self.pid(), signal) } == 0 { return Ok(true); }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) { Ok(false) } else { Err(error) }
    }

    pub(super) fn stop(&self) -> io::Result<()> {
        self.stop_with(&[(libc::SIGTERM, Duration::from_secs(30)), (libc::SIGKILL, Duration::from_secs(5))])
    }

    fn stop_with(&self, signals: &[(libc::c_int, Duration)]) -> io::Result<()> {
        for (signal, limit) in signals {
            if !self.signal(*signal)? { return Ok(()); }
            let deadline = Instant::now() + *limit;
            loop {
                if !self.running()? { return Ok(()); }
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else { break };
                std::thread::sleep(remaining.min(Duration::from_millis(50)));
            }
        }
        Err(io::Error::other(format!("the engine (process {}) did not stop", self.pid())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture { directory: PathBuf, child: Child }
    impl Fixture {
        fn new(ignore_term: bool) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = PathBuf::from("/tmp").join(format!("asagiri-peer-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::DirBuilder::new().mode(0o700).create(&directory).unwrap();
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "update::process::tests::owned_peer_helper", "--nocapture"])
                .env("ASAGIRI_TEST_PEER_SOCKET", directory.join("sock"))
                .env("ASAGIRI_TEST_PEER_IGNORE_TERM", if ignore_term { "1" } else { "0" })
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            Self { directory, child }
        }

        fn peer(&mut self) -> Engine {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if let Ok(connection) = UnixStream::connect(self.directory.join("sock")) {
                    let executable = std::env::current_exe().unwrap();
                    return Engine::from_socket(connection, &[executable.file_name().unwrap()]).unwrap().unwrap();
                }
                assert!(self.child.try_wait().unwrap().is_none(), "owned test peer exited before connecting");
                assert!(Instant::now() < deadline, "owned test peer did not start");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() { let _ = self.child.kill(); }
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn owned_peer_helper() {
        let Some(socket) = std::env::var_os("ASAGIRI_TEST_PEER_SOCKET") else { return };
        if std::env::var("ASAGIRI_TEST_PEER_IGNORE_TERM").as_deref() == Ok("1") {
            // SAFETY: this isolated test child explicitly ignores only SIGTERM.
            unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN); }
        }
        let listener = UnixListener::bind(socket).unwrap();
        let (_connection, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_secs(10));
    }

    #[test]
    fn stops_only_the_observed_owned_socket_peer_and_recognizes_its_exit() {
        let mut fixture = Fixture::new(false);
        let peer = fixture.peer();
        assert_eq!(peer.pid() as u32, fixture.child.id());
        peer.stop_with(&[(libc::SIGTERM, Duration::from_secs(1))]).unwrap();
        // A direct UI child may remain a zombie until wait(); it still stopped.
        assert!(!peer.running().unwrap());
        fixture.child.wait().unwrap();
    }

    #[test]
    fn escalation_rechecks_the_owned_process_identity() {
        let mut fixture = Fixture::new(true);
        let peer = fixture.peer();
        peer.stop_with(&[(libc::SIGTERM, Duration::from_millis(30)), (libc::SIGKILL, Duration::from_secs(1))]).unwrap();
        let status = fixture.child.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn a_changed_start_time_or_uid_does_not_authorize_signalling() {
        let mut fixture = Fixture::new(false);
        let mut peer = fixture.peer();
        peer.identity.started.1 += 1;
        assert!(!peer.signal(libc::SIGTERM).unwrap());
        assert!(fixture.child.try_wait().unwrap().is_none());
        peer.identity.started.1 -= 1;
        peer.identity.uid = peer.identity.uid.wrapping_add(1);
        assert!(!peer.signal(libc::SIGTERM).unwrap());
        assert!(fixture.child.try_wait().unwrap().is_none());
    }

    #[test]
    fn rejects_an_unrelated_socket_peer_without_signalling_it() {
        let mut fixture = Fixture::new(false);
        let _peer = fixture.peer();
        let connection = UnixStream::connect(fixture.directory.join("sock")).unwrap();
        assert!(Engine::from_socket(connection, &[OsStr::new("not-this-test-peer")]).is_err());
        assert!(fixture.child.try_wait().unwrap().is_none());
    }
}
