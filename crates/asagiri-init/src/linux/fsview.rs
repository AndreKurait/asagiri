//! Read-only views into running containers' filesystems, for browsing them
//! from the Mac. Every path is resolved with openat2(RESOLVE_IN_ROOT): the
//! container's root is `/` even for symlinks inside it, so nothing outside
//! the container is reachable.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use asagiri_proto::FsEntry;

use super::sys;

/// Entries per listing at most: keeps a reply well inside one frame.
pub const MAX_ENTRIES: usize = 5000;
/// Bytes per read at most.
pub const MAX_READ: usize = 512 * 1024;

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_IN_ROOT: u64 = 0x10;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// Opens `path` under `root`, every symlink resolved inside `root`.
pub(super) fn open_in(root: RawFd, path: &str, flags: i32) -> io::Result<OwnedFd> {
    let rel = path.trim_start_matches('/');
    let c = CString::new(if rel.is_empty() { "." } else { rel }).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let how = OpenHow { flags: (flags | libc::O_CLOEXEC) as u64, mode: 0, resolve: RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS };
    // SAFETY: openat2 with a valid dirfd, path and open_how of the stated size.
    let fd = unsafe { libc::syscall(libc::SYS_openat2, root, c.as_ptr(), &how as *const OpenHow, std::mem::size_of::<OpenHow>()) };
    sys::check(fd, "open")?;
    // SAFETY: openat2 returned a fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

fn root_of(rootfs: &Path) -> io::Result<OwnedFd> {
    let c = sys::cstr(rootfs)?;
    // SAFETY: plain open of a directory; owned right away.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    sys::check(fd as _, "open rootfs")?;
    // SAFETY: fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// What the file browser shows of a file.
#[derive(Debug, Clone, Copy)]
pub struct Meta {
    pub mode: u32,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: u32,
    pub ino: u64,
}

impl From<&libc::stat> for Meta {
    fn from(st: &libc::stat) -> Meta {
        Meta { mode: st.st_mode, size: st.st_size as u64, mtime: st.st_mtime, mtime_nsec: st.st_mtime_nsec as u32, ino: st.st_ino }
    }
}

fn stat_fd(fd: RawFd) -> io::Result<libc::stat> {
    // SAFETY: fstat into a zeroed stat.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    sys::check(unsafe { libc::fstat(fd, &mut st) } as _, "stat")?;
    Ok(st)
}

/// Metadata of `path`, following symlinks. RESOLVE_IN_ROOT keeps every link
/// inside the container (`/` in a link means the container's root), so a
/// link to a directory browses like the directory. A dangling link reports
/// as itself.
pub fn stat(rootfs: &Path, path: &str) -> io::Result<Meta> {
    let root = root_of(rootfs)?;
    let fd = match open_in(root.as_raw_fd(), path, libc::O_PATH) {
        Ok(fd) => fd,
        Err(_) => open_in(root.as_raw_fd(), path, libc::O_PATH | libc::O_NOFOLLOW)?,
    };
    Ok(Meta::from(&stat_fd(fd.as_raw_fd())?))
}

/// A directory's entries, sorted by name. Reject listings above `MAX_ENTRIES`.
pub fn list(rootfs: &Path, path: &str) -> io::Result<Vec<(String, Meta)>> {
    let root = root_of(rootfs)?;
    let dir = open_in(root.as_raw_fd(), path, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let mut out = Vec::new();
    // /proc/self/fd/N names the directory we already resolved safely.
    for entry in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(c) = CString::new(name.as_str()) else { continue };
        // SAFETY: fstatat relative to the resolved directory, not following links.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            continue;
        }
        let meta = if st.st_mode & libc::S_IFMT == libc::S_IFLNK {
            // Show what the link points at, resolved inside the container.
            let target = format!("{}/{}", path.trim_end_matches('/'), name);
            stat(rootfs, &target).unwrap_or_else(|_| Meta::from(&st))
        } else {
            Meta::from(&st)
        };
        out.push((name, meta));
        if out.len() > MAX_ENTRIES {
            return Err(io::Error::other("directory exceeds the 5000-entry listing limit"));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Up to `len` bytes from `offset` of a regular file. What the path names is
/// looked at before anything is opened for reading: opening a FIFO waits for
/// a writer, and opening a device does whatever its driver does. The read
/// does not wait either (/proc/kmsg is a regular file whose read waits for
/// the next kernel message). The agent has one thread, so a wait here
/// stopped every container's exec and start until it ended.
pub fn read(rootfs: &Path, path: &str, offset: u64, len: usize, buf: &mut Vec<u8>) -> io::Result<()> {
    let root = root_of(rootfs)?;
    let found = open_in(root.as_raw_fd(), path, libc::O_PATH)?;
    if stat_fd(found.as_raw_fd())?.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    // The file found, opened by its descriptor: no path is resolved again.
    let c = CString::new(format!("/proc/self/fd/{}", found.as_raw_fd())).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: plain open of a path we made; owned right away.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC) };
    sys::check(fd as _, "open")?;
    // SAFETY: fresh descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    buf.resize(len.min(MAX_READ), 0);
    // SAFETY: pread into a buffer we own.
    let n = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), offset as libc::off_t) };
    sys::check(n as _, "read")?;
    buf.truncate(n as usize);
    Ok(())
}

