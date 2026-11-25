mod agent;
mod cgroup;
mod cgroup_kill;
mod container;
mod devices;
mod copy;
mod disk;
mod dns;
mod fsnotify;
mod fsview;
mod hostopts;
mod inroot;
mod layers;
mod net;
mod netlink;
mod overlay;
mod pid1;
mod relay;
mod tarnum;
mod removal;
mod roots;
mod seccomp;
mod custom_seccomp;
mod startup;
mod sys;
mod syscall_table;
mod transport;
mod trim;
mod usb;
mod usage;
mod volmount;
mod volumes;

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use asagiri_proto::{HOST_PORT, Hello, Msg, PROTOCOL};

use sys::{boottime_ns, check, mount};

/// Where images and containers live: the data disk if attached, else tmpfs.
const DATA: &str = "/var/lib/asagiri";

pub fn main() {
    install_panic_hook();
    let init_start_ns = boottime_ns();
    let result = run(init_start_ns);
    if let Err(e) = &result {
        // Nothing above us can handle this; say why on the console and stop.
        eprintln!("asagiri-init: {e}");
    }
    if transport::is_host_process() {
        std::process::exit(i32::from(result.is_err()));
    }
    power_off();
}

/// Maximum length of a guest panic report, including its newline.
const PANIC_BUF_LEN: usize = 512;

/// With `panic = "abort"`, a panic in PID 1 or the host agent would otherwise
/// abort without leaving a message on the console/stderr. Format into a fixed
/// stack buffer and write directly to `STDERR_FILENO` via `libc::write` (no
/// heap allocation and no `std::io::stderr()` RefCell/lock, so a panic during
/// OOM or inside `eprintln!` argument formatting cannot double-panic).
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info
            .payload()
            .downcast_ref::<&'static str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str));
        let location = info.location().map(|l| (l.file(), l.line()));
        let mut buf = [0u8; PANIC_BUF_LEN];
        let msg = format_panic_message(&mut buf, location, payload);
        write_stderr_all(msg);
    }));
}

fn format_panic_message<'a>(
    buf: &'a mut [u8],
    location: Option<(&str, u32)>,
    payload: Option<&str>,
) -> &'a [u8] {
    if buf.is_empty() {
        return &[];
    }
    let what = payload.unwrap_or("a panic payload that is not a string");
    let mut cur = io::Cursor::new(&mut *buf);
    let res = match location {
        Some((file, line)) => write!(cur, "asagiri-init: panic at {file}:{line}: {what}\n"),
        None => write!(cur, "asagiri-init: panic at <unknown location>: {what}\n"),
    };
    let len = cur.position() as usize;
    if res.is_err() {
        let mut end = len.min(buf.len() - 1);
        // Back up past any UTF-8 continuation bytes so truncation ends on a
        // character boundary before overwriting the final byte with '\n'.
        while end > 0 && (buf[end] & 0b1100_0000) == 0b1000_0000 {
            end -= 1;
        }
        buf[end] = b'\n';
        return &buf[..=end];
    }
    &buf[..len]
}

fn write_stderr_all(mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // SAFETY: writing a valid stack slice directly to STDERR_FILENO.
        let n = unsafe { libc::write(libc::STDERR_FILENO, bytes.as_ptr().cast(), bytes.len()) };
        if n > 0 {
            bytes = &bytes[n as usize..];
        } else if n < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        } else {
            break;
        }
    }
}

