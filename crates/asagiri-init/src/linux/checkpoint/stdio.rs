//! Record only pipes whose other end belongs to the Asagiri agent. CRIU keys a pipe by its inode.
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Write};
use std::num::NonZeroU32;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use super::super::container::Process;

pub(super) const MANIFEST: &str = "stdio.json";

#[path = "../../../../asagiri-proto/src/checkpoint_stdio.rs"]
mod manifest;
pub(crate) use manifest::CheckpointStdio;

pub(in super::super) struct Input<'a> {
    pub(in super::super) queued: &'a VecDeque<u8>,
    pub(in super::super) close_when_drained: bool,
}

/// The post-dump worker copies this snapshot outside the control loop. Pin its descriptor until CRIU exits.
pub(super) fn queued_input(queue: &VecDeque<u8>) -> io::Result<Option<File>> {
    if queue.is_empty() { return Ok(None); }
    // SAFETY: a static NUL-terminated name, valid flags, and unique ownership of the new file.
    let fd = unsafe { libc::memfd_create(c"checkpoint-stdin".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let (first, rest) = queue.as_slices();
    file.write_all(first)?;
    file.write_all(rest)?;
    Ok(Some(file))
}

pub(super) fn capture(images: &Path, run: &Process, close_when_drained: bool) -> io::Result<()> {
    let pipe_id = |fd: Option<&OwnedFd>| -> io::Result<Option<NonZeroU32>> {
        let Some(fd) = fd.filter(|_| !run.tty) else { return Ok(None) };
        let meta = fs::metadata(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
        if !meta.file_type().is_fifo() { return Err(io::Error::other("engine stream is not a pipe")); }
        let id = u32::try_from(meta.ino()).ok().and_then(NonZeroU32::new)
            .ok_or_else(|| io::Error::other("unrepresentable CRIU pipe identity"))?;
        Ok(Some(id))
    };
    let streams = CheckpointStdio { tty: run.tty, close_after_input: !run.tty && run.stdin.is_some() && close_when_drained, stdin: pipe_id(run.stdin.as_ref())?, stdout: pipe_id(run.stdout.as_ref())?, stderr: pipe_id(run.stderr.as_ref())? };
    streams.validate()?;
    serde_json::to_writer(File::create_new(images.join(MANIFEST))?, &streams)?;
    Ok(())
}
