//! `kiri daemon`: owns the VM and serves the Docker Engine API on a Unix
//! socket. The docker CLI (context `kiri`), the kiri CLI and the window are
//! all clients of it.

#[cfg(target_os = "linux")]
pub mod access;
mod api;
mod binds;
mod builder;
pub mod dns;
pub mod tls;
pub mod proxy;
mod detach;
mod domains;
mod engine;
mod filters;
mod health;
mod hostetc;
mod hostopts;
mod mirror;
mod http;
mod logfile;
mod names;
mod network;
#[cfg(target_os = "macos")]
pub(crate) mod nfs;
mod ops;
pub(crate) mod ports;
mod pulls;
pub mod time;
#[cfg(target_os = "macos")]
mod usb;
mod vmcpu;
mod vmlink;
mod volumes;
mod websocket;

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::diag;

/// `~/.kiri` (`~/.kiri-dev` for Kiri Dev): the daemon's state (the CA key, proxy settings). Always under
/// HOME: `KIRI_SOCKET` may point the socket anywhere, but it must not move
/// private state into a directory another user can prepare.
pub fn state_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(home.join(crate::flavor::STATE.get()))
}

/// Root on Linux uses `/run/kiri/docker.sock`; other users use their home.
/// So does root in WSL: kiri-win's bridge, and the boot command of a Kiri
/// already installed there, know the engine by its home socket.
pub fn socket_path() -> io::Result<PathBuf> {
    if let Some(s) = std::env::var_os("KIRI_SOCKET") {
        return Ok(PathBuf::from(s));
    }
    #[cfg(target_os = "linux")]
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } == 0 && binds::wsl().is_none() {
        return Ok(crate::client::service_socket());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(home.join(crate::flavor::STATE.get()).join("run/docker.sock"))
}

/// Raises the soft open-file limit to the hard one, as Go does for dockerd.
/// launchd starts the daemon with 256, and each relayed connection holds
/// four or five descriptors: a LAN client opening ~60 connections to a
/// published port ran it out, and the daemon fell over with its VM.
fn raise_open_files() {
    // SAFETY: rlimit is plain data; get/setrlimit read and write it.
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        // macOS refuses more than OPEN_MAX for the soft limit.
        let want = if cfg!(target_os = "macos") { lim.rlim_max.min(10240) } else { lim.rlim_max };
        if lim.rlim_cur < want {
            lim.rlim_cur = want;
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
        }
    }
}

