//! Linux: no VM. The guest agent (`asagiri-init`) runs as a root process on the
//! host's kernel, and the vsock ports become unix sockets in a private
//! directory: the agent listens on `g<port>.sock`, the daemon on
//! `h<port>.sock`. Everything above this module is unchanged.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The same fields as on macOS, so callers build one `Config`. On Linux,
/// `initrd` names the agent binary (`asagiri-init`); kernel, disk, shares,
/// memory and network fields do not apply: the host provides them.
#[derive(Debug)]
pub struct Config {
    pub cpus: usize,
    pub memory_mib: u64,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub cmdline: String,
    pub console: Option<OwnedFd>,
    pub agent_port: u32,
    pub disk: Option<PathBuf>,
    pub share: Option<(PathBuf, String)>,
    /// macOS only: the Intel guest's arm64 emulator (see `apple::Config`).
    pub emulator: Option<(PathBuf, String)>,
    pub rosetta_tag: Option<String>,
    pub nat_mac: Option<String>,
}

#[derive(Debug)]
pub enum Event {
    /// The agent connected. The fd is a stream socket owned by the caller.
    GuestConnected(OwnedFd),
    /// The agent exited, with the reason if it failed.
    Stopped(Option<String>),
}

#[derive(Debug)]
pub struct Vm {
    dir: PathBuf,
    child: Arc<Mutex<Child>>,
    events: Receiver<Event>,
    closing: Arc<AtomicBool>,
    listeners: Mutex<Vec<std::thread::JoinHandle<()>>>,
    retired: bool,
}

fn runtime_directory(root: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    root.join(format!("asagiri-agent-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
}

/// Only a newly created private directory may hold the agent's root sockets.
/// A PID can recur after an unclean daemon restart, leaving old names behind.
fn private_runtime_directory(mut next: impl FnMut() -> PathBuf) -> io::Result<PathBuf> {
    for _ in 0..64 {
        let dir = next();
        match std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "could not allocate a fresh agent runtime directory after 64 attempts"))
}

fn accept_until_closed(listener: &UnixListener, closing: &AtomicBool) -> io::Result<Option<UnixStream>> {
    loop {
        if closing.load(Ordering::Acquire) {
            return Ok(None);
        }
        match listener.accept() {
            Ok((stream, _)) => return Ok(Some(stream)),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(e),
        }
    }
}

/// A connection to one of the agent's ports.
#[derive(Debug)]
pub struct VsockStream {
    pub fd: OwnedFd,
}

/// What `Vm::listen` calls for each connection the agent makes.
pub type Handler = Box<dyn Fn(VsockStream) + Send + Sync>;

/// Rosetta is a macOS feature.
pub fn rosetta_installed() -> bool {
    false
}

impl Vm {
    /// Containers need root on a Linux host (namespaces, cgroups, netlink).
    pub fn is_supported() -> bool {
        // SAFETY: geteuid cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    pub fn start(cfg: Config) -> io::Result<Vm> {
        // The agent's sockets take root commands, so their folder must be
        // root's alone: /run, not an inherited XDG_RUNTIME_DIR (`sudo -E`
        // keeps the user's, where they could have put the folder first).
        // Made private in one step; never reuse or remove an existing entry,
        // including a stale generation left behind when this PID last ran.
        let dir = private_runtime_directory(|| runtime_directory(Path::new("/run")))?;
        let result = Self::start_in(cfg, dir.clone());
        if result.is_err() {
            let _ = std::fs::remove_dir_all(dir);
        }
        result
    }

    fn start_in(cfg: Config, dir: PathBuf) -> io::Result<Vm> {
        let (sender, events) = mpsc::channel();
        // The agent's first dial: the host's control connection.
        let control = UnixListener::bind(dir.join(format!("h{}.sock", cfg.agent_port)))?;
        control.set_nonblocking(true)?;
        let mut cmd = Command::new(&cfg.initrd);
        cmd.env("ASAGIRI_AGENT_DIR", &dir).env_remove("ASAGIRI_SOCKET");
        if let Some(fd) = cfg.console {
            cmd.stderr(std::process::Stdio::from(fd.try_clone()?)).stdout(std::process::Stdio::from(fd));
        } else {
            cmd.stdout(std::process::Stdio::null());
        }
        let child = Arc::new(Mutex::new(cmd.spawn().map_err(|e| io::Error::new(e.kind(), format!("starting {}: {e}", cfg.initrd.display())))?));
        // Own the child before any fallible thread creation: Drop must reap
        // it and close every already-created listener on a partial start.
        let vm = Vm { dir, child, events, closing: Arc::new(AtomicBool::new(false)), listeners: Mutex::new(Vec::new()), retired: false };
        let (tx, closing) = (sender.clone(), vm.closing.clone());
        let accept = std::thread::Builder::new().name("asa-agent-accep".into()).spawn(move || {
            if let Ok(Some(s)) = accept_until_closed(&control, &closing) {
                let _ = tx.send(Event::GuestConnected(OwnedFd::from(s)));
            }
        })?;
        vm.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(accept);
        let (waiter, tx, closing) = (vm.child.clone(), sender, vm.closing.clone());
        let wait = std::thread::Builder::new().name("asa-agent-wait".into()).spawn(move || {
            while !closing.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(100));
                let status = waiter.lock().unwrap_or_else(|e| e.into_inner()).try_wait();
                match status {
                    Ok(Some(s)) => {
                        let _ = tx.send(Event::Stopped((!s.success()).then(|| format!("the agent exited: {s}"))));
                        return;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = tx.send(Event::Stopped(Some(e.to_string())));
                        return;
                    }
                }
            }
        })?;
        vm.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(wait);
        Ok(vm)
    }

    pub fn next_event(&self, timeout: Duration) -> Option<Event> {
        self.events.recv_timeout(timeout).ok()
    }

    pub fn force_stop(&self) {
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).kill();
    }

    pub fn is_running(&self) -> bool {
        matches!(self.child.lock().unwrap_or_else(|e| e.into_inner()).try_wait(), Ok(None))
    }

    /// Confirms process exit and completes cleanup before a successor starts.
    pub fn retire(&mut self, graceful: Duration, forced: Duration) -> io::Result<()> {
        if self.retired {
            return Ok(());
        }
        self.closing.store(true, Ordering::Release);
        let child = &self.child;
        super::stop::retire(
            || child.lock().unwrap_or_else(|e| e.into_inner()).try_wait().map(|s| s.is_some()),
            |_| child.lock().unwrap_or_else(|e| e.into_inner()).kill(),
            graceful,
            forced,
        )?;
        self.close_listeners();
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.retired = true;
        Ok(())
    }

    fn close_listeners(&mut self) {
        for thread in self.listeners.get_mut().unwrap_or_else(|e| e.into_inner()).drain(..) {
            let _ = thread.join();
        }
    }

    /// No balloon: the agent shares the host's memory as it is.
    pub fn set_memory_target(&self, _bytes: u64) -> Option<u64> {
        None
    }

    /// Accepts the agent's connections to `port`.
    pub fn listen(&self, port: u32, handler: Handler) -> io::Result<()> {
        if self.closing.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "the VM is retiring"));
        }
        let path = self.dir.join(format!("h{port}.sock"));
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path)?;
        l.set_nonblocking(true)?;
        let closing = self.closing.clone();
        let handler: Arc<dyn Fn(VsockStream) + Send + Sync> = Arc::from(handler);
        let listener = std::thread::Builder::new().name(format!("asa-vs-{port:x}")).spawn(move || {
            while let Ok(Some(s)) = accept_until_closed(&l, &closing) {
                let handler = handler.clone();
                // Retirement joins accept loops, never a handler waiting on
                // user data or on the engine's own exit accounting.
                if std::thread::Builder::new().name(format!("asa-vs-{port:x}-conn")).spawn(move || {
                    handler(VsockStream { fd: OwnedFd::from(s) });
                }).is_err() {
                    break;
                }
            }
        })?;
        self.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(listener);
        Ok(())
    }

    /// Opens a stream to a port the agent listens on.
    pub fn connect(&self, port: u32) -> io::Result<VsockStream> {
        self.connector().connect(port)
    }

    /// What opens streams to the agent's ports from any thread, while the
    /// `Vm` itself is kept elsewhere.
    pub fn connector(&self) -> Connector {
        Connector { dir: self.dir.clone(), closing: self.closing.clone() }
    }
}

