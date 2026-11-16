//! `asagiri host-access <user>`: everything the window's onboarding on a Linux
//! host needs root for, in one go, so the system asks for a password once.
//! The window runs it through pkexec (the polkit action dev.asagiri.host-access,
//! packaging/linux/dev.asagiri.host-access.policy); `sudo asagiri host-access $USER`
//! does the same from a terminal.
//!
//! 1. The `asagiri` group, if the package's install didn't make it.
//! 2. `user` in it.
//! 3. The engine: started (and started at boot) if it isn't running,
//!    restarted if it is but doesn't answer.
//! 4. The socket's access list, with `user` on it (crate::daemon::access), so
//!    the change applies now, without signing in again.
//!
//! `--restart` restarts a running engine as well (the window's Restart
//! engine, after the package was upgraded), and waits for it to answer.
//!
//! Exit status: 0 done; 10 done, but this system keeps no access lists, so
//! `user` must log out and back in once before Asagiri lets them in.

use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const GROUP: &str = "asagiri";
const SOCKET: &str = crate::client::SERVICE_SOCKET;
/// The exit status that says: done, but sign in again before it applies.
pub const SIGN_IN_AGAIN: u8 = 10;

pub fn run(args: &[String]) -> io::Result<u8> {
    let (restart, args) = match args {
        [flag, rest @ ..] if flag == "--restart" => (true, rest),
        _ => (false, args),
    };
    let [user] = args else { return Err(io::Error::other("usage: asagiri host-access [--restart] USER")) };
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other(format!("needs root: sudo asagiri host-access {}{user}", if restart { "--restart " } else { "" })));
    }
    if crate::daemon::access::uid(user)
        .map_err(|e| io::Error::new(e.kind(), format!("couldn't look up account {user}: {e}")))?
        .is_none()
    {
        return Err(io::Error::other(format!("no account named {user}")));
    }
    let lookup = || crate::daemon::access::members(GROUP)
        .map_err(|e| io::Error::new(e.kind(), format!("couldn't look up group {GROUP}: {e}")));
    let members = match lookup() {
        Ok(members) => members,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            must(Command::new("groupadd").args(["--system", GROUP]), "groupadd")?;
            lookup()?
        }
        Err(e) => return Err(e),
    };
    if !members.iter().any(|m| m == user) {
        // usermod everywhere we package for; gpasswd where it isn't.
        let added = Command::new("usermod").args(["-aG", GROUP, user]).status().is_ok_and(|s| s.success());
        if !added {
            must(Command::new("gpasswd").args(["-a", user, GROUP]), "gpasswd")?;
        }
        println!("{user} is in the {GROUP} group");
    }
    install_from_appimage()?;
    engine(restart)?;
    match crate::daemon::access::grant(Path::new(SOCKET), GROUP) {
        Ok(_) => {
            println!("{user} can use Asagiri now");
            Ok(0)
        }
        Err(e) if crate::daemon::access::unsupported(&e) => {
            println!("{user} can use Asagiri after logging out and back in (this system keeps no access lists)");
            Ok(SIGN_IN_AGAIN)
        }
        Err(e) => Err(io::Error::other(format!("couldn't give {user} the socket: {e}"))),
    }
}

fn install_from_appimage() -> io::Result<()> {
    let Some(appdir) = appdir() else { return Ok(()) };
    if ["/lib/systemd/system/asagiri.service", "/usr/lib/systemd/system/asagiri.service",
        "/etc/systemd/system/asagiri.service", "/run/systemd/system/asagiri.service"]
        .iter().any(|p| std::fs::symlink_metadata(p).is_ok()) {
        return Ok(());
    }
    let bin = Path::new("/usr/local/bin");
    install_appimage_files(&appdir, bin, Path::new("/etc/systemd/system/asagiri.service"))?;
    println!("Asagiri's engine is installed in /usr/local/bin");
    must(Command::new("systemctl").arg("daemon-reload"), "systemctl daemon-reload")
}

/// Recognize our two bundled executables, not another application's inherited APPDIR.
pub fn appdir() -> Option<std::path::PathBuf> {
    appdir_for(Path::new(&std::env::var_os("APPDIR")?), &std::env::current_exe().ok()?)
}

fn appdir_for(dir: &Path, exe: &Path) -> Option<std::path::PathBuf> {
    if !dir.is_absolute() { return None; }
    let dir = dir.canonicalize().ok()?;
    let exe = exe.canonicalize().ok()?;
    if dir.parent().is_none() { return None; }
    ["asagiri", "asagiri-window"].iter()
        .any(|name| exe == dir.join("usr/bin").join(name)).then_some(dir)
}