pub fn run(_args: &[String]) -> io::Result<()> {
    let t0 = Instant::now();
    raise_open_files();
    let socket = socket_path()?;
    // Root's socket in the service's directory, which is checked to be
    // root's alone: the one place a group is let in without being named.
    #[cfg(target_os = "linux")]
    // SAFETY: geteuid cannot fail.
    let service = unsafe { libc::geteuid() } == 0 && socket == crate::client::service_socket();
    if let Some(dir) = socket.parent() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o755).create(dir)?;
        #[cfg(target_os = "linux")]
        if service {
            service_dir(dir)?;
        }
    }
    if UnixStream::connect(&socket).is_ok() {
        // Not an error: launchd (KeepAlive on failure) must not retry it.
        diag::info!("engine already running", socket = socket.display());
        return Ok(());
    }
    let _ = std::fs::remove_file(&socket); // stale, from a daemon that died
    // The owner's alone from the moment it exists, whatever umask the daemon
    // was started with; a group is let in below, once it is known which.
    // SAFETY: umask cannot fail, and is put back at once.
    let umask = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(&socket);
    // SAFETY: as above.
    unsafe { libc::umask(umask) };
    let listener = listener?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    // An explicit group keeps Kiri's managed membership policy. Without one,
    // root's service socket on Linux uses ordinary Unix group access, like
    // dockerd: container users need not exist in the host's account
    // database. A socket anywhere else stays the owner's.
    let access_group = std::env::var_os("KIRI_SOCKET_GROUP");
    if let Some(group) = &access_group {
        socket_group(&socket, group)?;
    }
    #[cfg(target_os = "linux")]
    if access_group.is_none() && service {
        default_socket_access(&socket)?;
    }
    // Each member by name too, so one just added needn't sign in again; and
    // connections are checked against the group when made and while open, so
    // a member removed is let go at once, whatever the list still says.
    #[cfg(target_os = "linux")]
    let gate = match &access_group {
        Some(group) => {
            let gate = access::Gate::new(&socket, group)?;
            match gate.grant() {
                Ok(n) => diag::info!("socket access list written", members = n),
                Err(e) => diag::warn!("socket access list not written: new members sign in again first", error = e),
            }
            gate.clone().watch()?;
            Some(gate)
        }
        None => None,
    };

    // SIGINT/SIGTERM shut down cleanly: power the guest off, remove the socket.
    // Blocked here so every thread inherits it; one thread waits for them.
    // SAFETY: plain signal-mask calls on a local set.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };

    let store = kiri_image::Store::open(kiri_image::Store::default_root()?)?;
    let engine = engine::Engine::boot(store).inspect_err(|e| diag::error!("engine did not start", error = e))?;
    diag::info!("engine ready", version = env!("CARGO_PKG_VERSION"), ms = t0.elapsed().as_millis(), socket = format!("unix://{}", socket.display()));
    // Crash files from earlier runs, if the person said they may go.
    diag::report::spawn_crash_sender(Duration::from_secs(30), Some(Duration::from_secs(6 * 3600)));
    // Container files in Finder, at ~/Kiri. A Linux host has them on its
    // own disk already.
    #[cfg(target_os = "macos")]
    finder(&engine);
    kiri_image::set_proxy_source(proxy::for_registry);
    // The managed BuildKit stops itself when builds have been idle a while.
    builder::watch(engine.clone());
    // A Kubernetes node the engine restarted gets its kubectl context.
    crate::k8s::sync_at_start();
    // Kiri Dev leaves *.kiri.local and ports 80 and 443 to the installed
    // Kiri, unless given ports of its own (Settings → Domains, or
    // KIRI_HTTP_PORT and KIRI_HTTPS_PORT).
    let routers = !crate::flavor::dev() || domains::own_ports();
    if crate::flavor::dev() {
        if !routers {
            diag::info!(format!("{}: container domains off (ports in Settings → Domains turn them on)", crate::flavor::APP));
        }
    } else if let Err(e) = dns::start() {
        diag::warn!("*.kiri.local off", error = e);
    }
    if routers {
        match domains::start_tls(engine.clone()) {
            Ok(port) => diag::info!("HTTPS for container domains", port = port, trusted = tls::trusted()),
            Err(e) => diag::warn!("HTTPS for container domains off", error = e),
        }
        match domains::start(engine.clone()) {
            Ok(port) => diag::info!(format!(
                "container domains on http://<name>.{}{}",
                if dns::installed() { "kiri.local" } else { "kiri.localhost" },
                if port == 80 { String::new() } else { format!(":{port}") }
            )),
            Err(e) => diag::warn!("container domains off", error = e),
        }
    }

    watch_own_app(engine.clone(), socket.clone());

    let on_signal = engine.clone();
    let sock = socket.clone();
    std::thread::Builder::new().name("kiri-signals".into()).spawn(move || {
        let mut sig = 0;
        // SAFETY: waits on the blocked set; writes the signal number to `sig`.
        unsafe { libc::sigwait(&set, &mut sig) };
        diag::info!("engine shutting down", signal = sig);
        #[cfg(target_os = "macos")]
        nfs::unmount();
        let _ = std::fs::remove_file(&sock);
        on_signal.shutdown();
        // Give the guest a moment to power off before the VM is released.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::process::exit(0);
    })?;

    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let engine = engine.clone();
        #[cfg(target_os = "linux")]
        let gate = gate.clone();
        let spawned = std::thread::Builder::new().name("kiri-api".into()).spawn(move || {
            // Checked here, not in the accept loop: a slow user lookup holds up
            // this connection only.
            #[cfg(target_os = "linux")]
            let _seat = match &gate {
                Some(gate) => match gate.admit(&conn) {
                    Some(seat) => Some(seat),
                    None => return,
                },
                None => None,
            };
            let _ = api::serve(conn, engine);
        });
        if let Err(e) = spawned {
            diag::error!("cannot start a connection thread", error = e);
        }
    }
    Ok(())
}

/// When the app this daemon runs from is deleted (appwatch.rs): docker
/// build handed back (buildwith.rs: what was there before; the choice
/// stays, so the app taken up again takes it again), and the engine stops.
fn watch_own_app(engine: std::sync::Arc<engine::Engine>, socket: std::path::PathBuf) {
    crate::appwatch::watch(move |exe| {
        diag::error!("the app this engine runs from was deleted; stopping and handing docker build back", path = exe.display());
        if let Err(e) = crate::buildwith::places().and_then(|p| p.give_back()) {
            diag::warn!("docker build not handed back", error = e);
        }
        #[cfg(target_os = "macos")]
        nfs::unmount();
        let _ = std::fs::remove_file(&socket);
        engine.shutdown();
        std::thread::sleep(Duration::from_millis(50));
        // 0: launchd restarts the engine on failure only, and there is
        // nothing left to restart.
        std::process::exit(0);
    });
}

/// Serves container files over NFS and mounts them at ~/Kiri.
/// KIRI_NO_FINDER=1 skips the mount.
#[cfg(target_os = "macos")]
fn finder(engine: &std::sync::Arc<engine::Engine>) {
    match nfs::start(engine.clone()) {
        Ok(socket) if std::env::var_os("KIRI_NO_FINDER").is_none() => match nfs::mount(&socket) {
            Ok(dir) => diag::info!("container files in Finder", at = dir.display()),
            Err(e) => diag::warn!("Finder mount failed", error = e, nfs = socket.display()),
        },
        Ok(socket) => diag::info!("container files over NFS", at = socket.display()),
        Err(e) => diag::warn!("container files off", error = e),
    }
}