/// Opens streams to the agent's ports (`Vm::connector`).
#[derive(Debug)]
pub struct Connector {
    dir: PathBuf,
    closing: Arc<AtomicBool>,
}

impl Connector {
    /// Opens a stream to a port the agent listens on. Its listeners appear
    /// just after it connects, so a moment of waiting is allowed for.
    pub fn connect(&self, port: u32) -> io::Result<VsockStream> {
        let path = self.dir.join(format!("g{port}.sock"));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if self.closing.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::NotConnected, "the VM is retiring"));
            }
            match UnixStream::connect(&path) {
                Ok(s) => return Ok(VsockStream { fd: OwnedFd::from(s) }),
                Err(e) if Instant::now() < deadline && matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.retired {
            return;
        }
        self.closing.store(true, Ordering::Release);
        self.force_stop();
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).wait();
        self.close_listeners();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = runtime_directory(&std::env::temp_dir());
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn vm(&self, name: &str, child: Child) -> Vm {
            let dir = self.0.join(name);
            std::fs::create_dir(&dir).unwrap();
            let (_, events) = mpsc::channel();
            Vm { dir, child: Arc::new(Mutex::new(child)), events, closing: Arc::new(AtomicBool::new(false)), listeners: Mutex::new(Vec::new()), retired: false }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn a_reused_pid_allocates_a_new_private_generation_without_touching_old_entries() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let fixture = Fixture::new();
        let old = fixture.0.join("agent-1-0");
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("keep"), b"old generation").unwrap();
        let linked = fixture.0.join("agent-1-1");
        symlink(&old, &linked).unwrap();
        let file = fixture.0.join("agent-1-2");
        std::fs::write(&file, b"keep file").unwrap();
        let allocate_after_restart = || {
            let mut generation = 0;
            private_runtime_directory(|| {
                let path = fixture.0.join(format!("agent-1-{generation}"));
                generation += 1;
                path
            }).unwrap()
        };
        let first = allocate_after_restart();
        let second = allocate_after_restart();
        assert_ne!(first, second);
        assert_eq!(first, fixture.0.join("agent-1-3"));
        assert_eq!(second, fixture.0.join("agent-1-4"));
        for path in [&first, &second] {
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
        }
        assert_eq!(std::fs::read(old.join("keep")).unwrap(), b"old generation");
        assert_eq!(std::fs::read_link(linked).unwrap(), old);
        assert_eq!(std::fs::read(file).unwrap(), b"keep file");
    }

    #[test]
    fn runtime_directory_collision_retries_are_bounded_and_other_errors_are_preserved() {
        let fixture = Fixture::new();
        let mut attempts = 0;
        let error = private_runtime_directory(|| {
            attempts += 1;
            fixture.0.clone()
        }).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(attempts, 64);
        assert!(fixture.0.is_dir());
        let error = private_runtime_directory(|| fixture.0.join("missing/child")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn retirement_waits_for_a_natural_exit_and_finishes_cleanup() {
        let fixture = Fixture::new();
        let marker = fixture.0.join("flushed");
        let child = Command::new("/bin/sh").args(["-c", "printf flushed > \"$1\"", "sh"]).arg(&marker).spawn().unwrap();
        let mut vm = fixture.vm("natural", child);
        let dir = vm.dir.clone();
        vm.retire(Duration::from_secs(2), Duration::from_secs(1)).unwrap();
        assert!(vm.child.lock().unwrap().try_wait().unwrap().unwrap().success(), "retirement must allow the graceful flush to finish");
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "flushed");
        assert!(!dir.exists());
        vm.retire(Duration::ZERO, Duration::ZERO).unwrap();
    }

    #[test]
    fn retained_old_handles_cannot_reach_or_delete_the_successor() {
        let fixture = Fixture::new();
        let child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let mut old = fixture.vm("generation", child);
        let connector = old.connector();
        let dir = old.dir.clone();
        old.retire(Duration::ZERO, Duration::from_secs(2)).unwrap();
        assert!(old.child.lock().unwrap().try_wait().unwrap().is_some(), "force-stop success must mean the child was reaped");
        assert!(!dir.exists());
        std::fs::create_dir(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("g123.sock")).unwrap();
        assert_eq!(connector.connect(123).unwrap_err().kind(), io::ErrorKind::NotConnected);
        drop(old);
        assert!(dir.exists(), "late destruction of a retired VM must not remove successor resources");
        assert!(dir.join("g123.sock").exists());
        drop(listener);
    }

    #[test]
    fn cleanup_failure_retains_ownership_and_can_be_retried() {
        let fixture = Fixture::new();
        let child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let mut vm = fixture.vm("cleanup", child);
        std::fs::remove_dir(&vm.dir).unwrap();
        std::fs::write(&vm.dir, b"not a directory").unwrap();
        assert!(vm.retire(Duration::ZERO, Duration::from_secs(2)).is_err());
        assert!(!vm.retired, "cleanup failure must not authorize a successor");
        assert!(vm.dir.is_file());
        std::fs::remove_file(&vm.dir).unwrap();
        std::fs::create_dir(&vm.dir).unwrap();
        vm.retire(Duration::ZERO, Duration::from_secs(2)).unwrap();
        assert!(vm.retired);
    }

    #[test]
    fn runtime_directories_belong_to_one_generation() {
        let root = Path::new("/run");
        assert_ne!(runtime_directory(root), runtime_directory(root));
    }

    #[test]
    fn retirement_closes_unconnected_listeners_and_joins_their_threads() {
        let fixture = Fixture::new();
        let child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let mut vm = fixture.vm("listeners", child);
        vm.listen(123, Box::new(|_| panic!("no connections were made"))).unwrap();
        vm.listen(456, Box::new(|_| panic!("no connections were made"))).unwrap();
        assert_eq!(vm.listeners.lock().unwrap().len(), 2);
        let directory = vm.dir.clone();
        let started = Instant::now();
        vm.retire(Duration::ZERO, Duration::from_secs(2)).unwrap();
        assert!(vm.listeners.lock().unwrap().is_empty());
        assert!(!directory.exists());
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(vm.listen(789, Box::new(|_| {})).unwrap_err().kind(), io::ErrorKind::NotConnected);
    }

    #[test]
    fn control_accept_can_be_canceled_without_a_guest_connection() {
        let fixture = Fixture::new();
        let listener = UnixListener::bind(fixture.0.join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let closing = Arc::new(AtomicBool::new(false));
        let cancel = closing.clone();
        let (sent, received) = mpsc::channel();
        let thread = std::thread::spawn(move || sent.send(accept_until_closed(&listener, &cancel).unwrap().is_none()).unwrap());
        assert!(received.recv_timeout(Duration::from_millis(30)).is_err());
        closing.store(true, Ordering::Release);
        assert!(received.recv_timeout(Duration::from_secs(1)).unwrap());
        thread.join().unwrap();
    }
}
