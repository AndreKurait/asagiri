//! The guest's layer store: compressed layer blobs arrive from the host over
//! vsock and are unpacked once into `layers/<hex>/`, ready to be stacked by
//! overlayfs. A layer directory only appears, by rename, once it is complete,
//! and is only used once `<hex>.done` beside it says its files are on disk:
//! a VM that stops before the guest flushes can keep a layer's names but
//! lose its contents, leaving every file empty.

use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use asagiri_proto::{LayerBegin, LayerStored, Msg};

use super::sys;

pub fn listen() -> io::Result<OwnedFd> {
    super::transport::listen(asagiri_proto::LAYER_PORT)
}

/// The agent remains single-threaded for safe container forks. Each layer
/// gets a separate worker process and connection; host-side permits bound
/// simultaneous transfers.
pub fn accept(listener: &OwnedFd, root: &Path) {
    loop {
        // SAFETY: accept from our live listening socket.
        let fd = unsafe { libc::accept4(listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
        if fd < 0 {
            return;
        }
        // SAFETY: accept4 returned a new owned descriptor.
        let socket = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: getters and fork in the agent's single-threaded event loop,
        // following the same process isolation as the archive-copy service.
        let parent = unsafe { libc::getpid() };
        match unsafe { libc::fork() } {
            0 => {
                // Do not retain the control socket, container pipes or other
                // workers' sockets. A stopped agent also stops this worker.
                // SAFETY: descriptor ranges exclude our socket; prctl uses
                // a valid death signal. Check the parent-death race as well.
                unsafe {
                    if fd > 3 {
                        libc::syscall(libc::SYS_close_range, 3, fd - 1, 0);
                    }
                    libc::syscall(libc::SYS_close_range, fd + 1, libc::c_uint::MAX, 0);
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 || libc::getppid() != parent {
                        libc::_exit(1);
                    }
                }
                let mut connection = fs::File::from(socket);
                let result = upload(&mut connection, root);
                if let Err(error) = &result {
                    let mut reply = Vec::new();
                    Msg::Error(&format!("layer upload: {error}")).encode(&mut reply);
                    let _ = connection.write_all(&reply);
                }
                // SAFETY: exit the forked child without parent destructors.
                unsafe { libc::_exit(i32::from(result.is_err())) }
            }
            pid if pid < 0 => eprintln!("asagiri-init: layer fork: {}", io::Error::last_os_error()),
            _ => drop(socket),
        }
    }
}

fn upload(connection: &mut fs::File, root: &Path) -> io::Result<()> {
    let mut header = [0; 4];
    connection.read_exact(&mut header)?;
    let size = asagiri_proto::frame_len(&header).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
    let mut frame = vec![0; size];
    frame[..4].copy_from_slice(&header);
    connection.read_exact(&mut frame[4..])?;
    let Msg::LayerBegin(begin) = asagiri_proto::decode(&frame)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))? else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected LayerBegin"));
    };
    let stored = receive(connection, &begin, root)?;
    let mut reply = Vec::new();
    Msg::LayerStored(stored).encode(&mut reply);
    connection.write_all(&reply)
}

pub fn hex(digest: &str) -> io::Result<&str> {
    let h = digest.strip_prefix("sha256:").unwrap_or(digest);
    if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("bad layer digest {digest}")));
    }
    Ok(h)
}

pub fn dir(root: &Path, digest: &str) -> io::Result<PathBuf> {
    Ok(root.join(hex(digest)?))
}

fn marker(root: &Path, digest: &str) -> io::Result<PathBuf> {
    Ok(root.join(format!("{}.done", hex(digest)?)))
}

pub fn present(root: &Path, digest: &str) -> bool {
    dir(root, digest).is_ok_and(|d| d.is_dir()) && marker(root, digest).is_ok_and(|m| m.is_file())
}

/// Reads exactly `begin.size` bytes of blob from `stream` and unpacks them.
/// The stream stays in sync even if unpacking fails part-way.
pub fn receive(stream: &mut impl Read, begin: &LayerBegin<'_>, root: &Path) -> io::Result<LayerStored> {
    let started = Instant::now();
    let final_dir = dir(root, begin.digest)?;
    fs::create_dir_all(root)?;
    // Protect the temporary directory and atomic install even if two
    // independent upload connections name the same digest.
    let lock = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(root.join(format!(".lock-{}", hex(begin.digest)?)))?;
    loop {
        // SAFETY: lock owns a live descriptor until receive returns.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    let tmp = root.join(format!(".tmp-{}", hex(begin.digest)?));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;

    let mut limited = stream.take(begin.size);
    let result = unpack(&mut limited, begin.media_type, &tmp, begin.size);
    // Whatever happened, consume the rest of the blob so the next frame lines up.
    let drained = io::copy(&mut limited, &mut io::sink()).and_then(|_| {
        if limited.limit() == 0 {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::UnexpectedEof, "layer transfer ended before its declared size"))
        }
    });
    let result = result.and_then(|bytes| drained.map(|()| bytes));

    match result.and_then(|unpacked_bytes| install(root, begin.digest, &tmp, &final_dir).map(|()| unpacked_bytes)) {
        Ok(unpacked_bytes) => Ok(LayerStored { unpacked_bytes, elapsed_ns: started.elapsed().as_nanos() as u64 }),
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            Err(e)
        }
    }
}

/// Puts an unpacked layer in place: its files reach the disk first, then
/// the directory is renamed in, then the marker says it is whole.
fn install(root: &Path, digest: &str, tmp: &Path, final_dir: &Path) -> io::Result<()> {
    if present(root, digest) {
        let _ = fs::remove_dir_all(tmp);
        return Ok(());
    }
    sync_tree(tmp)?;
    if final_dir.exists() {
        // A copy without a marker was cut short and may hold empty files.
        // Moved aside rather than deleted here, which can take a while;
        // the next RetainLayers clears it.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        fs::rename(final_dir, root.join(format!(".stale-{}-{nanos}", hex(digest)?)))?;
    }
    fs::rename(tmp, final_dir)?;
    fs::File::create(marker(root, digest)?)?.sync_all()?;
    fs::File::open(root)?.sync_all()
}

/// Flush this worker's completed unpack, children before their directories.
/// On native Linux the layer store may share a filesystem with unrelated
/// workloads; syncfs would wait for all of their dirty files too. Symlinks and
/// special nodes have no regular file data to flush; their directory persists
/// their creation. Never open a device/FIFO or follow an image's symlink.
fn sync_tree(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let directory = fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            fs::OpenOptions::new().read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(entry.path())?.sync_all()?;
        }
    }
    directory.sync_all()
}

/// Deletes every layer not in `keep` (blob digests), every layer without
/// its marker, and whatever an unpack left half-done. Anything that isn't
/// one of those names is left alone.
pub fn retain<'a>(root: &Path, keep: impl Iterator<Item = &'a str>) {
    let keep: std::collections::HashSet<&str> = keep.filter_map(|d| hex(d).ok()).collect();
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let leftover = name.starts_with(".tmp-") || name.starts_with(".stale-") || name.starts_with(".lock-");
        let (h, is_marker) = match name.strip_suffix(".done") {
            Some(h) => (h, true),
            None => (name, false),
        };
        let layer = h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit());
        if !leftover && !layer {
            continue;
        }
        let whole = layer && root.join(h).is_dir() && root.join(format!("{h}.done")).is_file();
        if leftover || !keep.contains(h) || !whole {
            let path = entry.path();
            let _ = if is_marker || !path.is_dir() { fs::remove_file(&path) } else { fs::remove_dir_all(&path) };
        }
    }
}

/// The most a layer may expand to. The blob's digest covers only its
/// compressed bytes, so a tiny, digest-valid layer could otherwise expand
/// without limit onto the disk (on a Linux host or in WSL, the host's own).
/// 256x the compressed size, never below 1 GiB nor above 64 GiB.
fn expansion_cap(compressed: u64) -> u64 {
    compressed.saturating_mul(256).clamp(1 << 30, 64 << 30)
}

/// A reader that fails once more than `left` bytes have come through.
struct Capped<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n as u64 > self.left {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "layer expands beyond its allowed size"));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