/// The directory of root's service socket must be root's own, and not a
/// link to somewhere else: the socket made in it is opened to a group.
#[cfg(target_os = "linux")]
fn service_dir(dir: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} must be a root-owned directory, not a symlink", dir.display())));
    }
    // The socket enforces group access; a restrictive service umask
    // must not prevent that group from reaching it.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
}

/// Docker's permissions for the service socket of a root-run Linux daemon:
/// 0660 root:docker, or root:root where there is no docker group. A lookup
/// that fails is not a missing group: the daemon doesn't start.
#[cfg(target_os = "linux")]
fn default_socket_access(socket: &std::path::Path) -> io::Result<()> {
    let gid = group_id(std::ffi::OsStr::new("docker"))?;
    // Root's group by number, and before the mode: the socket was made with
    // the group the daemon ran with, which need not be root's.
    std::os::unix::fs::lchown(socket, Some(0), Some(gid.unwrap_or(0)))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o660))?;
    match gid {
        Some(gid) => diag::info!("socket access", group = "docker", gid = gid),
        // Minimal Linux installations may have no docker group.
        None => diag::warn!("no docker group: socket access is for root's group", gid = 0),
    }
    Ok(())
}

/// Gives `group` read-write access to the socket.
fn socket_group(socket: &std::path::Path, group: &std::ffi::OsStr) -> io::Result<()> {
    let gid = group_id(group)?.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no group {}", group.to_string_lossy())))?;
    std::os::unix::fs::chown(socket, None, Some(gid))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o660))?;
    diag::info!("socket access", group = group.to_string_lossy(), gid = gid);
    Ok(())
}

/// A group's id, `None` when there is no such group. A lookup that fails
/// (a directory service that is down) is an error, not a missing group.
fn group_id(group: &std::ffi::OsStr) -> io::Result<Option<u32>> {
    let name = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(group)).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut size = 1024;
    loop {
        let mut buf: Vec<libc::c_char> = vec![0; size];
        // SAFETY: group is plain data; all-zero is a valid value.
        let mut rec: libc::group = unsafe { std::mem::zeroed() };
        let mut out: *mut libc::group = std::ptr::null_mut();
        // SAFETY: getgrnam_r with a valid C string, a group record and a buffer of the stated size.
        match unsafe { libc::getgrnam_r(name.as_ptr(), &mut rec, buf.as_mut_ptr(), buf.len(), &mut out) } {
            0 if out.is_null() => return Ok(None),
            0 => return Ok(Some(rec.gr_gid)),
            // A group of many members needs a larger buffer.
            libc::ERANGE if size < 1 << 28 => size *= 2,
            libc::ENOENT | libc::ESRCH => return Ok(None),
            e => {
                let e = io::Error::from_raw_os_error(e);
                return Err(io::Error::new(e.kind(), format!("looking up group {}: {e}", group.to_string_lossy())));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_group_is_not_a_failed_lookup() {
        assert_eq!(group_id(std::ffi::OsStr::new("kiri-no-such-group")).unwrap(), None);
        assert_eq!(group_id(std::ffi::OsStr::new("a\0b")).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        // SAFETY: getegid cannot fail; getgrgid's record is read at once.
        let own = unsafe {
            let g = libc::getgrgid(libc::getegid());
            (!g.is_null()).then(|| std::ffi::CStr::from_ptr((*g).gr_name).to_owned())
        };
        if let Some(own) = own {
            let name = std::ffi::OsStr::new(own.to_str().unwrap());
            // SAFETY: getegid cannot fail.
            assert_eq!(group_id(name).unwrap(), Some(unsafe { libc::getegid() }));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn root_listens_where_clients_look_for_the_service() {
        assert_eq!(crate::client::service_socket(), std::path::Path::new(crate::client::SERVICE_SOCKET));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_service_directory_is_roots_and_not_a_link() {
        use std::os::unix::fs::MetadataExt;
        let base = std::env::temp_dir().join(format!("kiri-socket-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (real, link) = (base.join("real"), base.join("link"));
        std::fs::create_dir_all(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // A link is refused, and what it leads to is left as it was.
        assert_eq!(service_dir(&link).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::metadata(&real).unwrap().mode() & 0o777, 0o700);
        // SAFETY: geteuid cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            service_dir(&real).unwrap();
            assert_eq!(std::fs::metadata(&real).unwrap().mode() & 0o777, 0o755);
            std::os::unix::fs::chown(&real, Some(4242), None).unwrap();
            assert_eq!(service_dir(&real).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        } else {
            assert_eq!(service_dir(&real).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Without a docker group the socket is root's group's, whatever group
    /// it was made with; only root can try it.
    #[cfg(target_os = "linux")]
    #[test]
    fn default_access_never_keeps_the_group_the_socket_was_made_with() {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kiri-socket-access-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("docker.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        std::os::unix::fs::chown(&socket, Some(0), Some(4242)).unwrap();
        default_socket_access(&socket).unwrap();
        let meta = std::fs::metadata(&socket).unwrap();
        let docker = group_id(std::ffi::OsStr::new("docker")).unwrap();
        assert_eq!((meta.uid(), meta.gid(), meta.mode() & 0o777), (0, docker.unwrap_or(0), 0o660));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