fn run(init_start_ns: u64) -> io::Result<()> {
    let paths = if transport::is_host_process() {
        // On a Linux host: the host's kernel filesystems, network and clock.
        host_dirs()?
    } else {
        leave_initramfs()?;
        mount_kernel_filesystems()?;
        sys::sethostname(b"asagiri")?;
        // Containers may bind the VM's /etc/hostname, as on a Linux host.
        let _ = std::fs::write("/etc/hostname", "asagiri\n");
        sys::loopback_up()?;
        data_dirs()?
    };

    let served = serve(&paths, init_start_ns);
    if transport::is_host_process() {
        // However the agent stops (Shutdown, its daemon gone, SIGTERM, an
        // error), nothing of the engine stays behind on a Linux host.
        let cleaned = container::sweep(&paths, &paths.cgroup);
        net::remove_host_network();
        return match (served, cleaned) {
            (Err(serve), Err(cleanup)) => Err(io::Error::new(serve.kind(), format!("{serve}; cleanup: {cleanup}"))),
            (Err(error), _) | (_, Err(error)) => Err(error),
            _ => Ok(()),
        };
    }
    served
}

fn serve(paths: &container::Paths, init_start_ns: u64) -> io::Result<()> {
    let mut conn = dial_host()?;
    let (mem_total_kib, mem_available_kib) = meminfo();
    let mut out = Vec::with_capacity(256);
    Msg::Hello(Hello { protocol: PROTOCOL, init_start_ns, ready_ns: boottime_ns(), mem_total_kib, mem_available_kib })
        .encode(&mut out);
    conn.write_all(&out)?;
    agent::serve(conn, paths)
}

/// Re-roots PID 1 onto a tmpfs. The kernel refuses pivot_root while the
/// current root is the initramfs, and containers need pivot_root (chroot
/// alone can be escaped). This is what switch_root does for normal systems.
fn leave_initramfs() -> io::Result<()> {
    sys::mkdir(c"/root", 0o755);
    mount(c"root", c"/root", c"tmpfs", 0, Some(c"mode=0755"))?;
    // /lib/modules for containers that bind it (kind does); the kernel has
    // no modules, so it stays empty.
    for d in [c"/root/dev", c"/root/proc", c"/root/sys", c"/root/run", c"/root/tmp", c"/root/etc", c"/root/var", c"/root/lib", c"/root/lib/modules"] {
        sys::mkdir(d, 0o755);
    }
    // Keep the optional checkpoint runtime visible after moving to the tmpfs root.
    if std::path::Path::new("/bundle").try_exists()? {
        std::fs::create_dir("/root/bundle")?;
        mount(c"/bundle", c"/root/bundle", c"", libc::MS_BIND, None)?;
        mount(c"", c"/root/bundle", c"", libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV, None)?;
    }
    // Free the initramfs copy of this binary; the running image stays mapped.
    let _ = std::fs::remove_file("/init");
    // SAFETY: plain path syscalls with valid NUL-terminated strings.
    unsafe {
        check(libc::chdir(c"/root".as_ptr()) as _, "chdir /root")?;
        check(libc::mount(c".".as_ptr(), c"/".as_ptr(), std::ptr::null(), libc::MS_MOVE, std::ptr::null()) as _, "move root")?;
        check(libc::chroot(c".".as_ptr()) as _, "chroot")?;
        check(libc::chdir(c"/".as_ptr()) as _, "chdir /")?;
    }
    // The console descriptors were opened from the old root's /dev/console
    // and keep working after the root switch.
    Ok(())
}

fn mount_kernel_filesystems() -> io::Result<()> {
    let nosuid_nodev_noexec = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    let m = |src, target: &std::ffi::CStr, fstype, flags, data| {
        sys::mkdir(target, 0o755);
        mount(src, target, fstype, flags, data)
    };
    m(c"proc", c"/proc", c"proc", nosuid_nodev_noexec, None)?;
    m(c"sysfs", c"/sys", c"sysfs", nosuid_nodev_noexec, None)?;
    m(c"devtmpfs", c"/dev", c"devtmpfs", libc::MS_NOSUID, Some(c"mode=0755"))?;
    m(c"devpts", c"/dev/pts", c"devpts", libc::MS_NOSUID | libc::MS_NOEXEC, Some(c"ptmxmode=0666,mode=0620"))?;
    m(c"shm", c"/dev/shm", c"tmpfs", libc::MS_NOSUID | libc::MS_NODEV, Some(c"mode=1777"))?;
    m(c"cgroup2", c"/sys/fs/cgroup", c"cgroup2", nosuid_nodev_noexec, Some(c"nsdelegate,memory_recursiveprot"))?;
    m(c"run", c"/run", c"tmpfs", libc::MS_NOSUID | libc::MS_NODEV, Some(c"mode=0755"))?;
    m(c"tmp", c"/tmp", c"tmpfs", libc::MS_NOSUID | libc::MS_NODEV, Some(c"mode=1777"))?;
    Ok(())
}

