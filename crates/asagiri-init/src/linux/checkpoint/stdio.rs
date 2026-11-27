//! Record only pipes whose other end belongs to the Asagiri agent. CRIU keys a pipe by its inode.
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::num::NonZeroU32;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::process::Command;

use super::super::container::Process;

pub(super) const MANIFEST: &str = "stdio.json";

#[path = "../../../../asagiri-proto/src/checkpoint_stdio.rs"]
mod manifest;
pub(crate) use manifest::CheckpointStdio;

pub(in super::super) struct Input<'a> {
    pub(in super::super) queued: &'a VecDeque<u8>,
    pub(in super::super) replay: Option<&'a Replay>,
    pub(in super::super) close_when_drained: bool,
}

/// The input file belongs to an immutable published checkpoint. Keep its descriptor
/// until the last byte enters the pipe, even if a concurrent operation unlinks it.
pub(in super::super) struct Replay {
    pub(in super::super) name: String,
    file: Option<File>,
    offset: i64,
    length: i64,
    pub(in super::super) error: Option<String>,
}

impl Replay {
    pub(in super::super) fn open(containers: &Path, id: &str, name: &str) -> io::Result<(Self, bool)> {
        super::super::container::valid_id(id)?;
        super::super::container::valid_id(name)?;
        let images = containers.join(id).join("checkpoints").join(name);
        let streams: CheckpointStdio = super::receive::manifest(&images.join(MANIFEST))?;
        streams.validate()?;
        if streams.stdin.is_none() { return Err(io::Error::other("conflict: checkpoint has no engine input pipe")); }
        let file = File::open(images.join("stdin.bin"))?;
        let meta = file.metadata()?;
        if !meta.is_file() { return Err(io::Error::other("checkpoint input is not a regular file")); }
        let length = i64::try_from(meta.len()).map_err(io::Error::other)?;
        Ok((Self { name: name.to_owned(), file: (length > 0).then_some(file), offset: 0, length, error: None }, streams.close_after_input))
    }

    pub(in super::super) fn pending(&self) -> bool { self.file.is_some() }

    pub(in super::super) fn fail(&mut self, error: String) {
        self.error = Some(error);
        self.file = None;
    }

    /// Move one bounded chunk per event-loop turn. Ordinary attach credit covers
    /// only live input, so these bytes are never included in InputConsumed.
    pub(in super::super) fn advance(&mut self, pipe: &OwnedFd) -> io::Result<()> {
        let Some(file) = &self.file else { return Ok(()) };
        let chunk = (self.length - self.offset).min(64 * 1024) as usize;
        // SAFETY: both descriptors are live, the input offset is writable and the output is a pipe.
        let n = unsafe { libc::splice(file.as_raw_fd(), &mut self.offset, pipe.as_raw_fd(), std::ptr::null_mut(), chunk, libc::SPLICE_F_NONBLOCK) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) { return Ok(()); }
            return Err(e);
        }
        if n == 0 { return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "checkpoint input was truncated")); }
        if self.offset == self.length { self.file = None; }
        Ok(())
    }

    /// Pin the unread suffix at the capture boundary. The post-dump worker copies
    /// it between the kernel pipe contents and any subsequently queued attach bytes.
    pub(super) fn snapshot(&self, command: &mut Command) -> io::Result<Option<File>> {
        if let Some(error) = &self.error { return Err(io::Error::other(error.clone())); }
        let Some(file) = &self.file else { return Ok(None) };
        let pin = file.try_clone()?;
        command.env("ASAGIRI_CHECKPOINT_REPLAY_FILE", format!("/proc/{}/fd/{}", std::process::id(), pin.as_raw_fd()))
            .env("ASAGIRI_CHECKPOINT_REPLAY_OFFSET", self.offset.to_string());
        Ok(Some(pin))
    }
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

/// Pin the receiver's child-side pipe ends alongside their saved CRIU identities.
pub(super) fn restore_pipes(images: &Path, run: &Process) -> io::Result<Vec<(NonZeroU32, File)>> {
    let streams: CheckpointStdio = super::receive::manifest(&images.join(MANIFEST))?;
    streams.validate()?;
    if streams.tty || run.tty {
        return Err(io::Error::other("checkpoint restore requires non-TTY streams"));
    }
    let mut pipes = Vec::new();
    for (index, saved, agent) in [(0, streams.stdin, &run.stdin), (1, streams.stdout, &run.stdout), (2, streams.stderr, &run.stderr)] {
        let Some(saved) = saved else { continue; };
        let agent = agent.as_ref().ok_or_else(|| io::Error::other(format!("receiver is missing engine pipe {index}")))?;
        let expected = fs::metadata(format!("/proc/self/fd/{}", agent.as_raw_fd()))?;
        let path = format!("/proc/{}/fd/{index}", run.pid);
        let same_pipe = |meta: &fs::Metadata| meta.file_type().is_fifo() && (meta.dev(), meta.ino()) == (expected.dev(), expected.ino());
        if !same_pipe(&fs::metadata(&path)?) {
            return Err(io::Error::other(format!("receiver descriptor {index} does not match the engine pipe")));
        }
        let child = OpenOptions::new().read(index == 0).write(index != 0).open(path)?;
        if !same_pipe(&child.metadata()?) {
            return Err(io::Error::other(format!("receiver descriptor {index} does not match the engine pipe")));
        }
        pipes.push((saved, child));
    }
    Ok(pipes)
}