fn install_appimage_files(appdir: &Path, bin: &Path, unit: &Path) -> io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    // Preflight both inputs and destinations before creating installed files.
    // Never follow a source link or replace an existing, different executable.
    let readonly_fuse = validate_appimage_source(appdir)?;
    let mut inputs = Vec::new();
    for name in ["asagiri", "asagiri-init"] {
        let mut source = OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(appdir.join("usr/bin").join(name))?;
        let metadata = source.metadata()?;
        // SAFETY: geteuid has no preconditions.
        if !metadata.is_file() || metadata.mode() & 0o022 != 0
            || metadata.mode() & 0o100 == 0
            || (!readonly_fuse && metadata.uid() != unsafe { libc::geteuid() }) {
            return Err(io::Error::other("AppImage engine input is not an owned, regular executable"));
        }
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes)?;
        let existing = if let Ok(metadata) = std::fs::symlink_metadata(bin.join(name)) {
            // SAFETY: geteuid has no preconditions.
            if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o7777 != 0o755 || std::fs::read(bin.join(name))? != bytes {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists,
                    format!("{} already exists and differs from this AppImage", bin.join(name).display())));
            }
            true
        } else { false };
        inputs.push((name, bytes, existing));
    }
    if std::fs::symlink_metadata(unit).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "the Asagiri service already exists"));
    }
    for directory in [bin, unit.parent().ok_or_else(|| io::Error::other("service path has no parent"))?] {
        std::fs::create_dir_all(directory)?;
        let metadata = std::fs::symlink_metadata(directory)?;
        // SAFETY: geteuid has no preconditions.
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            return Err(io::Error::other(format!("{} must be owned by the installing user and not writable by other users", directory.display())));
        }
    }
    for (name, bytes, existing) in inputs {
        if !existing { install_new_file(&bin.join(name), &bytes, 0o755)?; }
    }
    let contents = include_str!("../../../packaging/linux/asagiri.service")
        .replace("/usr/bin/asagiri", "/usr/local/bin/asagiri");
    install_new_file(unit, contents.as_bytes(), 0o644)
}