/// On a Linux host: images and containers under /var/lib/asagiri, as in the
/// VM, and containers under the `asagiri` cgroup.
///
/// /var/lib/asagiri is 0710 (the asagiri group may pass through it, no one may
/// list it) and what is under it 0700: layer contents and container
/// directories are root's; containers reach them through their mounts. An
/// existing tree is tightened too, in case an older build left it 0755.
/// The umask is set here, not inherited: the unit's UMask=0027 is right for
/// the daemon, but the files this process makes for containers (their
/// /etc/hosts and resolv.conf, bind-mount targets, new volumes) and the
/// umask the containers' own processes start with must stay conventional
/// (022), or nothing running as a non-root user in a container could read
/// its own resolv.conf.
fn host_dirs() -> io::Result<container::Paths> {
    // SAFETY: umask only changes this process's file-creation mask.
    unsafe { libc::umask(0o022) };
    let root = PathBuf::from(DATA);
    if let Some(data) = std::env::var_os("ASAGIRI_AGENT_DATA").map(PathBuf::from).filter(|p| p.is_absolute() && p != &root) {
        mount_agent_data(&data, &root)?;
    }
    let mkdir = |p: &std::path::Path, mode: u32| -> io::Result<()> {
        std::fs::create_dir_all(p)?;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
    };
    mkdir(&root, 0o710)?;
    mkdir(&root.join("volumes"), 0o700)?;
    take_over(&root)?;
    let paths = container::Paths { layers: root.join("layers"), containers: root.join("containers"), cgroup: host_cgroup()? };
    mkdir(&paths.layers, 0o700)?;
    mkdir(&paths.containers, 0o700)?;
    // What an agent that died uncleanly left running: under the cgroup it
    // used (recorded below), and under this one.
    let record = root.join("agent.cgroup");
    if let Ok(old) = std::fs::read_to_string(&record) {
        let old = PathBuf::from(old.trim());
        if old != paths.cgroup && old.starts_with("/sys/fs/cgroup") {
            container::sweep(&paths, &old)?;
        }
    }
    container::sweep(&paths, &paths.cgroup)?;
    std::fs::write(&record, paths.cgroup.as_os_str().as_encoded_bytes())?;
    std::env::set_current_dir(&paths.layers)?;
    Ok(paths)
}