fn unpack(blob: &mut impl Read, media_type: &str, dest: &Path, compressed: u64) -> io::Result<u64> {
    let buffered = BufReader::with_capacity(256 * 1024, blob);
    let left = expansion_cap(compressed);
    if media_type.ends_with("gzip") {
        untar(Capped { inner: flate2::read::MultiGzDecoder::new(buffered), left }, dest)
    } else if media_type.ends_with("zstd") {
        let decoder = ruzstd::decoding::StreamingDecoder::new(buffered)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("zstd: {e}")))?;
        untar(Capped { inner: decoder, left }, dest)
    } else {
        untar(Capped { inner: buffered, left }, dest)
    }
}

/// Unpacks a layer tarball, translating OCI whiteouts into the form overlayfs
/// reads: `.wh.name` becomes a 0:0 character device `name`, and
/// `.wh..wh..opq` marks its directory opaque.
fn untar(reader: impl Read, dest: &Path) -> io::Result<u64> {
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_preserve_mtime(true);
    archive.set_unpack_xattrs(true);
    archive.set_overwrite(true);
    let mut total = 0u64;
    let mut dir_times = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path.file_name().map(|n| n.as_bytes()).unwrap_or_default();
        if let Some(hidden) = name.strip_prefix(b".wh.") {
            // `.wh.`, `.wh..` and `.wh...` would hide the directory itself or its parent.
            if matches!(hidden, b"" | b"." | b"..") {
                return Err(escapes());
            }
            let parent = whiteout_dir(dest, path.parent().unwrap_or(Path::new("")))?;
            if hidden == b".wh..opq" {
                let p = sys::cstr(&parent)?;
                // SAFETY: valid NUL-terminated path, name and a 1-byte value.
                let ret = unsafe {
                    libc::setxattr(p.as_ptr(), c"trusted.overlay.opaque".as_ptr(), b"y".as_ptr().cast(), 1, 0)
                };
                sys::check(ret as _, "mark opaque")?;
            } else {
                let target = parent.join(std::ffi::OsStr::from_bytes(hidden));
                let _ = fs::remove_file(&target);
                let _ = fs::remove_dir_all(&target);
                sys::mknod(&sys::cstr(&target)?, libc::S_IFCHR, 0, 0)?;
            }
            continue;
        }
        total += entry.size();
        let kind = entry.header().entry_type();
        if kind.is_fifo() || kind.is_character_special() || kind.is_block_special() {
            unpack_special(&mut entry, dest, &path)?;
        } else if kind.is_hard_link() {
            if crate::layer_tar::unpack_hardlink(&mut entry, dest)? {
                hardlink_metadata(&mut entry, dest, &path)?;
            }
        } else {
            if entry.unpack_in(dest)? && kind.is_dir() && let Ok(mtime) = entry.header().mtime() {
                let relative: PathBuf = path.components().filter(|c| !matches!(c, std::path::Component::CurDir)).collect();
                dir_times.push((relative, mtime));
            }
        }
    }
    // Tar stops at its end marker, before a compressed stream necessarily
    // reaches its checksum or final member. Validate that tail as well.
    io::copy(&mut archive.into_inner(), &mut io::sink())?;
    if !dir_times.is_empty() {
        let root = super::inroot::open_root(dest)?;
        set_dir_times(root.as_fd(), dir_times);
    }
    Ok(total)
}

/// Sets each directory's time once the layer is unpacked (#676): deepest
/// first, as Docker sets them, and the last entry's time for a directory
/// named twice. One a later entry removed, or made something else, is
/// skipped, and a time that can't be set doesn't fail the layer.
fn set_dir_times(root: BorrowedFd<'_>, mut dirs: Vec<(PathBuf, u64)>) {
    // By path, component by component, so a directory's subtree sits right
    // after it, and stable, so a path named twice keeps its entries in
    // archive order. Layers list their paths in this order already, which
    // makes the sort a single pass.
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut walk = DirWalk::default();
    let mut last: Option<&Path> = None;
    // Backwards, every directory comes before its parent, and consecutive
    // directories share most of their path, which `walk` opens only once.
    for (dir, mtime) in dirs.iter().rev() {
        if last == Some(dir.as_path()) {
            continue;
        }
        last = Some(dir);
        let _ = walk.set_time(root, dir, *mtime);
    }
}

/// The most directories a [`DirWalk`] keeps open, so a layer nested deeper
/// than this can't run the agent out of descriptors; deeper ones are opened
/// for each path on their own.
const DIR_WALK_KEEP: usize = 64;

/// Finds directories inside a layer one name at a time, as the kernel walks
/// a path but never through a symlink (one a later entry may have put
/// there) and making nothing. The parents of the last directory stay open
/// for the next one, which mostly shares them, so setting a time costs two
/// lookups of one name each.
#[derive(Default)]
struct DirWalk {
    /// The open parents of the last directory, from the layer root down.
    open: Vec<(std::ffi::OsString, OwnedFd)>,
}

impl DirWalk {
    /// Sets directory `rel`'s access and modification times to `mtime`.
    fn set_time(&mut self, root: BorrowedFd<'_>, rel: &Path, mtime: u64) -> io::Result<()> {
        let secs = i64::try_from(mtime).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "directory time out of range"))?;
        let mut names = Vec::new();
        for part in rel.components() {
            match part {
                std::path::Component::Normal(name) => names.push(name),
                std::path::Component::CurDir | std::path::Component::RootDir => {}
                _ => return Err(escapes()),
            }
        }
        // The layer root itself keeps its time, as before.
        let Some((name, parents)) = names.split_last() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the layer root"));
        };
        let shared = self.open.iter().zip(parents).take_while(|((open, _), name)| open.as_os_str() == **name).count();
        self.open.truncate(shared);
        // Past the cap, the rest of the parents are walked with one
        // descriptor at a time, kept only until the next step.
        let mut deep: Option<OwnedFd> = None;
        for part in &parents[shared..] {
            let parent = match &deep {
                Some(fd) => fd.as_fd(),
                None => self.open.last().map_or(root, |(_, fd)| fd.as_fd()),
            };
            let fd = open_dir_nofollow(parent, part)?;
            if deep.is_none() && self.open.len() < DIR_WALK_KEEP {
                self.open.push((part.to_os_string(), fd));
            } else {
                deep = Some(fd);
            }
        }
        let parent = match &deep {
            Some(fd) => fd.as_fd(),
            None => self.open.last().map_or(root, |(_, fd)| fd.as_fd()),
        };
        let name = sys::cstr(Path::new(name))?;
        // Only a directory: not a symlink, which AT_SYMLINK_NOFOLLOW would
        // give the time instead of its target, nor anything else a later
        // entry put in the directory's place. Nothing else changes the layer
        // while it unpacks.
        // SAFETY: stat is plain data (all-zero is valid); a valid directory
        // descriptor and a NUL-terminated name.
        let st = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            sys::check(libc::fstatat(parent.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) as _, "stat directory")?;
            st
        };
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let time = libc::timespec { tv_sec: secs, tv_nsec: 0 };
        // SAFETY: a valid directory descriptor, a NUL-terminated name and two timespecs.
        let ret = unsafe { libc::utimensat(parent.as_raw_fd(), name.as_ptr(), [time, time].as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
        sys::check(ret as _, "set directory time")?;
        Ok(())
    }
}

/// Opens directory `name` in `parent` with O_NOFOLLOW; a symlink, anything
/// else that isn't a directory, and a name that isn't there are errors.
fn open_dir_nofollow(parent: BorrowedFd<'_>, name: &std::ffi::OsStr) -> io::Result<OwnedFd> {
    let name = sys::cstr(Path::new(name))?;
    // SAFETY: a valid directory descriptor and a NUL-terminated name.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    sys::check(fd as _, "open directory")?;
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}



