//! What the engine's storage takes on disk, for `docker system df`: each
//! unpacked layer, each container's writable layer and each volume, as
//! `du -x` counts it (allocated blocks, hard links once, never across into
//! another filesystem such as a host folder or NFS export mounted on a
//! volume). Walking tens of gigabytes takes a while, so the agent runs this
//! in a child process. The parent must remain single-threaded for container,
//! archive and layer-transfer forks.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use asagiri_proto::{MAX_FRAME, Msg};

use super::sys;

/// One outstanding measurement, read without blocking the agent event loop.
pub struct Measurement {
    pipe: fs::File,
    frame: Vec<u8>,
}

impl Measurement {
    pub fn start(layers: &Path, containers: &Path, volumes: &Path) -> io::Result<Self> {
        let (read, write) = sys::pipe()?;
        sys::set_nonblocking(read.as_raw_fd())?;
        // SAFETY: called only by the single-threaded agent. The child closes
        // inherited descriptors and exits without running parent destructors.
        let parent = unsafe { libc::getpid() };
        match unsafe { libc::fork() } {
            0 => {
                // SAFETY: this is the fork child; only its reply pipe survives.
                let result = unsafe { sys::isolate_worker(parent, Some(write.as_raw_fd())) }.and_then(|()| {
                    let lines = measure(layers, containers, volumes);
                    let mut frame = Vec::new();
                    asagiri_proto::encode_usage(&mut frame, &lines);
                    if frame.len() > MAX_FRAME {
                        return Err(io::Error::other("usage reply exceeds protocol limit"));
                    }
                    fs::File::from(write).write_all(&frame)
                });
                if let Err(error) = &result {
                    eprintln!("asagiri-init: usage: {error}");
                }
                // SAFETY: no inherited Rust object may be dropped in the child.
                unsafe { libc::_exit(i32::from(result.is_err())) }
            }
            pid if pid < 0 => Err(io::Error::last_os_error()),
            _ => Ok(Self { pipe: read.into(), frame: Vec::new() }),
        }
    }

    pub fn fd(&self) -> RawFd {
        self.pipe.as_raw_fd()
    }

    /// Return only a complete, validated Usage frame. Partial reads and a full
    /// pipe never block unrelated container work; worker failure is not success.
    pub fn on_readable(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut chunk = [0; 8192];
        loop {
            match self.pipe.read(&mut chunk) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "usage worker ended without a complete reply")),
                Ok(n) => self.frame.extend_from_slice(&chunk[..n]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            if self.frame.len() >= 4 {
                let header: [u8; 4] = self.frame[..4].try_into().expect("checked header");
                let size = asagiri_proto::frame_len(&header)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("usage frame: {e:?}")))?;
                if self.frame.len() >= size {
                    if self.frame.len() != size || !matches!(asagiri_proto::decode(&self.frame), Ok(Msg::Usage(_))) {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid usage worker reply"));
                    }
                    return Ok(Some(std::mem::take(&mut self.frame)));
                }
            }
        }
    }
}

/// One `KIND NAME BYTES` line per entry (see `asagiri_proto::Msg::Usage`).
pub fn measure(layers: &Path, containers: &Path, volumes: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    // Every entry in the layer store, half-done unpacks included, so the
    // total is what the store really takes.
    for (name, path) in entries(layers) {
        lines.push(format!("l {name} {}", du(&path)));
    }
    for (id, path) in entries(containers) {
        let upper = path.join("upper");
        if upper.is_dir() {
            lines.push(format!("c {id} {}", du(&upper)));
        }
    }
    for (name, path) in entries(volumes) {
        lines.push(format!("v {name} {}", du(&path)));
    }
    lines
}

/// The directories in `dir`, by a name that is safe on one line.
fn entries(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    read.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| (name, e.path()))
        })
        .collect()
}