/// Bind-mounts `src` (the chosen folder's `agent` directory, from
/// `ASAGIRI_AGENT_DATA`) onto `/var/lib/asagiri` so every agent path stays under
/// `/var/lib/asagiri` while living on the chosen disk. No-op when `src` is
/// already mounted there; detaches any other mount on `target` first.
fn mount_agent_data(src: &std::path::Path, target: &std::path::Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    std::fs::create_dir_all(src)?;
    std::fs::create_dir_all(target)?;
    let (sm, tm) = (std::fs::metadata(src)?, std::fs::metadata(target)?);
    if (sm.dev(), sm.ino()) == (tm.dev(), tm.ino()) {
        return Ok(());
    }
    let c_target = CString::new(target.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    if volmount::mounted(DATA) {
        // SAFETY: valid NUL-terminated path; detaches a stale mount at `target`.
        check(unsafe { libc::umount2(c_target.as_ptr(), libc::MNT_DETACH) } as _, "detach previous agent data mount")?;
    }
    let c_src = CString::new(src.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: valid NUL-terminated paths for a bind mount (fstype and data unused).
    check(unsafe { libc::mount(c_src.as_ptr(), c_target.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) } as _, "bind-mount ASAGIRI_AGENT_DATA").map(drop)
}


/// The lock on a Linux host's data directory, held while the agent runs.
static AGENT_LOCK: std::sync::OnceLock<File> = std::sync::OnceLock::new();

/// One agent per data directory on a Linux host: takes `agent.lock` (an
/// flock, released when the process ends however it ends). An agent that
/// still holds it is stopped if its daemon is gone (an orphan: SIGTERM, so
/// it cleans up, then SIGKILL), and waited for a few seconds otherwise (a
/// daemon still shutting down); if it stays, starting fails and says why
/// rather than failing later on its sockets and ports.
fn take_over(root: &std::path::Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let path = root.join("agent.lock");
    let mut file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(&path)?;
    let locked = |f: &File| {
        // SAFETY: flock on a descriptor we own.
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
    };
    let start = std::time::Instant::now();
    let mut signalled = None;
    while !locked(&file) {
        let mut text = String::new();
        let _ = (&file).read_to_string(&mut text);
        let _ = std::io::Seek::rewind(&mut file);
        let old: libc::pid_t = text.trim().parse().unwrap_or(0);
        let daemon = parent_of(old);
        let orphan = old > 0 && !daemon.is_some_and(is_daemon);
        let waited = start.elapsed();
        if orphan && signalled.is_none() {
            eprintln!("asagiri-init: stopping agent {old}, left running by a daemon that is gone");
            // SAFETY: signalling the process that holds our data directory's lock.
            unsafe { libc::kill(old, libc::SIGTERM) };
            signalled = Some(waited);
        } else if orphan && signalled.is_some_and(|t| waited - t > std::time::Duration::from_secs(5)) {
            // SAFETY: as above; it did not stop on SIGTERM.
            unsafe { libc::kill(old, libc::SIGKILL) };
        }
        if waited > std::time::Duration::from_secs(8) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another Asagiri engine is running on this machine (agent {old}, daemon {}): stop it first", daemon.unwrap_or(0)),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    file.set_len(0)?;
    use std::os::unix::fs::FileExt;
    file.write_all_at(format!("{}\n", std::process::id()).as_bytes(), 0)?;
    let _ = AGENT_LOCK.set(file);
    Ok(())
}

/// A live process's parent, from /proc/<pid>/stat.
fn parent_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // pid (comm) state ppid …; comm may hold spaces and parentheses.
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// Whether a process is a `asagiri daemon` (by its arguments: the binary may
/// have any name). PID 1 or a subreaper adopting an orphan is not.
fn is_daemon(pid: libc::pid_t) -> bool {
    pid > 1 && std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| c.split(|b| *b == 0).any(|a| a == b"daemon"))
}

/// The cgroup containers go under on a Linux host, chosen as dockerd and
/// containerd choose theirs on cgroup v2:
///
/// - In a cgroup delegated to Asagiri (a systemd service with `Delegate=yes`,
///   or one made for it that holds only the daemon and this agent): those
///   processes, and no others, move to its `asagiri-host` leaf, so it can hand
///   controllers down; containers go in its `asagiri`.
/// - At the root of a cgroup namespace that holds processes (Asagiri inside a
///   container): they move to a `asagiri-host` leaf, as Docker-in-Docker's
///   entrypoint does; containers go in `asagiri` beside it.
/// - Anywhere else (a login session, WSL's `init.scope`: cgroups shared
///   with processes that are not Asagiri's): nothing moves, and containers go
///   in `asagiri` at the top of the hierarchy, as dockerd's cgroupfs driver
///   puts them in `docker`. Moving every process of a shared cgroup took
///   WSL's own along, one level deeper on every start.
///
/// Started again from inside a `asagiri-host` leaf (the daemon stays there
/// when its agent restarts), it is not nested again.
fn host_cgroup() -> io::Result<PathBuf> {
    let own = std::fs::read_to_string("/proc/self/cgroup")?;
    let rel = own.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or("/").trim().trim_start_matches('/').to_owned();
    let root = PathBuf::from("/sys/fs/cgroup");
    if !root.join("cgroup.controllers").exists() {
        mount_cgroup2(&root)?;
    }
    let own = root.join(&rel);
    let mut base = own.clone();
    while base != root && base.file_name().is_some_and(|n| n == "asagiri-host") {
        base.pop();
    }
    let procs = |dir: &std::path::Path| -> io::Result<Vec<String>> {
        Ok(std::fs::read_to_string(dir.join("cgroup.procs"))?.lines().map(str::to_owned).collect())
    };
    let move_processes = if base == root {
        // Only the hierarchy's true root (which has no cgroup.type) may hold
        // processes and hand out controllers; a namespace's root is not it.
        root.join("cgroup.type").exists()
    } else if delegated(&own, &procs(&own)?) {
        true
    } else {
        base = root.clone();
        false
    };
    let asagiri = base.join("asagiri");
    let leaf = base.join("asagiri-host");
    let to_leaf = |from: &std::path::Path| -> io::Result<()> {
        let pids = procs(from)?;
        // A custom cgroup can cease to be exclusive between snapshots. Do
        // not pull unrelated login/WSL processes into Asagiri's leaf.
        if base != root && !delegated(&base, &pids) {
            return Err(io::Error::new(io::ErrorKind::ResourceBusy,
                format!("{} now contains processes outside Asagiri", from.display())));
        }
        for pid in pids {
            if let Err(error) = std::fs::write(leaf.join("cgroup.procs"), &pid) {
                // A short-lived readiness probe may exit after the snapshot.
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(io::Error::new(error.kind(),
                        format!("move process {pid} from {} to {}: {error}", from.display(), leaf.display())));
                }
            }
        }
        Ok(())
    };
    if move_processes {
        make_cgroup(&leaf)?;
        if own != base && own != leaf {
            to_leaf(&own)?;
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if move_processes {
            to_leaf(&base)?;
        }
        match containers_cgroup(&asagiri) {
            Ok(()) => return Ok(asagiri),
            // Docker exec readiness probes can enter a namespace root just
            // after its process snapshot. Retry only in our delegated scope;
            // an atomic controller write leaves it a domain on failure.
            Err(error) if move_processes && error.kind() == io::ErrorKind::ResourceBusy
                && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

fn make_cgroup(path: &std::path::Path) -> io::Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(io::Error::new(error.kind(), format!("create cgroup {}: {error}", path.display()))),
    }
}

fn enable_controllers(path: &std::path::Path) -> io::Result<()> {
    let available = std::fs::read_to_string(path.join("cgroup.controllers"))?;
    let mut controllers = String::from("+cpu +memory +pids");
    for required in ["cpu", "memory", "pids"] {
        if !available.split_whitespace().any(|name| name == required) {
            return Err(io::Error::new(io::ErrorKind::Unsupported,
                format!("{} is missing the delegated {required} controller", path.display())));
        }
    }
    for optional in ["cpuset", "io"] {
        if available.split_whitespace().any(|name| name == optional) {
            controllers.push_str(" +");
            controllers.push_str(optional);
        }
    }
    // One write is all-or-nothing. Separate +cpu/+pids writes can turn an
    // occupied domain into "domain threaded" before +memory fails, leaving
    // newly created container cgroups invalid and accounting incomplete.
    std::fs::write(path.join("cgroup.subtree_control"), controllers)
        .map_err(|error| io::Error::new(error.kind(), format!("delegate controllers under {}: {error}", path.display())))
}

/// Makes the cgroup containers go under, with the controllers handed down
/// to it and from it. Also when a container starts and it is gone: systemd
/// removes a stopped service's cgroups when it sees them empty, and on a
/// restart that can land after the new agent made this one, still empty
/// (Fedora 44's systemd 259 did); `asagiri-host`, which holds the daemon,
/// stays, and every container start then failed with ENOENT.
pub fn containers_cgroup(asagiri: &std::path::Path) -> io::Result<()> {
    let base = asagiri.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cgroup has no parent"))?;
    make_cgroup(asagiri)?;
    enable_controllers(base)?;
    enable_controllers(asagiri)
}

/// `ip netns exec` mounts a fresh /sys for its network namespace, without
/// the cgroup tree, and every container start then failed with ENOENT. An
/// empty /sys/fs/cgroup gets cgroup2 mounted on it (in that command's
/// private mount namespace, so nothing outside changes); anything else
/// there (cgroup v1) fails the start now, saying why.
fn mount_cgroup2(root: &std::path::Path) -> io::Result<()> {
    let empty = std::fs::read_dir(root).map(|mut d| d.next().is_none());
    match empty {
        Ok(true) => mount(c"cgroup2", c"/sys/fs/cgroup", c"cgroup2", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None)
            .map_err(|e| io::Error::new(e.kind(), format!("/sys/fs/cgroup has no cgroup tree (ip netns exec remounts /sys) and mounting cgroup2 there failed: {e}"))),
        Ok(false) => Err(io::Error::new(io::ErrorKind::Unsupported, "Asagiri needs cgroup v2 at /sys/fs/cgroup (this machine has cgroup v1 or a hybrid tree there)")),
        Err(e) => Err(io::Error::new(e.kind(), format!("/sys/fs/cgroup: {e}; Asagiri needs cgroup v2 mounted there"))),
    }
}

/// Whether the agent's cgroup is Asagiri's own: a systemd service's (with
/// `Delegate=yes`, the whole cgroup is the service's), or one holding no
/// process but this agent and its parent, the daemon.
fn delegated(own: &std::path::Path, pids: &[String]) -> bool {
    if own.extension().is_some_and(|e| e == "service") {
        return true;
    }
    let (me, parent) = (std::process::id().to_string(), std::os::unix::process::parent_id().to_string());
    pids.iter().all(|p| *p == me || *p == parent)
}

/// Mounts the data disk (or a tmpfs without one), creates the layer and
/// container directories and the cgroup that all containers live under.
fn data_dirs() -> io::Result<container::Paths> {
    sys::mkdir(c"/var/lib", 0o755);
    sys::mkdir(c"/var/lib/asagiri", 0o755);
    // noinit_itable: the disk is made with lazy inode tables; zeroing them in
    // the background would only write gigabytes of zeros to the host.
    let disk = mount(c"/dev/vda", c"/var/lib/asagiri", c"ext4", libc::MS_NOATIME, Some(c"noinit_itable"));
    if let Err(ref e) = disk {
        eprintln!("asagiri-init: no data disk ({e}); keeping images in memory");
        mount(c"asagiri", c"/var/lib/asagiri", c"tmpfs", libc::MS_NOATIME, Some(c"mode=0755"))?;
    } else {
        disk::grow()?;
        if let Err(e) = disk::configure_swap() {
            eprintln!("asagiri-init: guest swap unavailable ({e}); continuing without swap");
        }
    }
    // The Mac's filesystem over virtiofs, for bind mounts.
    sys::mkdir(c"/mnt", 0o755);
    sys::mkdir(c"/mnt/mac", 0o755);
    if let Err(e) = mount(c"asagiri-mac", c"/mnt/mac", c"virtiofs", libc::MS_NOATIME, None) {
        eprintln!("asagiri-init: no Mac file share ({e}); bind mounts of Mac paths will fail");
    } else if let Err(e) = mac_readahead() {
        eprintln!("asagiri-init: keeping the Mac file share's default read-ahead ({e})");
    }
    // An x86-64 guest (Intel Macs, WSL2) runs amd64 binaries natively.
    #[cfg(target_arch = "aarch64")]
    rosetta();
    // An x86-64 guest has no Rosetta for Linux: arm64 images run through
    // qemu-user, when the Mac shares it in.
    #[cfg(target_arch = "x86_64")]
    arm64_emulator();
    std::fs::create_dir_all(format!("{DATA}/volumes"))?;
    let root = PathBuf::from(DATA);
    let paths = container::Paths {
        layers: root.join("layers"),
        containers: root.join("containers"),
        cgroup: PathBuf::from("/sys/fs/cgroup/asagiri"),
    };
    std::fs::create_dir_all(&paths.layers)?;
    std::fs::create_dir_all(&paths.containers)?;
    // Stopped containers keep their writable layers across boots; the host
    // sends `Retain` with the ones it still knows about, and the rest go.
    let _ = std::fs::create_dir(&paths.cgroup);
    // cpu, memory and pids for limits; cpuset and io so containers that
    // run their own containers (Kubernetes, Docker in Docker) find them.
    sys::write_file("/sys/fs/cgroup/cgroup.subtree_control", "+cpu +memory +pids")?;
    sys::write_file("/sys/fs/cgroup/asagiri/cgroup.subtree_control", "+cpu +memory +pids")?;
    for c in ["+cpuset", "+io"] {
        let _ = sys::write_file("/sys/fs/cgroup/cgroup.subtree_control", c);
        let _ = sys::write_file("/sys/fs/cgroup/asagiri/cgroup.subtree_control", c);
    }
    // Overlay lower dirs are given relative to the layer store.
    std::env::set_current_dir(&paths.layers)?;
    if disk.is_ok() {
        trim::start(DATA);
    }
    Ok(paths)
}

/// Batch sequential reads across virtiofs without changing cache coherence.
/// Only the Mac share's backing device is changed; Rosetta and the data disk
/// keep their own settings. Older kernels may omit this optional tuning.
fn mac_readahead() -> io::Result<()> {
    let device = std::fs::metadata("/mnt/mac")?.dev();
    let path = format!("/sys/class/bdi/{}:{}/read_ahead_kb", libc::major(device), libc::minor(device));
    sys::write_file(&path, "1024\n")
}

/// x86-64 Linux binaries through Rosetta, when the Mac shares it: amd64
/// images then run as they do on Docker Desktop and OrbStack. The `F` flag
/// opens the interpreter now, so it works inside every container's mounts.
#[cfg(target_arch = "aarch64")]
fn rosetta() {
    sys::mkdir(c"/run/rosetta", 0o755);
    if mount(c"rosetta", c"/run/rosetta", c"virtiofs", libc::MS_RDONLY, None).is_err() {
        return;
    }
    if let Err(e) = mount(c"binfmt_misc", c"/proc/sys/fs/binfmt_misc", c"binfmt_misc", 0, None) {
        eprintln!("asagiri-init: no binfmt_misc ({e}); amd64 images will not run");
        return;
    }
    const X86_64: &str = concat!(
        ":rosetta:M::",
        r"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00",
        ":",
        r"\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff",
        ":/run/rosetta/rosetta:CF",
    );
    if let Err(e) = sys::write_file("/proc/sys/fs/binfmt_misc/register", X86_64) {
        eprintln!("asagiri-init: registering Rosetta: {e}");
    }
}

/// arm64 Linux binaries through qemu-user, when the Mac shares it in: arm64
/// images then run as amd64 ones do on Apple silicon. Rosetta for Linux
/// exists only on Apple silicon, so an Intel Mac ships qemu-aarch64 with the
/// app and shares it over virtiofs, mounted here at `/run/qemu`. The `F` flag
/// opens the interpreter now, so it works inside every container's mounts.
#[cfg(target_arch = "x86_64")]
fn arm64_emulator() {
    sys::mkdir(c"/run/qemu", 0o755);
    if mount(c"qemu", c"/run/qemu", c"virtiofs", libc::MS_RDONLY, None).is_err() {
        return;
    }
    if let Err(e) = mount(c"binfmt_misc", c"/proc/sys/fs/binfmt_misc", c"binfmt_misc", 0, None) {
        eprintln!("asagiri-init: no binfmt_misc ({e}); arm64 images will not run");
        return;
    }
    // AArch64: e_machine 0xB7 at offset 18, ET_EXEC or ET_DYN in e_type.
    const AARCH64: &str = concat!(
        ":qemu-aarch64:M::",
        r"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\xb7\x00",
        ":",
        r"\xff\xff\xff\xff\xff\xff\xff\x00\x00\x00\x00\x00\x00\x00\x00\x00\xfe\xff\xff\xff",
        ":/run/qemu/qemu-aarch64:CF",
    );
    if let Err(e) = sys::write_file("/proc/sys/fs/binfmt_misc/register", AARCH64) {
        eprintln!("asagiri-init: registering qemu-aarch64: {e}");
    }
}

fn dial_host() -> io::Result<File> {
    // The host registers its listener before the VM starts, so the first
    // attempt normally succeeds. Retry briefly in case the vsock device is
    // still being probed.
    let mut last = io::Error::from(io::ErrorKind::NotConnected);
    for attempt in 0..200 {
        match transport::dial(HOST_PORT) {
            Ok(sock) => return Ok(File::from(sock)),
            Err(e) => last = e,
        }
        if attempt > 0 && attempt % 50 == 0 {
            eprintln!("asagiri-init: still dialing host: {last}");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    Err(io::Error::new(last.kind(), format!("dial host: {last}")))
}

/// MemTotal and MemAvailable in KiB.
fn meminfo() -> (u64, u64) {
    let mut buf = [0u8; 2048];
    let Ok(n) = File::open("/proc/meminfo").and_then(|mut f| f.read(&mut buf)) else { return (0, 0) };
    let s = std::str::from_utf8(&buf[..n]).unwrap_or_default();
    let field = |name: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap_or(0)
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

fn power_off() -> ! {
    // SAFETY: sync and reboot take no pointers.
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    // reboot only returns on failure; PID 1 must never exit.
    loop {
        // SAFETY: pause takes no arguments.
        unsafe { libc::pause() };
    }
}

#[cfg(test)]
mod panic_hook_tests {
    #[test]
    fn formats_panic_location_and_payload_into_stack_buffer() {
        let mut buf = [0u8; super::PANIC_BUF_LEN];
        assert_eq!(
            super::format_panic_message(&mut buf, Some(("crates/asagiri-init/src/linux/agent.rs", 42)), Some("boom")),
            b"asagiri-init: panic at crates/asagiri-init/src/linux/agent.rs:42: boom\n"
        );
        assert_eq!(
            super::format_panic_message(&mut buf, None, Some("oops")),
            b"asagiri-init: panic at <unknown location>: oops\n"
        );
        assert_eq!(
            super::format_panic_message(&mut buf, Some(("mod.rs", 7)), None),
            b"asagiri-init: panic at mod.rs:7: a panic payload that is not a string\n"
        );

        // Oversized messages truncate cleanly inside the fixed stack buffer
        // with a trailing newline and valid UTF-8.
        let long = "x".repeat(super::PANIC_BUF_LEN * 2);
        let out = super::format_panic_message(&mut buf, Some(("a.rs", 1)), Some(&long));
        assert_eq!(out.len(), super::PANIC_BUF_LEN);
        assert!(out.ends_with(b"\n"));
        assert!(std::str::from_utf8(out).is_ok());

        let mut small = [0u8; 34];
        // "asagiri-init: panic at a.rs:1: " is 31 bytes; "éééé" has 2-byte UTF-8 chars.
        let out = super::format_panic_message(&mut small, Some(("a.rs", 1)), Some("éééé"));
        assert!(out.ends_with(b"\n"));
        let text = std::str::from_utf8(out).expect("truncated output must stay valid UTF-8");
        assert!(text.starts_with("asagiri-init: panic at a.rs:1: "));

        let mut empty = [];
        assert_eq!(super::format_panic_message(&mut empty, Some(("a.rs", 1)), Some("x")), b"");
    }
}