/// Docker applies a hard link's header to its shared inode. Pin that inode
/// without opening a device or waiting on a FIFO, and never follow a final
/// symlink while changing ownership, times or extended attributes.
fn hardlink_metadata<R: Read>(entry: &mut tar::Entry<'_, R>, dest: &Path, path: &Path) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid hard-link metadata");
    let mut pax_mtime = None;
    if let Some(extensions) = entry.pax_extensions()? {
        for extension in extensions {
            let extension = extension?;
            match extension.key_bytes() {
                b"uid" | b"gid" if !extension.value_bytes().is_empty() => {
                    let owner: u32 = extension.value().map_err(|_| invalid())?.parse().map_err(|_| invalid())?;
                    if owner == u32::MAX { return Err(invalid()); }
                },
                b"mtime" => {
                    pax_mtime = if extension.value_bytes().is_empty() { None }
                        else { Some(pax_node_time(extension.value_bytes())?) };
                },
                _ => {},
            }
        }
    }
    let header = entry.header();
    let uid: libc::uid_t = header.uid()?.try_into().map_err(|_| invalid())?;
    let gid: libc::gid_t = header.gid()?.try_into().map_err(|_| invalid())?;
    if uid == libc::uid_t::MAX || gid == libc::gid_t::MAX { return Err(invalid()); }
    let mode = header.mode()? & 0o7777;
    let mtime = match pax_mtime {
        Some(time) => time,
        None => libc::timespec { tv_sec: header.mtime()?.try_into().map_err(|_| invalid())?, tv_nsec: 0 },
    };
    let target = crate::layer_tar::target_path(dest, path)?;
    let parent = fs::File::open(target.parent().ok_or_else(invalid)?)?;
    let name = sys::cstr(Path::new(target.file_name().ok_or_else(invalid)?))?;
    // SAFETY: a validated parent in this private layer, a single file name,
    // and O_PATH|O_NOFOLLOW to pin the inode without reading or following it.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    sys::check(fd as _, "open hard-link inode")?;
    // SAFETY: openat returned a new descriptor, owned from now on.
    let file = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: stat is plain C output storage, initialized by fstat below.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat initializes the valid output for our owned descriptor.
    sys::check(unsafe { libc::fstat(file.as_raw_fd(), &mut stat) } as _, "stat hard-link inode")?;
    let empty = c"";
    // SAFETY: AT_EMPTY_PATH addresses the pinned inode; no path is followed.
    sys::check(unsafe {
        libc::fchownat(file.as_raw_fd(), empty.as_ptr(), uid, gid, libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW)
    } as _, "chown hard-link inode")?;
    let target_c = sys::cstr(&target)?;
    if let Some(extensions) = entry.pax_extensions()? {
        for extension in extensions {
            let extension = extension?;
            if let Some(key) = extension.key_bytes().strip_prefix(b"SCHILY.xattr.") {
                let key = std::ffi::CString::new(key).map_err(|_| invalid())?;
                let value = extension.value_bytes();
                // SAFETY: canonical in-layer parent and no final-link following.
                // This layer is private until its one extraction worker publishes it.
                sys::check(unsafe {
                    libc::lsetxattr(target_c.as_ptr(), key.as_ptr(), value.as_ptr().cast(), value.len(), 0)
                } as _, "restore hard-link xattr")?;
            }
        }
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFLNK {
        let pinned = std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
        // SAFETY: procfs names our pinned non-symlink inode. chmod performs no
        // device/FIFO I/O, and follows no archive-supplied path components.
        sys::check(unsafe { libc::chmod(pinned.as_ptr(), mode) } as _, "chmod hard-link inode")?;
    }
    let times = [mtime; 2];
    // SAFETY: an owned in-layer parent and its just-created directory entry.
    // NOFOLLOW also handles a hard link to a symlink inode.
    sys::check(unsafe {
        libc::utimensat(parent.as_raw_fd(), name.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW)
    } as _, "restore hard-link time")?;
    Ok(())
}

/// tar-rs treats these POSIX types as regular files. Create the node without
/// opening it (a FIFO could block, and device I/O must never run during unpack).
/// Like whiteouts, parents must be real directories in this private layer.
fn unpack_special<R: Read>(entry: &mut tar::Entry<'_, R>, dest: &Path, path: &Path) -> io::Result<()> {
    if entry.size() != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "special layer entry has a payload"));
    }
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid special layer metadata");
    let mut pax_mtime = None;
    if let Some(extensions) = entry.pax_extensions()? {
        for extension in extensions {
            let extension = extension?;
            match extension.key_bytes() {
                b"uid" | b"gid" if !extension.value_bytes().is_empty() => {
                    // tar applies valid owner overrides to the header, but
                    // silently falls back on parse errors. Validate the raw
                    // records before replacing a node or creating parents.
                    let owner: u32 = extension.value().map_err(|_| invalid())?
                        .parse().map_err(|_| invalid())?;
                    if owner == u32::MAX { return Err(invalid()); }
                },
                b"mtime" => {
                    // Unlike uid/gid, tar leaves the physical mtime intact.
                    pax_mtime = if extension.value_bytes().is_empty() { None }
                        else { Some(pax_node_time(extension.value_bytes())?) };
                },
                b"size" if !extension.value_bytes().is_empty() => {
                    let size: u64 = extension.value().map_err(|_| invalid())?
                        .parse().map_err(|_| invalid())?;
                    if size != 0 { return Err(invalid()); }
                },
                _ => {},
            }
        }
    }
    let header = entry.header();
    let uid: libc::uid_t = header.uid()?.try_into().map_err(|_| invalid())?;
    let gid: libc::gid_t = header.gid()?.try_into().map_err(|_| invalid())?;
    // -1 means 'leave unchanged' to chown, not a representable image owner.
    if uid == libc::uid_t::MAX || gid == libc::gid_t::MAX {
        return Err(invalid());
    }
    let mode = header.mode()? & 0o7777;
    let mtime = match pax_mtime {
        Some(time) => time,
        None => libc::timespec { tv_sec: header.mtime()?.try_into().map_err(|_| invalid())?, tv_nsec: 0 },
    };
    let kind = header.entry_type();
    let (file_type, major, minor) = if kind.is_fifo() {
        (libc::S_IFIFO, 0, 0)
    } else {
        (if kind.is_character_special() { libc::S_IFCHR } else { libc::S_IFBLK },
         header.device_major()?.ok_or_else(invalid)?, header.device_minor()?.ok_or_else(invalid)?)
    };
    let device = libc::makedev(major, minor);
    if libc::major(device) != major || libc::minor(device) != minor {
        return Err(invalid());
    }
    let name = path.file_name().ok_or_else(escapes)?;
    let parent = whiteout_dir(dest, path.parent().unwrap_or(Path::new("")))?;
    let target = parent.join(name);
    // Unlink only this directory entry, never follow a previous symlink or
    // modify an existing hard-linked inode. Replacing a directory is an error.
    match fs::remove_file(&target) {
        Ok(()) => {},
        Err(e) if e.kind() == io::ErrorKind::NotFound => {},
        Err(e) => return Err(e),
    }
    let target = sys::cstr(&target)?;
    sys::mknod(&target, file_type | mode, major, minor)?;
    // Ownership changes can clear set-ID bits, so restore mode afterwards.
    // SAFETY: the parent walk rejects symlinks and this private extraction
    // worker just created the node. All paths and arrays outlive each call.
    sys::check(unsafe { libc::lchown(target.as_ptr(), uid, gid) } as _, "chown layer node")?;
    sys::check(unsafe { libc::chmod(target.as_ptr(), mode) } as _, "chmod layer node")?;
    let times = [mtime; 2];
    sys::check(unsafe {
        libc::utimensat(libc::AT_FDCWD, target.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW)
    } as _, "restore layer node time")?;
    if let Some(extensions) = entry.pax_extensions()? {
        for extension in extensions {
            let extension = extension?;
            if let Some(key) = extension.key_bytes().strip_prefix(b"SCHILY.xattr.") {
                let key = std::ffi::CString::new(key).map_err(|_| invalid())?;
                let value = extension.value_bytes();
                // SAFETY: valid C strings and a value buffer of the supplied length.
                sys::check(unsafe {
                    libc::lsetxattr(target.as_ptr(), key.as_ptr(), value.as_ptr().cast(), value.len(), 0)
                } as _, "restore layer node xattr")?;
            }
        }
    }
    Ok(())
}