/// Bytes allocated under `root`, on `root`'s filesystem only.
pub fn du(root: &Path) -> u64 {
    let Ok(top) = fs::symlink_metadata(root) else { return 0 };
    let dev = top.dev();
    let mut seen = HashSet::new();
    let mut total = top.blocks() * 512;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else { continue };
        for entry in read.flatten() {
            let Ok(m) = entry.metadata() else { continue };
            if m.dev() != dev {
                continue;
            }
            if m.nlink() > 1 && !m.is_dir() && !seen.insert(m.ino()) {
                continue;
            }
            total += m.blocks() * 512;
            if m.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader() -> (Measurement, fs::File) {
        let (read, write) = sys::pipe().unwrap();
        sys::set_nonblocking(read.as_raw_fd()).unwrap();
        (Measurement { pipe: read.into(), frame: Vec::new() }, write.into())
    }

    #[test]
    fn partial_measurements_never_block_and_keep_the_complete_reply() {
        let (mut read, mut write) = reader();
        let mut frame = Vec::new();
        asagiri_proto::encode_usage(&mut frame, &["l abcd 8192", "v data 4096"]);
        assert!(read.on_readable().unwrap().is_none());
        write.write_all(&frame[..2]).unwrap();
        assert!(read.on_readable().unwrap().is_none());
        write.write_all(&frame[2..7]).unwrap();
        assert!(read.on_readable().unwrap().is_none());
        write.write_all(&frame[7..]).unwrap();
        assert_eq!(read.on_readable().unwrap().unwrap(), frame);
    }

    #[test]
    fn a_failed_worker_does_not_report_an_empty_measurement() {
        let (mut read, mut write) = reader();
        write.write_all(&[10, 0]).unwrap();
        drop(write);
        assert_eq!(read.on_readable().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn malformed_worker_replies_are_bounded_and_rejected() {
        let (mut read, mut write) = reader();
        write.write_all(&(MAX_FRAME as u32).to_le_bytes()).unwrap();
        assert_eq!(read.on_readable().unwrap_err().kind(), io::ErrorKind::InvalidData);
        let (mut read, mut write) = reader();
        let mut frame = Vec::new();
        Msg::Ack.encode(&mut frame);
        write.write_all(&frame).unwrap();
        assert_eq!(read.on_readable().unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn counts_files_once_and_lists_each_kind() {
        let root = std::env::temp_dir().join(format!("asagiri-usage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (layers, containers, volumes) = (root.join("layers"), root.join("containers"), root.join("volumes"));
        fs::create_dir_all(layers.join("ab12")).unwrap();
        fs::create_dir_all(containers.join("c1/upper")).unwrap();
        fs::create_dir_all(volumes.join("data/_data/sub")).unwrap();
        fs::write(layers.join("ab12/f"), vec![1u8; 64 << 10]).unwrap();
        fs::write(volumes.join("data/_data/sub/big"), vec![1u8; 256 << 10]).unwrap();
        // A hard link costs nothing more; a symlink is not followed.
        fs::hard_link(volumes.join("data/_data/sub/big"), volumes.join("data/_data/again")).unwrap();
        std::os::unix::fs::symlink(&layers, volumes.join("data/_data/link")).unwrap();
        // A plain file in the store is not an entry.
        fs::write(layers.join("ab12.done"), b"").unwrap();

        let lines = measure(&layers, &containers, &volumes);
        let size = |kind: &str, name: &str| -> u64 {
            let l = lines.iter().find(|l| l.starts_with(&format!("{kind} {name} "))).unwrap_or_else(|| panic!("no {kind} {name} in {lines:?}"));
            l.rsplit(' ').next().unwrap().parse().unwrap()
        };
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(size("l", "ab12") >= 64 << 10);
        assert!(size("c", "c1") < 64 << 10);
        let v = size("v", "data");
        assert!((256 << 10..512 << 10).contains(&v), "the hard link was counted twice, or the symlink followed: {v}");
        let _ = fs::remove_dir_all(&root);
    }
}
