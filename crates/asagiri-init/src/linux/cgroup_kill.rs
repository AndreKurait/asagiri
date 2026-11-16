//! Stop an owned cgroup without signaling a recycled numeric process ID.

use std::collections::BTreeSet;
use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(3);

struct Group(File);

impl Group {
    fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
        Self::from_file(file)
    }

    fn from_file(file: File) -> io::Result<Self> {
        let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: fstatfs writes the provided structure; the descriptor is owned.
        if unsafe { libc::fstatfs(file.as_raw_fd(), info.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatfs succeeded. Only genuine cgroup-v2 controls are accepted.
        if unsafe { info.assume_init() }.f_type as u64 != 0x6367_7270 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a cgroup-v2 directory"));
        }
        Ok(Self(file))
    }

    fn control(&self, name: &CStr, write: bool) -> io::Result<File> {
        // Nonblocking open also keeps an overmounted FIFO from hanging before
        // its type and filesystem can be checked.
        let flags = (if write { libc::O_WRONLY } else { libc::O_RDONLY })
            | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        // SAFETY: the name is NUL-terminated and relative to an owned directory.
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 { return Err(io::Error::last_os_error()); }
        // SAFETY: openat returned a new descriptor owned by nobody else.
        let file = unsafe { File::from_raw_fd(fd) };
        let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: fstatfs writes the supplied structure for this owned fd.
        if unsafe { libc::fstatfs(file.as_raw_fd(), info.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatfs succeeded. A genuine directory can still have an
        // ordinary file mounted over one control; verify the opened file.
        if unsafe { info.assume_init() }.f_type as u64 != 0x6367_7270 || !file.metadata()?.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a cgroup-v2 control file"));
        }
        Ok(file)
    }

    fn read(&self, name: &CStr) -> io::Result<String> {
        let mut value = String::new();
        self.control(name, false)?.read_to_string(&mut value)?;
        Ok(value)
    }

    fn write(&self, name: &CStr, value: &[u8]) -> io::Result<()> {
        self.control(name, true)?.write_all(value)
    }

    fn removed(&self) -> bool {
        self.0.metadata().is_ok_and(|metadata| metadata.nlink() == 0)
    }

    fn event(&self, key: &str) -> io::Result<bool> {
        event(&self.read(c"cgroup.events")?, key)
    }

    fn processes(&self) -> io::Result<BTreeSet<libc::pid_t>> {
        self.read(c"cgroup.procs")?.split_whitespace().map(|value| {
            value.parse::<libc::pid_t>().ok().filter(|pid| *pid > 0)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unidentifiable cgroup process"))
        }).collect()
    }

    fn child(&self, name: &std::ffi::OsStr) -> io::Result<Self> {
        let name = CString::new(name.as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // SAFETY: a relative NUL-terminated name, with symlinks refused.
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 { return Err(io::Error::last_os_error()); }
        // SAFETY: openat returned a newly owned descriptor. Check the filesystem
        // again: an overmounted child must not supply ordinary control files.
        Self::from_file(unsafe { File::from_raw_fd(fd) })
    }
}

fn event(text: &str, key: &str) -> io::Result<bool> {
    let mut found = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some(key) { continue; }
        let value = match fields.next() {
            Some("0") => false,
            Some("1") => true,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid cgroup event")),
        };
        if found.replace(value).is_some() || fields.next().is_some() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "ambiguous cgroup event"));
        }
    }
    found.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("missing cgroup event {key}")))
}

fn before(deadline: Instant) -> io::Result<()> {
    if Instant::now() < deadline { Ok(()) } else {
        Err(io::Error::new(io::ErrorKind::TimedOut, "cgroup stop did not finish within the wait deadline"))
    }
}

fn wait(group: &Group, key: &str, value: bool, deadline: Instant) -> io::Result<()> {
    loop {
        if group.removed() || group.event(key)? == value { return Ok(()); }
        before(deadline)?;
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn pidfd(pid: libc::pid_t) -> io::Result<OwnedFd> {
    // SAFETY: opens a handle to this process identity; no signal is sent.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: the syscall returned a new descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as _) })
}

fn signal(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: SIGKILL is addressed to the pinned process identity, never a PID.
    let result = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), libc::SIGKILL, std::ptr::null::<libc::siginfo_t>(), 0u32) };
    if result == 0 { return Ok(()); }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) { Ok(()) } else { Err(error) }
}