pub fn entry(name: &str, m: Meta) -> FsEntry<'_> {
    FsEntry { name, mode: m.mode, size: m.size, mtime: m.mtime, mtime_nsec: m.mtime_nsec, ino: m.ino }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::time::Duration;

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    // A read of what is not a regular file is refused at once. Opening a
    // FIFO that has no writer waits for one, and the agent has one thread:
    // that read stopped every container's exec and start.
    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_writer() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("asagiri-fsview-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let _cleanup = Cleanup(dir.clone());
        std::fs::write(dir.join("file"), "hello").unwrap();
        std::fs::write(dir.join("empty"), "").unwrap();
        let pipe = std::ffi::CString::new(dir.join("pipe").to_str().unwrap()).unwrap();
        // SAFETY: mkfifo of a path in our own folder.
        assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o644) }, 0);

        let (tx, rx) = std::sync::mpsc::channel();
        let root = dir.clone();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = tx.send(super::read(&root, "/pipe", 0, 16, &mut buf).map_err(|e| e.kind()));
        });
        let answer = rx.recv_timeout(Duration::from_secs(5));
        if answer.is_err() {
            // Release the old blocking implementation too, so a failed
            // regression never leaves its reader thread waiting.
            // SAFETY: the owned fixture's FIFO, opened without waiting.
            let fd = unsafe { libc::open(pipe.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
            if fd >= 0 {
                // SAFETY: close the fresh descriptor exactly once.
                unsafe { libc::close(fd) };
            }
        }
        reader.join().unwrap();
        let answer = answer.expect("the read of a FIFO waited for a writer");
        assert_eq!(answer, Err(ErrorKind::InvalidInput));
        let mut buf = Vec::new();
        assert_eq!(super::read(&dir, "/", 0, 16, &mut buf).map_err(|e| e.kind()), Err(ErrorKind::InvalidInput), "a folder");

        // A regular file reads as before, from any offset.
        super::read(&dir, "/file", 0, 16, &mut buf).unwrap();
        assert_eq!(buf, b"hello");
        super::read(&dir, "/file", 3, 16, &mut buf).unwrap();
        assert_eq!(buf, b"lo");
        super::read(&dir, "/empty", 0, 16, &mut buf).unwrap();
        assert!(buf.is_empty());
        std::os::unix::fs::symlink("/file", dir.join("link")).unwrap();
        super::read(&dir, "/link", 0, 16, &mut buf).unwrap();
        assert_eq!(buf, b"hello", "absolute links stay inside the container root");
        std::os::unix::fs::symlink("/pipe", dir.join("pipe-link")).unwrap();
        assert_eq!(super::read(&dir, "/pipe-link", 0, 16, &mut buf).map_err(|e| e.kind()), Err(ErrorKind::InvalidInput));
    }
}