fn validate_appimage_source(appdir: &Path) -> io::Result<bool> {
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let dir = fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(appdir)?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    let mut flags = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: both outputs point to valid writable storage and dir is open.
    if unsafe { libc::fstatfs(dir.as_raw_fd(), filesystem.as_mut_ptr()) } != 0
        || unsafe { libc::fstatvfs(dir.as_raw_fd(), flags.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // A real read-only FUSE mount can legitimately report the image builder's
    // UID. Environment variables and a ".mount_" name do not prove a mount.
    // SAFETY: the successful calls above initialized both outputs.
    let readonly_fuse = unsafe { filesystem.assume_init() }.f_type as u64 == 0x6573_5546
        && unsafe { flags.assume_init() }.f_flag & libc::ST_RDONLY != 0;
    if !readonly_fuse {
        // SAFETY: geteuid has no preconditions.
        let owner = unsafe { libc::geteuid() };
        let metadata = dir.metadata()?;
        if metadata.uid() != owner || metadata.mode() & 0o7777 != 0o700 {
            return Err(io::Error::other("AppImage extraction must be private and owned by the installing user"));
        }
        // The runtime creates this directory after elevation. A private child
        // is unsafe if another user can rename or replace an ancestor.
        let absolute = appdir.canonicalize()?;
        for parent in absolute.ancestors().skip(1) {
            let metadata = fs::symlink_metadata(parent)?;
            if !metadata.is_dir() || (metadata.uid() != 0 && metadata.uid() != owner)
                || (metadata.mode() & 0o022 != 0 && !(metadata.uid() == 0 && metadata.mode() & 0o1000 != 0)) {
                return Err(io::Error::other("AppImage extraction has an untrusted parent directory"));
            }
        }
    }
    for child in ["usr", "usr/bin"] {
        let metadata = fs::symlink_metadata(appdir.join(child))?;
        // SAFETY: geteuid has no preconditions.
        if !metadata.is_dir() || metadata.mode() & 0o022 != 0
            || (!readonly_fuse && metadata.uid() != unsafe { libc::geteuid() }) {
            return Err(io::Error::other("AppImage executable directory is not an owned, regular directory"));
        }
    }
    Ok(readonly_fuse)
}

fn install_new_file(destination: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let directory = destination.parent().ok_or_else(|| io::Error::other("installation path has no parent"))?;
    let name = destination.file_name().ok_or_else(|| io::Error::other("installation path has no filename"))?.to_string_lossy();
    let temporary = directory.join(format!(".{name}.{}.part", std::process::id()));
    let mut file = OpenOptions::new().write(true).create_new(true).mode(mode).open(&temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        std::fs::hard_link(&temporary, destination)?;
        File::open(directory)?.sync_all()
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

/// The engine running and answering on its socket; with `restart`, a
/// running one restarted (on the binary the package installed).
fn engine(restart: bool) -> io::Result<()> {
    if !restart && answers(Duration::from_millis(250)) {
        return Ok(());
    }
    let active = Command::new("systemctl").args(["is-active", "--quiet", "asagiri"]).status().is_ok_and(|s| s.success());
    if active {
        must(Command::new("systemctl").args(["restart", "asagiri"]), "systemctl restart asagiri")?;
    } else {
        must(Command::new("systemctl").args(["enable", "--now", "asagiri"]), "systemctl enable --now asagiri")?;
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if answers(remaining.min(Duration::from_millis(250))) {
            return Ok(());
        }
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            std::thread::sleep(remaining.min(Duration::from_millis(200)));
        }
    }
    Err(io::Error::other("Asagiri's engine didn't start: journalctl -u asagiri says why"))
}

fn answers(timeout: Duration) -> bool {
    crate::client::probe_socket(Path::new(SOCKET), "/_ping", timeout).is_ok_and(|r| r.ok() && r.body == b"OK")
}

fn must(cmd: &mut Command, what: &str) -> io::Result<()> {
    let status = cmd.status().map_err(|e| io::Error::other(format!("{what}: {e}")))?;
    if status.success() { Ok(()) } else { Err(io::Error::other(format!("{what} failed ({status})"))) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!("asagiri-appimage-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir(&root).unwrap();
            std::fs::create_dir_all(root.join("app/usr/bin")).unwrap();
            std::fs::set_permissions(root.join("app"), std::fs::Permissions::from_mode(0o700)).unwrap();
            for name in ["asagiri", "asagiri-init", "asagiri-window"] {
                std::fs::write(root.join("app/usr/bin").join(name), name).unwrap();
                std::fs::set_permissions(root.join("app/usr/bin").join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Self(root)
        }
        fn install(&self) -> io::Result<()> {
            install_appimage_files(&self.0.join("app"), &self.0.join("bin"), &self.0.join("units/asagiri.service"))
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

    #[test]
    fn inherited_appdir_does_not_identify_an_unrelated_executable() {
        let f = Fixture::new();
        let app = f.0.join("app");
        assert_eq!(appdir_for(&app, &app.join("usr/bin/asagiri")), Some(app.canonicalize().unwrap()));
        assert_eq!(appdir_for(&app, &app.join("usr/bin/asagiri-window")), Some(app.canonicalize().unwrap()));
        assert!(appdir_for(&app, &app.join("usr/bin/asagiri-init")).is_none());
        assert!(appdir_for(&f.0, &app.join("usr/bin/asagiri")).is_none());
        assert!(appdir_for(Path::new("/"), &app.join("usr/bin/asagiri")).is_none());
        assert!(appdir_for(Path::new("."), &app.join("usr/bin/asagiri")).is_none());
    }

    #[test]
    fn installation_uses_regular_executables_and_preserves_existing_services() {
        let f = Fixture::new();
        std::fs::set_permissions(f.0.join("app/usr/bin/asagiri"), std::fs::Permissions::from_mode(0o6755)).unwrap();
        f.install().unwrap();
        assert_eq!(std::fs::read(f.0.join("bin/asagiri")).unwrap(), b"asagiri");
        assert_eq!(std::fs::metadata(f.0.join("bin/asagiri")).unwrap().permissions().mode() & 0o7777, 0o755);
        let unit = f.0.join("units/asagiri.service");
        assert!(std::fs::read_to_string(&unit).unwrap().contains("/usr/local/bin/asagiri daemon"));
        std::fs::write(&unit, "custom service").unwrap();
        assert_eq!(f.install().unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(unit).unwrap(), "custom service");
    }

    #[test]
    fn missing_or_linked_agent_cannot_leave_a_partial_installation() {
        let f = Fixture::new();
        let agent = f.0.join("app/usr/bin/asagiri-init");
        std::fs::remove_file(&agent).unwrap();
        assert!(f.install().is_err());
        assert!(!f.0.join("bin/asagiri").exists());
        symlink("asagiri", &agent).unwrap();
        assert!(f.install().is_err());
        assert!(!f.0.join("bin/asagiri").exists());
    }

    #[test]
    fn an_existing_different_executable_is_not_replaced() {
        let f = Fixture::new();
        std::fs::create_dir(f.0.join("bin")).unwrap();
        std::fs::write(f.0.join("bin/asagiri"), "existing engine").unwrap();
        assert_eq!(f.install().unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(f.0.join("bin/asagiri")).unwrap(), "existing engine");
        assert!(!f.0.join("bin/asagiri-init").exists());
        assert!(!f.0.join("units").exists());
    }

    #[test]
    fn writable_bundle_paths_are_rejected_before_installation() {
        for path in ["app", "app/usr", "app/usr/bin", "app/usr/bin/asagiri-init", "."] {
            let f = Fixture::new();
            std::fs::set_permissions(f.0.join(path), std::fs::Permissions::from_mode(0o777)).unwrap();
            assert!(f.install().is_err(), "accepted writable {path}");
            assert!(!f.0.join("bin").exists());
            assert!(!f.0.join("units").exists());
        }
    }

    #[test]
    fn a_linked_bundle_directory_cannot_supply_installed_executables() {
        let f = Fixture::new();
        std::fs::rename(f.0.join("app/usr/bin"), f.0.join("substitute")).unwrap();
        symlink(f.0.join("substitute"), f.0.join("app/usr/bin")).unwrap();
        assert!(f.install().is_err());
        assert!(!f.0.join("bin").exists());
        assert!(!f.0.join("units").exists());
    }
}