fn signal_members(group: &Group, snapshot: &[libc::pid_t], deadline: Instant) -> io::Result<usize> {
    let mut count = 0;
    for batch in snapshot.chunks(32) {
        before(deadline)?;
        let mut pinned = Vec::new();
        for &pid in batch {
            match pidfd(pid) {
                Ok(fd) => pinned.push((pid, fd)),
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                Err(error) => return Err(error),
            }
        }
        count += signal_pinned_members(group, pinned)?;
    }
    Ok(count)
}

fn signal_pinned_members(group: &Group, pinned: Vec<(libc::pid_t, OwnedFd)>) -> io::Result<usize> {
    // A PID from the earlier snapshot could have been recycled before
    // pidfd_open. Verify membership after pinning, then signal only the fd.
    // A privileged external migration after this check is not atomic on old
    // kernels. cgroup.kill, when available, also handles concurrent migration.
    let current = group.processes()?;
    let mut count = 0;
    for (pid, fd) in pinned {
        if current.contains(&pid) {
            signal(&fd)?;
            count += 1;
        }
    }
    Ok(count)
}

fn signal_tree(group: &Group, deadline: Instant) -> io::Result<()> {
    let result = (|| {
        before(deadline)?;
        // The domain root's cgroup.procs includes its entire threaded subtree.
        // Reading cgroup.procs in a threaded child is unsupported.
        match group.read(c"cgroup.type")?.trim() {
            "threaded" | "domain invalid" => return Ok(()),
            "domain" | "domain threaded" => {}
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown cgroup type")),
        }
        let directory = format!("/proc/self/fd/{}", group.0.as_raw_fd());
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() { continue; }
            match group.child(&entry.file_name()) {
                Ok(child) => signal_tree(&child, deadline)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        let snapshot: Vec<_> = group.processes()?.into_iter().collect();
        signal_members(group, &snapshot, deadline)?;
        Ok(())
    })();
    if group.removed() { Ok(()) } else { result }
}

fn fallback_allowed(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT | libc::EOPNOTSUPP))
}

fn kill_open(group: &Group, force_pidfds: bool, deadline: Instant) -> io::Result<()> {
    if !force_pidfds {
        match group.write(c"cgroup.kill", b"1") {
            Ok(()) => return wait(group, "populated", false, deadline),
            Err(error) if fallback_allowed(&error) => {}
            Err(error) => return Err(error),
        }
    }
    if group.removed() || !group.event("populated")? { return Ok(()); }
    if !matches!(group.read(c"cgroup.type")?.trim(), "domain" | "domain threaded") {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "cannot stop a threaded cgroup independently of its domain"));
    }
    let frozen = match group.read(c"cgroup.freeze")?.trim() {
        "0" => false,
        "1" => true,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid cgroup freezer state")),
    };
    if !frozen { group.write(c"cgroup.freeze", b"1")?; }
    let stopped: io::Result<()> = (|| {
        wait(group, "frozen", true, deadline)?;
        while !group.removed() && group.event("populated")? {
            signal_tree(group, deadline)?;
            if group.removed() || !group.event("populated")? { break; }
            before(deadline)?;
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    })();
    // Restore only a state we changed, including after a failure or timeout.
    let restored = if frozen || group.removed() { Ok(()) } else { group.write(c"cgroup.freeze", b"0") };
    match (stopped, restored) {
        (Err(stop), Err(thaw)) => Err(io::Error::new(stop.kind(), format!("{stop}; restore freezer: {thaw}"))),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

fn kill_mode(path: &Path, force_pidfds: bool) -> io::Result<()> {
    let group = match Group::open(path) {
        Ok(group) => group,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let result = kill_open(&group, force_pidfds, Instant::now() + WAIT);
    if group.removed() { Ok(()) } else { result }
}

pub(super) fn kill(path: &Path) -> io::Result<()> {
    kill_mode(path, false).map_err(|error| io::Error::new(error.kind(), format!("stop cgroup {}: {error}", path.display())))
}

#[cfg(test)]
#[path = "cgroup_kill_tests.rs"]
mod tests;
