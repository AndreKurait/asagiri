//! Bound the raw daemon.log without replacing the inode owned by launchd.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const KEEP_TAIL: u64 = 1 << 20;

static OUTPUT: OnceLock<Mutex<DaemonOutput>> = OnceLock::new();

struct DaemonOutput {
    path: PathBuf,
    file: File,
}

pub(super) fn init(stderr: &File) {
    if let Some(home) = super::state_dir()
        && let Ok(output) = DaemonOutput::open(home.join("daemon.log"), stderr)
    {
        let _ = OUTPUT.set(Mutex::new(output));
    }
}

/// Route stdout through the same pipe only when launchd sent it to this log.
pub(super) fn capture_matching_stdout() {
    let Some(output) = OUTPUT.get() else { return };
    let output = output.lock().unwrap_or_else(|error| error.into_inner());
    // SAFETY: stdout is still owned by the process. ManuallyDrop keeps it open.
    let stdout = ManuallyDrop::new(unsafe { File::from_raw_fd(libc::STDOUT_FILENO) });
    let Ok(meta) = stdout.metadata() else { return };
    if output.file.metadata().is_ok_and(|log| same_file(&log, &meta)) {
        // SAFETY: stderr is the write end of the capture pipe at this point.
        unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) };
    }
}

/// All captured stderr and structured diagnostics pass through this lock.
pub(super) fn echo(stderr: &mut File, text: &str) -> bool {
    let Some(output) = OUTPUT.get() else { return false };
    let output = output.lock().unwrap_or_else(|error| error.into_inner());
    let _ = stderr.write_all(format!("{text}\n").as_bytes());
    let _ = output.trim(super::MAX_LOG, KEEP_TAIL);
    true
}

impl DaemonOutput {
    fn open(path: PathBuf, stderr: &File) -> io::Result<Self> {
        // SAFETY: F_GETFL only reads flags from the live stderr descriptor.
        let flags = unsafe { libc::fcntl(stderr.as_raw_fd(), libc::F_GETFL) };
        let meta = stderr.metadata()?;
        // SAFETY: geteuid reads the effective identity of this process.
        if flags < 0 || flags & libc::O_APPEND == 0 || !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() } || meta.nlink() != 1
        {
            return Err(io::Error::other("stderr is not an owned append-only log"));
        }
        let file = OpenOptions::new().read(true).write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path)?;
        if !same_file(&meta, &file.metadata()?) {
            return Err(io::Error::other("stderr belongs to a different file"));
        }
        Ok(Self { path, file })
    }

    fn trim(&self, limit: u64, keep: u64) -> io::Result<()> {
        let meta = self.file.metadata()?;
        if meta.len() <= limit || !matches_path(&self.path, &meta)? {
            return Ok(());
        }
        let tail_len = (keep + 1).min(meta.len()) as usize;
        let mut tail = vec![0; tail_len];
        self.file.read_exact_at(&mut tail, meta.len() - tail_len as u64)?;
        let Some(first) = tail.iter().position(|&byte| byte == b'\n').map(|index| index + 1) else {
            // This window contains no complete lines. Future complete output
            // can be retained when another diagnostic reaches the log.
            return Ok(());
        };
        let last = tail.iter().rposition(|&byte| byte == b'\n').map_or(tail.len(), |index| index + 1);
        let retained = &tail[first..last];
        // Leave the original tail in place until its replacement is written.
        // A failed write or a concurrent append thus leaves the tail available.
        let before = self.file.metadata()?;
        if before.len() != meta.len() || !matches_path(&self.path, &before)? {
            return Ok(());
        }
        self.file.write_all_at(retained, 0)?;
        if self.file.metadata()?.len() == before.len() {
            self.file.set_len(retained.len() as u64)?;
        }
        Ok(())
    }
}

fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

fn matches_path(path: &Path, meta: &Metadata) -> io::Result<bool> {
    let current = std::fs::symlink_metadata(path)?;
    Ok(current.is_file() && current.nlink() == 1 && same_file(&current, meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_complete_lines_on_the_same_inode_and_ignores_a_replacement() {
        let dir = std::env::temp_dir().join(format!("asagiri-raw-log-test-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("daemon.log");
        let mut stderr = OpenOptions::new().create_new(true).append(true).open(&path).unwrap();
        let output = DaemonOutput::open(path.clone(), &stderr).unwrap();
        let inode = stderr.metadata().unwrap().ino();
        stderr.write_all(b"part of old line\nkeep me\nand me\npartial").unwrap();
        output.trim(30, 24).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me\nand me\n");
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        stderr.write_all(b"latest\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me\nand me\nlatest\n");

        let moved = dir.join("moved.log");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, b"replacement\n").unwrap();
        output.trim(8, 7).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement\n");
        assert_eq!(std::fs::read(&moved).unwrap(), b"keep me\nand me\nlatest\n");
        assert!(DaemonOutput::open(path, &stderr).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
