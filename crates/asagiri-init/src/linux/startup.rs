//! A container's private setup reply, independent of its eventual stdout/TTY.

use std::cell::Cell;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{OwnedFd, RawFd};

// One reply fits in PIPE_BUF, including its tag and two-byte message length.
const MAX_MESSAGE: usize = 4093;

#[derive(Debug)]
pub(super) struct Rejected(String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}
impl std::error::Error for Rejected {}

pub fn rejected(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|e| e.is::<Rejected>())
}

/// Only the cloned child owns this descriptor. A successful setup closes it;
/// a failed child exits without running the parent's inherited destructors.
#[derive(Debug)]
pub struct Child {
    fd: Cell<Option<RawFd>>,
}

impl Child {
    pub fn new(fd: Option<RawFd>) -> Self { Self { fd: Cell::new(fd) } }
    pub fn fd(&self) -> Option<RawFd> { self.fd.get() }

    pub fn ready(&self) -> io::Result<()> {
        if let Some(fd) = self.fd.get() {
            write(fd, b"r")?;
            self.fd.set(None);
            // SAFETY: only this child owns its descriptor table. On Linux,
            // close releases the fd even if it reports EINTR.
            unsafe { libc::close(fd) };
        }
        Ok(())
    }

    pub fn error(&self, message: &str, invalid_command: bool) {
        let Some(fd) = self.fd.get() else { return };
        let mut end = message.len().min(MAX_MESSAGE);
        while !message.is_char_boundary(end) { end -= 1; }
        let mut bytes = Vec::with_capacity(end + 3);
        bytes.push(if invalid_command { b'i' } else { b'e' });
        bytes.extend_from_slice(&(end as u16).to_le_bytes());
        bytes.extend_from_slice(&message.as_bytes()[..end]);
        let _ = write(fd, &bytes);
    }
}

fn write(fd: RawFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: a live child-owned pipe and a valid byte slice.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted { continue; }
            return Err(error);
        }
        if n == 0 { return Err(io::ErrorKind::WriteZero.into()); }
        bytes = &bytes[n as usize..];
    }
    Ok(())
}

/// A length-delimited failure never waits for application stderr or EOF: a
/// child may still be reporting to a full pipe when the parent must reap it.
pub fn wait(fd: OwnedFd) -> io::Result<()> {
    let mut file = File::from(fd);
    let mut tag = [0];
    file.read_exact(&mut tag)?;
    if tag == *b"r" { return Ok(()); }
    if tag != *b"e" && tag != *b"i" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid container setup reply"));
    }
    let mut len = [0; 2];
    file.read_exact(&mut len)?;
    let len = u16::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid container setup error length"));
    }
    let mut message = vec![0; len];
    file.read_exact(&mut message)?;
    let message = String::from_utf8(message).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Err(if tag == *b"i" { io::Error::other(Rejected(message)) } else { io::Error::other(message) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn readiness_retires_the_private_descriptor_before_the_program() {
        use std::os::fd::IntoRawFd;
        let (read, write) = super::super::sys::pipe().unwrap();
        let fd = write.into_raw_fd();
        let child = Child::new(Some(fd));
        child.ready().unwrap();
        assert!(child.fd().is_none());
        // HUP on this still-owned read end proves there is no surviving
        // writer, even if another test thread has reused the old fd number.
        let mut poll = libc::pollfd { fd: read.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one live descriptor and a writable pollfd.
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 1);
        assert_ne!(poll.revents & libc::POLLHUP, 0);
        wait(read).unwrap();
    }

    #[test]
    fn failed_setup_reports_without_stderr_or_waiting_for_child_exit() {
        for invalid in [false, true] {
            let (read, write) = super::super::sys::pipe().unwrap();
            let child = Child::new(Some(write.as_raw_fd()));
            child.error("exec: missing command", invalid);
            // The writer is deliberately still open, as it is while a failed
            // child reports to a blocked stderr or a terminal.
            let error = wait(read).unwrap_err();
            assert_eq!(error.to_string(), "exec: missing command");
            assert_eq!(rejected(&error), invalid);
        }
    }

    #[test]
    fn premature_exit_and_malformed_setup_replies_cannot_mean_ready() {
        for bytes in [&b""[..], b"x", b"i", b"i\x04\x00cut", b"i\xff\xff", b"i\x01\x00\xff"] {
            let (read, writer) = super::super::sys::pipe().unwrap();
            write(writer.as_raw_fd(), bytes).unwrap();
            drop(writer);
            let error = wait(read).unwrap_err();
            assert!(!rejected(&error), "{bytes:?}");
        }
    }

    #[test]
    fn a_bounded_error_preserves_utf8_and_the_rejection_kind() {
        let (read, writer) = super::super::sys::pipe().unwrap();
        let child = Child::new(Some(writer.as_raw_fd()));
        child.error(&"é".repeat(MAX_MESSAGE), true);
        let error = wait(read).unwrap_err();
        assert!(rejected(&error));
        assert_eq!(error.to_string().len(), MAX_MESSAGE - 1);
    }
}