/// PAX decimals may precede the epoch or exceed nanosecond precision. Avoid
/// floating-point rounding; truncate excess fractional digits toward zero,
/// then normalize a negative fraction into a nonnegative timespec nanosecond.
fn pax_node_time(value: &[u8]) -> io::Result<libc::timespec> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid PAX layer timestamp");
    let value = std::str::from_utf8(value).map_err(|_| invalid())?;
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let mut seconds: i64 = whole.parse().map_err(|_| invalid())?;
    let mut nanos: libc::c_long = 0;
    for (index, digit) in fraction.bytes().enumerate() {
        if !digit.is_ascii_digit() { return Err(invalid()); }
        if index < 9 { nanos = nanos * 10 + libc::c_long::from(digit - b'0'); }
    }
    for _ in fraction.len()..9 { nanos *= 10; }
    if value.starts_with('-') && nanos != 0 {
        seconds = seconds.checked_sub(1).ok_or_else(invalid)?;
        nanos = 1_000_000_000 - nanos;
    }
    Ok(libc::timespec { tv_sec: seconds, tv_nsec: nanos })
}

fn escapes() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "whiteout escapes the layer")
}

/// The directory under `dest` a whiteout lives in, made if missing. Every
/// step must be a real directory of this layer: an earlier entry could have
/// made one a symlink, which a check of the path's text cannot see and which
/// would carry the whiteout outside the layer.
fn whiteout_dir(dest: &Path, parent: &Path) -> io::Result<PathBuf> {
    let mut dir = dest.to_path_buf();
    for part in parent.components() {
        match part {
            std::path::Component::Normal(name) => dir.push(name),
            std::path::Component::CurDir => continue,
            _ => return Err(escapes()),
        }
        match fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(escapes()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&dir)?,
            Err(e) => return Err(e),
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete header: tar-rs refuses to unpack one with empty uid/gid.
    fn header(size: u64, mode: u32) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.set_size(size);
        h.set_mode(mode);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_cksum();
        h
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("asagiri-layers-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn directory_times_in_any_order_depth_and_kind() {
        use std::os::unix::fs::MetadataExt;

        let mut b = tar::Builder::new(Vec::new());
        // `./dot`, then `dot/` further on (the tar crate's builder drops a
        // leading `./`, so it is written by hand).
        let mut h = header(0, 0o755);
        h.as_gnu_mut().unwrap().name[..5].copy_from_slice(b"./dot");
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mtime(7_000_000);
        h.set_cksum();
        b.append(&h, io::empty()).unwrap();
        let mut add = |path: &str, kind: tar::EntryType, mtime: u64, link: Option<&str>| {
            let mut h = header(0, if kind.is_symlink() { 0o777 } else { 0o755 });
            h.set_entry_type(kind);
            h.set_mtime(mtime);
            match link {
                Some(to) => b.append_link(&mut h, path, to).unwrap(),
                None => {
                    h.set_cksum();
                    b.append_data(&mut h, path, io::empty()).unwrap();
                }
            }
        };
        let dir = tar::EntryType::Directory;
        // Out of order, with names that sort between a directory and its
        // children byte by byte ("a-c", "a.d" before "a/b").
        add("a/b", dir, 2_000_000, None);
        add("a-c", dir, 3_000_000, None);
        add("a", dir, 1_000_000, None);
        add("a.d", dir, 4_000_000, None);
        add("a/b/c", dir, 5_000_000, None);
        // Named twice: the later entry's time, also when spelt another way
        // (`./dot` came first).
        add("dup", dir, 6_000_000, None);
        add("dup", dir, 7_000_000, None);
        add("dot/", dir, 6_000_000, None);
        // Deeper than a walk keeps open, then a sibling at each end.
        let mut deep = String::from("deep");
        let mut deeps = vec![deep.clone()];
        for i in 0..(DIR_WALK_KEEP + 20) {
            deep.push_str(&format!("/{i}"));
            deeps.push(deep.clone());
        }
        for (i, p) in deeps.iter().enumerate() {
            add(p, dir, 10_000_000 + i as u64, None);
        }
        add(&format!("{deep}-sib"), dir, 8_000_000, None);
        add("deep/0-sib", dir, 9_000_000, None);
        // A symlink to a directory keeps its own time, and so does the directory.
        add("real", dir, 11_000_000, None);
        add("sw", tar::EntryType::Symlink, 14_000_000, Some("real"));
        let bytes = b.into_inner().unwrap();

        let dest = scratch("dir-times-order");
        untar(&bytes[..], &dest).unwrap();
        let mtime = |p: &str| fs::symlink_metadata(dest.join(p)).unwrap().mtime();
        for (p, t) in [("a", 1_000_000), ("a/b", 2_000_000), ("a-c", 3_000_000), ("a.d", 4_000_000), ("a/b/c", 5_000_000), ("dup", 7_000_000), ("dot", 6_000_000)] {
            assert_eq!(mtime(p), t, "{p}");
        }
        for (i, p) in deeps.iter().enumerate() {
            assert_eq!(mtime(p), 10_000_000 + i as i64, "{p}");
        }
        assert_eq!(mtime(&format!("{deep}-sib")), 8_000_000);
        assert_eq!(mtime("deep/0-sib"), 9_000_000);
        assert_eq!(mtime("real"), 11_000_000);
        assert_eq!(mtime("sw"), 14_000_000, "the symlink's own time");
        fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn absolute_hardlink_is_relative_to_the_layer_root() {
        use std::os::unix::fs::MetadataExt;
        let mut builder = tar::Builder::new(Vec::new());
        builder.append_data(&mut header(4, 0o640), "bin/tool", &b"tool"[..]).unwrap();
        let mut link = header(0, 0o777);
        link.set_entry_type(tar::EntryType::Link);
        builder.append_link(&mut link, "bin/tool-1.0", "/bin/tool").unwrap();
        let bytes = builder.into_inner().unwrap();
        let dest = scratch("absolute-hardlink");
        untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).unwrap();
        let original = fs::metadata(dest.join("bin/tool")).unwrap();
        let linked = fs::metadata(dest.join("bin/tool-1.0")).unwrap();
        assert_eq!(fs::read(dest.join("bin/tool-1.0")).unwrap(), b"tool");
        assert_eq!((original.dev(), original.ino()), (linked.dev(), linked.ino()));
        assert_eq!(linked.mode() & 0o7777, 0o777);
        fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn hardlink_headers_update_both_names_including_pax_metadata() {
        use std::os::unix::fs::MetadataExt;
        for (index, target) in ["bin/tool", "/bin/tool"].iter().enumerate() {
            let mut builder = tar::Builder::new(Vec::new());
            let mut source = header(4, 0o640);
            source.set_uid(1001);
            source.set_gid(1002);
            source.set_mtime(1_700_000_000);
            builder.append_data(&mut source, "bin/tool", &b"tool"[..]).unwrap();
            builder.append_data(&mut header(3, 0o600), "bin/copy", &b"old"[..]).unwrap();
            builder.append_pax_extensions([
                ("uid", b"2001".as_slice()), ("gid", b"2002".as_slice()),
                ("mtime", b"1712345689.125".as_slice()),
                ("SCHILY.xattr.user.asagiri.hardlink", b"shared-value".as_slice()),
            ]).unwrap();
            let mut link = header(0, 0o2650);
            link.set_uid(1003);
            link.set_gid(1004);
            link.set_entry_type(tar::EntryType::Link);
            builder.append_link(&mut link, "bin/copy", target).unwrap();
            let dest = scratch(&format!("hardlink-metadata-{index}"));
            untar(builder.into_inner().unwrap().as_slice(), &dest).unwrap();
            let original = fs::metadata(dest.join("bin/tool")).unwrap();
            for path in ["bin/tool", "bin/copy"] {
                let path = dest.join(path);
                let meta = fs::metadata(&path).unwrap();
                assert_eq!((meta.dev(), meta.ino()), (original.dev(), original.ino()));
                assert_eq!((meta.uid(), meta.gid(), meta.mode() & 0o7777), (2001, 2002, 0o2650));
                assert_eq!((meta.mtime(), meta.mtime_nsec()), (1_712_345_689, 125_000_000));
                assert_eq!(fs::read(&path).unwrap(), b"tool");
                let path = sys::cstr(&path).unwrap();
                let mut bytes = [0u8; 32];
                // SAFETY: owned file and a writable output of the supplied length.
                let len = unsafe {
                    libc::lgetxattr(path.as_ptr(), c"user.asagiri.hardlink".as_ptr(), bytes.as_mut_ptr().cast(), bytes.len())
                };
                assert_eq!(len, 12);
                assert_eq!(&bytes[..12], b"shared-value");
            }
            fs::remove_dir_all(dest).unwrap();
        }
    }

    #[test]
    fn hardlink_to_symlink_changes_only_the_symlink_inode() {
        use std::os::unix::fs::MetadataExt;
        let outside = scratch("hardlink-symlink-outside");
        fs::write(outside.join("inside"), b"host proof").unwrap();
        let before = fs::metadata(outside.join("inside")).unwrap();
        let dest = outside.join("layer");
        fs::create_dir(&dest).unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append_data(&mut header(5, 0o640), "inside", &b"image"[..]).unwrap();
        let mut symlink = header(0, 0o777);
        symlink.set_entry_type(tar::EntryType::Symlink);
        builder.append_link(&mut symlink, "bin/link", "../inside").unwrap();
        let mut hardlink = header(0, 0o7777);
        hardlink.set_entry_type(tar::EntryType::Link);
        hardlink.set_uid(2001);
        hardlink.set_gid(2002);
        builder.append_link(&mut hardlink, "copy", "/bin/link").unwrap();
        untar(builder.into_inner().unwrap().as_slice(), &dest).unwrap();
        let first = fs::symlink_metadata(dest.join("bin/link")).unwrap();
        let linked = fs::symlink_metadata(dest.join("copy")).unwrap();
        assert!(linked.file_type().is_symlink());
        assert_eq!((first.dev(), first.ino()), (linked.dev(), linked.ino()));
        assert_eq!((linked.uid(), linked.gid()), (2001, 2002));
        let after = fs::metadata(outside.join("inside")).unwrap();
        assert_eq!((before.uid(), before.gid(), before.mode()), (after.uid(), after.gid(), after.mode()));
        assert_eq!(fs::read(outside.join("inside")).unwrap(), b"host proof");
        assert_eq!(fs::read(dest.join("inside")).unwrap(), b"image");
        fs::remove_dir_all(outside).unwrap();
    }

    /// A layer whose first entry makes `foo` a symlink to `/` and whose
    /// second writes `foo/x` must not write outside the layer directory.
    #[test]
    fn symlink_parent_cannot_escape_layer() {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = header(0, 0o777);
        h.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut h, "foo", "/").unwrap();
        let mut h = header(1, 0o644);
        b.append_data(&mut h, "foo/x", &b"x"[..]).unwrap();
        let bytes = b.into_inner().unwrap();
        let dest = scratch("escape");
        let err = untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).expect_err("writing through a symlinked parent must fail");
        // The refusal must be the path check, not some other unpack error.
        assert!(err.to_string().contains("outside"), "unexpected error: {err}");
        assert!(!Path::new("/x").is_file() || fs::read_to_string("/x").map_or(true, |c| c != "x"), "escaped to /x");
        let _ = fs::remove_dir_all(&dest);
    }

    fn whiteout_layer(entries: &[(&str, Option<&str>)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, link) in entries {
            if let Some(to) = link {
                let mut h = header(0, 0o777);
                h.set_entry_type(tar::EntryType::Symlink);
                b.append_link(&mut h, name, to).unwrap();
            } else {
                let mut h = header(0, 0o644);
                b.append_data(&mut h, name, &[][..]).unwrap();
            }
        }
        b.into_inner().unwrap()
    }

    /// A whiteout under a symlink to a directory outside the layer must not
    /// delete, replace or mark anything there.
    #[test]
    fn whiteout_through_symlink_stays_in_layer() {
        let outside = scratch("wh-outside");
        fs::write(outside.join("victim"), "keep").unwrap();
        let to = outside.to_str().unwrap();
        for hidden in ["foo/.wh.victim", "foo/.wh..wh..opq"] {
            let dest = scratch("wh-escape");
            let bytes = whiteout_layer(&[("foo", Some(to)), (hidden, None)]);
            let err = untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).expect_err(hidden);
            assert!(err.to_string().contains("escapes"), "{hidden}: unexpected error: {err}");
            assert_eq!(fs::read_to_string(outside.join("victim")).unwrap(), "keep", "{hidden}");
            assert!(!outside.join(&hidden[4..]).exists(), "{hidden}: placeholder written outside");
            let _ = fs::remove_dir_all(&dest);
        }
        let _ = fs::remove_dir_all(&outside);
    }

    /// Whiteouts naming the directory itself or its parent are refused
    /// before anything is removed.
    #[test]
    fn whiteout_of_dot_or_dotdot_is_refused() {
        for hidden in ["d/.wh.", "d/.wh..", "d/.wh..."] {
            let dest = scratch("wh-dots");
            fs::create_dir_all(dest.join("d")).unwrap();
            fs::write(dest.join("d/keep"), "x").unwrap();
            let bytes = whiteout_layer(&[(hidden, None)]);
            let err = untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).expect_err(hidden);
            assert!(err.to_string().contains("escapes"), "{hidden}: unexpected error: {err}");
            assert!(dest.join("d/keep").is_file(), "{hidden}");
            let _ = fs::remove_dir_all(&dest);
        }
    }

    /// A whiteout whose own path is absolute (the tar builder won't write
    /// one, so the header is patched by hand) must not reach outside.
    #[test]
    fn whiteout_path_cannot_escape_layer() {
        let base = scratch("wh-abs");
        let dest = base.join("layer");
        fs::create_dir(&dest).unwrap();
        fs::write(base.join("keep"), "x").unwrap();
        let path = format!("{}/.wh.keep", base.display());
        let mut b = tar::Builder::new(Vec::new());
        b.append_data(&mut header(0, 0o644), "a".repeat(path.len()), &[][..]).unwrap();
        let mut bytes = b.into_inner().unwrap();
        bytes[..100].fill(0);
        bytes[..path.len()].copy_from_slice(path.as_bytes());
        bytes[148..156].fill(b' ');
        let checksum: u32 = bytes[..512].iter().map(|b| u32::from(*b)).sum();
        bytes[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        assert!(untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).is_err());
        assert!(base.join("keep").is_file());
        let _ = fs::remove_dir_all(&base);
    }

    /// An ordinary whiteout becomes overlayfs's 0:0 character device and
    /// leaves no `.wh.` file behind. mknod needs root, as in the guest.
    #[test]
    fn whiteout_becomes_char_device() {
        // SAFETY: a plain getter.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: needs root for mknod");
            return;
        }
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let dest = scratch("wh-ok");
        fs::create_dir_all(dest.join("etc")).unwrap();
        fs::write(dest.join("etc/gone"), "x").unwrap();
        let bytes = whiteout_layer(&[("etc/.wh.gone", None)]);
        untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).unwrap();
        let m = fs::symlink_metadata(dest.join("etc/gone")).unwrap();
        assert!(m.file_type().is_char_device() && m.rdev() == 0, "not a 0:0 char device");
        assert!(!dest.join("etc/.wh.gone").exists(), "placeholder left behind");
        let _ = fs::remove_dir_all(&dest);
        // As `docker commit` writes them: no owner or time, which tar-rs
        // would refuse to unpack. A whiteout is never unpacked, so it works,
        // and missing parents are made.
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(0);
        h.set_mode(0o644);
        b.append_data(&mut h, "usr/share/.wh.doc", std::io::empty()).unwrap();
        let bytes = b.into_inner().unwrap();
        let dest = scratch("wh-bare");
        untar(Capped { inner: &bytes[..], left: u64::MAX }, &dest).unwrap();
        assert!(fs::symlink_metadata(dest.join("usr/share/doc")).unwrap().file_type().is_char_device());
        let _ = fs::remove_dir_all(&dest);
    }

    fn special_layer(kind: tar::EntryType, path: &str, hardlink: bool) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = header(0, 0o2640);
        h.set_entry_type(kind);
        // SAFETY: getters do not mutate process state.
        h.set_uid(u64::from(unsafe { libc::geteuid() }));
        h.set_gid(u64::from(unsafe { libc::getegid() }));
        h.set_mtime(1_700_000_000);
        if kind.is_character_special() || kind.is_block_special() {
            h.set_device_major(1).unwrap();
            h.set_device_minor(7).unwrap();
        }
        b.append_data(&mut h, path, io::empty()).unwrap();
        if hardlink {
            let mut h = header(0, 0o644);
            h.set_entry_type(tar::EntryType::Link);
            h.set_uid(u64::from(unsafe { libc::geteuid() }));
            h.set_gid(u64::from(unsafe { libc::getegid() }));
            h.set_mtime(1_710_000_000);
            b.append_link(&mut h, "alias", path).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn special_fifo_preserves_type_metadata_and_hardlinks() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        for hardlink in [false, true] {
            let dest = scratch("special-fifo");
            let bytes = special_layer(tar::EntryType::Fifo, "nested/pipe", hardlink);
            assert_eq!(untar(&bytes[..], &dest).unwrap(), 0);
            let m = fs::symlink_metadata(dest.join("nested/pipe")).unwrap();
            assert!(m.file_type().is_fifo(), "FIFO layer entry became a regular file");
            // A later link header updates the shared inode, as Docker does.
            assert_eq!(m.mode() & 0o7777, if hardlink { 0o644 } else { 0o2640 });
            assert_eq!(m.mtime(), if hardlink { 1_710_000_000 } else { 1_700_000_000 });
            assert_eq!(m.uid(), unsafe { libc::geteuid() });
            assert_eq!(m.gid(), unsafe { libc::getegid() });
            if hardlink {
                assert_eq!(m.ino(), fs::symlink_metadata(dest.join("alias")).unwrap().ino());
                assert_eq!(m.nlink(), 2);
            }
            fs::remove_dir_all(dest).unwrap();
        }
    }

    #[test]
    fn special_entry_replaces_symlink_without_touching_target() {
        use std::os::unix::fs::FileTypeExt;
        let dest = scratch("special-replace");
        let outside = scratch("special-target");
        fs::write(outside.join("keep"), b"unchanged").unwrap();
        std::os::unix::fs::symlink(outside.join("keep"), dest.join("pipe")).unwrap();
        let bytes = special_layer(tar::EntryType::Fifo, "pipe", false);
        untar(&bytes[..], &dest).unwrap();
        assert!(fs::symlink_metadata(dest.join("pipe")).unwrap().file_type().is_fifo());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"unchanged");
        fs::remove_dir_all(dest).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn special_entry_cannot_follow_a_symlink_parent() {
        let dest = scratch("special-parent");
        let outside = scratch("special-outside");
        std::os::unix::fs::symlink(&outside, dest.join("link")).unwrap();
        let bytes = special_layer(tar::EntryType::Fifo, "link/pipe", false);
        assert!(untar(&bytes[..], &dest).is_err());
        assert!(fs::symlink_metadata(outside.join("pipe")).is_err());
        fs::remove_dir_all(dest).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn special_character_and_block_devices_preserve_numbers() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: needs root for mknod");
            return;
        }
        for (name, kind) in [("char", tar::EntryType::Char), ("block", tar::EntryType::Block)] {
            for hardlink in [false, true] {
                let dest = scratch(&format!("special-{name}"));
                let bytes = special_layer(kind, "node", hardlink);
                untar(&bytes[..], &dest).unwrap();
                let m = fs::symlink_metadata(dest.join("node")).unwrap();
                assert_eq!(m.file_type().is_char_device(), kind.is_character_special());
                assert_eq!(m.file_type().is_block_device(), kind.is_block_special());
                assert_eq!(libc::major(m.rdev()), 1);
                assert_eq!(libc::minor(m.rdev()), 7);
                assert_eq!(m.mode() & 0o7777, if hardlink { 0o644 } else { 0o2640 });
                assert_eq!(m.mtime(), if hardlink { 1_710_000_000 } else { 1_700_000_000 });
                assert_eq!(m.uid(), unsafe { libc::geteuid() });
                assert_eq!(m.gid(), unsafe { libc::getegid() });
                if hardlink {
                    assert_eq!(m.ino(), fs::symlink_metadata(dest.join("alias")).unwrap().ino());
                    assert_eq!(m.nlink(), 2);
                }
                fs::remove_dir_all(dest).unwrap();
            }
        }
    }

    #[test]
    fn special_payload_is_rejected_without_publishing_layer() {
        let root = scratch("special-payload");
        let mut b = tar::Builder::new(Vec::new());
        let mut h = header(1, 0o600);
        h.set_entry_type(tar::EntryType::Fifo);
        b.append_data(&mut h, "pipe", &b"x"[..]).unwrap();
        let blob = b.into_inner().unwrap();
        let begin = LayerBegin { digest: D1, media_type: "application/vnd.oci.image.layer.v1.tar", size: blob.len() as u64 };
        assert!(receive(&mut &blob[..], &begin, &root).is_err());
        assert!(!present(&root, D1));
        assert!(!dir(&root, D1).unwrap().exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn special_paths_cannot_be_absolute_or_contain_parent_steps() {
        for path in ["/escape", "../escape", "nested/../../escape"] {
            let dest = scratch("special-path");
            let mut bytes = special_layer(tar::EntryType::Fifo, "placeholder", false);
            // The builder rejects unsafe paths; construct a valid checksum
            // over the malicious wire header to exercise the actual reader.
            bytes[..100].fill(0);
            bytes[..path.len()].copy_from_slice(path.as_bytes());
            bytes[148..156].fill(b' ');
            let checksum: u32 = bytes[..512].iter().map(|b| u32::from(*b)).sum();
            bytes[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
            assert!(untar(&bytes[..], &dest).is_err(), "accepted {path}");
            fs::remove_dir_all(dest).unwrap();
        }
    }

    #[test]
    fn special_fifo_uses_pax_owner_overrides() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let dest = scratch("special-pax-owner");
        let mut b = tar::Builder::new(Vec::new());
        // SAFETY: plain getters; using our own IDs also works without root.
        let uid = unsafe { libc::geteuid() }.to_string();
        let gid = unsafe { libc::getegid() }.to_string();
        b.append_pax_extensions([("uid", uid.as_bytes()), ("gid", gid.as_bytes())]).unwrap();
        let mut h = header(0, 0o600);
        h.set_entry_type(tar::EntryType::Fifo);
        h.set_uid(u64::from(u32::MAX));
        h.set_gid(u64::from(u32::MAX));
        b.append_data(&mut h, "pipe", io::empty()).unwrap();
        let bytes = b.into_inner().unwrap();
        untar(&bytes[..], &dest).unwrap();
        let m = fs::symlink_metadata(dest.join("pipe")).unwrap();
        assert!(m.file_type().is_fifo());
        assert_eq!(m.uid().to_string(), uid);
        assert_eq!(m.gid().to_string(), gid);
        fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn special_fifo_rejects_invalid_pax_numbers_before_replacement() {
        let mut accepted = Vec::new();
        for key in ["uid", "gid", "size"] {
            for value in ["18446744073709551616", "not-an-owner", "4294967296", "4294967295", "-1"] {
                let dest = scratch(&format!("special-pax-invalid-{key}-{value}"));
                fs::write(dest.join("pipe"), b"keep the existing file").unwrap();
                let mut b = tar::Builder::new(Vec::new());
                b.append_pax_extensions([(key, value.as_bytes())]).unwrap();
                let mut h = header(0, 0o600);
                h.set_entry_type(tar::EntryType::Fifo);
                b.append_data(&mut h, "pipe", io::empty()).unwrap();
                let bytes = b.into_inner().unwrap();
                let result = untar(&bytes[..], &dest);
                // Never open the target if the old implementation made a FIFO.
                let unchanged = fs::symlink_metadata(dest.join("pipe")).unwrap().is_file()
                    && fs::read(dest.join("pipe")).unwrap() == b"keep the existing file";
                if result.is_ok() || !unchanged {
                    accepted.push(format!("{key}={value}"));
                }
                fs::remove_dir_all(dest).unwrap();
            }
        }
        assert!(accepted.is_empty(), "invalid numeric metadata modified the layer: {accepted:?}");
    }

    #[test]
    fn special_fifo_uses_fractional_pax_mtime() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        for (index, (value, seconds, nanos)) in [
            ("1700000000.25", 1_700_000_000, 250_000_000),
            ("1700000001", 1_700_000_001, 0),
            ("-0.25", -1, 750_000_000),
            ("1700000000.1234567899", 1_700_000_000, 123_456_789),
            ("", 0, 0),
        ].into_iter().enumerate() {
            let dest = scratch(&format!("special-pax-time-{index}"));
            let mut b = tar::Builder::new(Vec::new());
            b.append_pax_extensions([("mtime", value.as_bytes())]).unwrap();
            let mut h = header(0, 0o600);
            h.set_entry_type(tar::EntryType::Fifo);
            h.set_uid(u64::from(unsafe { libc::geteuid() }));
            h.set_gid(u64::from(unsafe { libc::getegid() }));
            b.append_data(&mut h, "pipe", io::empty()).unwrap();
            let bytes = b.into_inner().unwrap();
            // The locked reader exposes PAX mtime separately; its physical
            // header retains zero, unlike the applied uid/gid overrides.
            let mut archive = tar::Archive::new(&bytes[..]);
            let mut entry = archive.entries().unwrap().next().unwrap().unwrap();
            assert_eq!(entry.header().mtime().unwrap(), 0);
            let timestamp = entry.pax_extensions().unwrap().unwrap()
                .find_map(|item| {
                    let item = item.unwrap();
                    (item.key_bytes() == b"mtime").then(|| item.value_bytes().to_vec())
                }).unwrap();
            assert_eq!(timestamp, value.as_bytes());
            untar(&bytes[..], &dest).unwrap();
            let m = fs::symlink_metadata(dest.join("pipe")).unwrap();
            assert!(m.file_type().is_fifo());
            fs::remove_dir_all(dest).unwrap();
            assert_eq!((m.mtime(), m.mtime_nsec()), (seconds, nanos), "{value}");
        }
    }

    #[test]
    fn special_pax_timestamp_bounds_and_precision() {
        for (value, seconds, nanos) in [
            ("0", 0, 0), ("+1.25", 1, 250_000_000), ("1.", 1, 0),
            ("-0.000000001", -1, 999_999_999), ("-1.25", -2, 750_000_000),
            ("-0.0000000009", 0, 0),
            ("9223372036854775807.999999999", i64::MAX, 999_999_999),
            ("-9223372036854775808", i64::MIN, 0),
        ] {
            let time = pax_node_time(value.as_bytes()).unwrap();
            assert_eq!((time.tv_sec, time.tv_nsec), (seconds, nanos), "{value}");
        }
        for value in ["", "NaN", "1e9", "1.2.3", ".25", " 1", "1.123456789x",
                      "9223372036854775808", "-9223372036854775809", "-9223372036854775808.1"] {
            assert_eq!(pax_node_time(value.as_bytes()).err().unwrap().kind(), io::ErrorKind::InvalidData, "{value}");
        }
        assert!(pax_node_time(b"1.\xff").is_err());
    }

    #[test]
    fn special_fifo_rejects_invalid_pax_time_before_replacement() {
        for (index, value) in ["NaN", "1.2.3", "1.123456789x", "9223372036854775808",
                               "-9223372036854775808.1"].into_iter().enumerate() {
            let dest = scratch(&format!("special-pax-invalid-time-{index}"));
            fs::write(dest.join("pipe"), b"keep the existing file").unwrap();
            let mut b = tar::Builder::new(Vec::new());
            b.append_pax_extensions([("mtime", value.as_bytes())]).unwrap();
            let mut h = header(0, 0o600);
            h.set_entry_type(tar::EntryType::Fifo);
            b.append_data(&mut h, "pipe", io::empty()).unwrap();
            let bytes = b.into_inner().unwrap();
            assert_eq!(untar(&bytes[..], &dest).unwrap_err().kind(), io::ErrorKind::InvalidData);
            assert!(fs::symlink_metadata(dest.join("pipe")).unwrap().is_file());
            assert_eq!(fs::read(dest.join("pipe")).unwrap(), b"keep the existing file");
            fs::remove_dir_all(dest).unwrap();
        }
    }

    fn layer_blob(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, data) in files {
            b.append_data(&mut header(data.len() as u64, 0o644), name, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    fn send(root: &Path, digest: &str, blob: &[u8]) -> LayerStored {
        let begin = LayerBegin { digest, media_type: "application/vnd.oci.image.layer.v1.tar", size: blob.len() as u64 };
        receive(&mut &blob[..], &begin, root).unwrap()
    }

    /// Keep syscall failures in a separate process, not in other libtest cases.
    fn isolated_sync_case(name: &str) -> bool {
        const CHILD: &str = "ASAGIRI_LAYER_SYNC_TEST";
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return true;
        }
        let filter = name.strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::")).unwrap_or(name);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", filter, "--nocapture"]).env(CHILD, name).output().unwrap();
        assert!(output.status.success() && String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
                "{name}: isolated layer test did not pass\n{}\n{}",
                String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        false
    }

    fn owned_header(size: u64, mode: u32) -> tar::Header {
        let mut h = header(size, mode);
        // SAFETY: getters do not mutate process state.
        h.set_uid(u64::from(unsafe { libc::geteuid() }));
        h.set_gid(u64::from(unsafe { libc::getegid() }));
        h
    }

    fn fail_sync_calls(calls: &[libc::c_long]) {
        let mut filter = vec![libc::sock_filter {
            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, jt: 0, jf: 0, k: 0,
        }];
        for &call in calls {
            filter.push(libc::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16, jt: 0, jf: 1, k: call as u32,
            });
            filter.push(libc::sock_filter {
                code: (libc::BPF_RET | libc::BPF_K) as u16, jt: 0, jf: 0,
                k: libc::SECCOMP_RET_ERRNO | libc::EIO as u32,
            });
        }
        filter.push(libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16, jt: 0, jf: 0, k: libc::SECCOMP_RET_ALLOW,
        });
        let program = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_mut_ptr() };
        // SAFETY: this child test thread only restricts its own syscalls. The
        // filter lives through prctl, which copies it before returning.
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
            assert_eq!(libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program), 0);
        }
    }

    #[test]
    fn layer_sync_ignores_unrelated_filesystem_writes() {
        if !isolated_sync_case(concat!(module_path!(), "::layer_sync_ignores_unrelated_filesystem_writes")) {
            return;
        }
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let root = scratch("scoped-sync");
        let mut b = tar::Builder::new(Vec::new());
        b.append_data(&mut owned_header(7, 0o644), "nested/payload", &b"durable"[..]).unwrap();
        let mut link = owned_header(0, 0o777);
        link.set_entry_type(tar::EntryType::Link);
        b.append_link(&mut link, "alias", "nested/payload").unwrap();
        link.set_entry_type(tar::EntryType::Symlink);
        for (name, target) in [("outside", "/proc"), ("cycle", "."), ("broken", "missing")] {
            b.append_link(&mut link, name, target).unwrap();
        }
        let mut fifo = owned_header(0, 0o600);
        fifo.set_entry_type(tar::EntryType::Fifo);
        b.append_data(&mut fifo, "nested/pipe", io::empty()).unwrap();
        let blob = b.into_inner().unwrap();
        // Unrelated dirty inodes or filesystem-wide writeback errors must not
        // hold this complete layer. File and directory fsync remain available.
        fail_sync_calls(&[libc::SYS_syncfs]);
        send(&root, D1, &blob);
        assert!(present(&root, D1));
        let layer = dir(&root, D1).unwrap();
        assert_eq!(fs::read(layer.join("nested/payload")).unwrap(), b"durable");
        assert_eq!(fs::metadata(layer.join("alias")).unwrap().ino(),
                   fs::metadata(layer.join("nested/payload")).unwrap().ino());
        assert_eq!(fs::read_link(layer.join("outside")).unwrap(), Path::new("/proc"));
        assert_eq!(fs::read_link(layer.join("cycle")).unwrap(), Path::new("."));
        assert!(fs::symlink_metadata(layer.join("broken")).unwrap().is_symlink());
        assert!(fs::symlink_metadata(layer.join("nested/pipe")).unwrap().file_type().is_fifo());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn layer_sync_failure_is_not_a_ready_layer() {
        if !isolated_sync_case(concat!(module_path!(), "::layer_sync_failure_is_not_a_ready_layer")) {
            return;
        }
        let root = scratch("failed-sync");
        let mut b = tar::Builder::new(Vec::new());
        let bytes = b"must reach disk before publication";
        b.append_data(&mut owned_header(bytes.len() as u64, 0o644), "nested/payload", &bytes[..]).unwrap();
        let blob = b.into_inner().unwrap();
        let begin = LayerBegin { digest: D1, media_type: "application/vnd.oci.image.layer.v1.tar", size: blob.len() as u64 };
        fail_sync_calls(&[libc::SYS_fsync, libc::SYS_syncfs]);
        assert_eq!(receive(&mut &blob[..], &begin, &root).unwrap_err().kind(),
                   io::Error::from_raw_os_error(libc::EIO).kind());
        assert!(!present(&root, D1));
        assert!(!dir(&root, D1).unwrap().exists());
        assert!(!root.join(format!(".tmp-{}", hex(D1).unwrap())).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_transfer_never_publishes_a_complete_layer() {
        let blob = layer_blob(&[("file", b"complete tar but incomplete transfer")]);
        for missing in [1, 512, 1024] {
            let root = scratch(&format!("short-{missing}"));
            let begin = LayerBegin {
                digest: D1, media_type: "application/vnd.oci.image.layer.v1.tar",
                size: blob.len() as u64 + missing,
            };
            assert!(receive(&mut &blob[..], &begin, &root).is_err(), "missing {missing} bytes");
            assert!(!present(&root, D1));
            assert!(!dir(&root, D1).unwrap().exists());
            assert!(!root.join(format!(".tmp-{}", hex(D1).unwrap())).exists());
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn corrupt_gzip_after_tar_end_never_publishes_a_layer() {
        use std::io::Write;
        let root = scratch("gzip-tail");
        let blob = layer_blob(&[("file", b"complete")]);
        let compress = |bytes: &[u8]| {
            let mut writer = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            writer.write_all(bytes).unwrap();
            writer.finish().unwrap()
        };
        let mut compressed = compress(&blob);
        let mut trailing = compress(b"padding");
        let length = trailing.len();
        trailing[length - 8] ^= 1; // Damage the second member's CRC.
        compressed.extend(trailing);
        let begin = LayerBegin {
            digest: D1, media_type: "application/vnd.oci.image.layer.v1.tar+gzip",
            size: compressed.len() as u64,
        };
        assert!(receive(&mut &compressed[..], &begin, &root).is_err());
        assert!(!present(&root, D1));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_receives_of_one_digest_install_one_whole_layer() {
        let root = scratch("concurrent");
        let blob = layer_blob(&[("file", b"complete")]);
        std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8).map(|_| scope.spawn(|| send(&root, D1, &blob))).collect();
            for thread in threads {
                thread.join().unwrap();
            }
        });
        assert!(present(&root, D1));
        assert_eq!(fs::read(dir(&root, D1).unwrap().join("file")).unwrap(), b"complete");
        assert!(!root.join(format!(".tmp-{}", hex(D1).unwrap())).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn upload_connection_replies_only_after_a_whole_layer_is_installed() {
        use std::os::unix::net::UnixStream;
        let root = scratch("upload");
        let blob = layer_blob(&[("file", b"complete")]);
        let (client, server) = UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            let server = scope.spawn(|| upload(&mut fs::File::from(OwnedFd::from(server)), &root));
            let mut client = fs::File::from(OwnedFd::from(client));
            let mut frame = Vec::new();
            Msg::LayerBegin(LayerBegin {
                digest: D1, media_type: "application/vnd.oci.image.layer.v1.tar", size: blob.len() as u64,
            }).encode(&mut frame);
            client.write_all(&frame).unwrap();
            client.write_all(&blob).unwrap();
            let mut header = [0; 4];
            client.read_exact(&mut header).unwrap();
            let mut reply = vec![0; asagiri_proto::frame_len(&header).unwrap()];
            reply[..4].copy_from_slice(&header);
            client.read_exact(&mut reply[4..]).unwrap();
            assert!(matches!(asagiri_proto::decode(&reply).unwrap(), Msg::LayerStored(_)));
            assert!(present(&root, D1));
            server.join().unwrap().unwrap();
        });
        let _ = fs::remove_dir_all(root);
    }

    /// A stored layer is present only with its marker; one cut short
    /// (directory, no marker) is not, and is replaced when sent again.
    #[test]
    fn layer_without_marker_is_replaced() {
        let root = scratch("marker");
        let blob = layer_blob(&[("bin/sh", b"real")]);
        send(&root, D1, &blob);
        assert!(present(&root, D1));
        assert_eq!(fs::read(dir(&root, D1).unwrap().join("bin/sh")).unwrap(), b"real");

        // What a VM stopped before the flush leaves: names, empty contents.
        fs::remove_file(marker(&root, D1).unwrap()).unwrap();
        fs::write(dir(&root, D1).unwrap().join("bin/sh"), b"").unwrap();
        assert!(!present(&root, D1));
        send(&root, D1, &blob);
        assert!(present(&root, D1));
        assert_eq!(fs::read(dir(&root, D1).unwrap().join("bin/sh")).unwrap(), b"real");
        let _ = fs::remove_dir_all(&root);
    }

    /// Retain keeps whole layers it is told to and deletes the rest:
    /// unlisted layers, unmarked ones, leftovers. Other names stay.
    #[test]
    fn retain_deletes_unused_and_broken_layers() {
        let root = scratch("retain");
        let blob = layer_blob(&[("f", b"x")]);
        send(&root, D1, &blob);
        send(&root, D2, &blob);
        let broken = "3".repeat(64);
        fs::create_dir(root.join(&broken)).unwrap();
        fs::create_dir(root.join(format!(".stale-{broken}-1"))).unwrap();
        fs::create_dir(root.join(format!(".tmp-{broken}"))).unwrap();
        fs::write(root.join("other"), "keep").unwrap();

        retain(&root, [D1, &format!("sha256:{broken}")].into_iter());
        assert!(present(&root, D1));
        assert!(!dir(&root, D2).unwrap().exists() && !marker(&root, D2).unwrap().exists());
        assert!(!root.join(&broken).exists(), "unmarked layer kept");
        assert!(!root.join(format!(".stale-{broken}-1")).exists() && !root.join(format!(".tmp-{broken}")).exists());
        assert!(root.join("other").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    /// A layer that expands past its cap is refused.
    #[test]
    fn expansion_is_capped() {
        let mut b = tar::Builder::new(Vec::new());
        let data = vec![0u8; 4096];
        let mut h = header(data.len() as u64, 0o644);
        b.append_data(&mut h, "big", &data[..]).unwrap();
        let bytes = b.into_inner().unwrap();
        // As in receive(): a layer that fails is discarded, never reused.
        let capped = scratch("cap");
        assert!(untar(Capped { inner: &bytes[..], left: 1024 }, &capped).is_err());
        let _ = fs::remove_dir_all(&capped);
        let fresh = scratch("cap-ok");
        assert_eq!(untar(Capped { inner: &bytes[..], left: u64::MAX }, &fresh).unwrap(), 4096);
        let _ = fs::remove_dir_all(&fresh);
        assert_eq!(expansion_cap(1), 1 << 30);
        assert_eq!(expansion_cap(1 << 40), 64 << 30);
    }
}
