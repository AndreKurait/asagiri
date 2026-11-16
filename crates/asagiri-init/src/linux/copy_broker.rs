//! The agent owns copy leases and the client socket until the direct worker
//! is reaped. Large archives stream in the child; only requests for a root
//! and bounded final replies pass through this nonblocking broker.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use asagiri_proto::{Msg, copy_op};
use super::super::container::Paths;
use super::super::roots::Roots;
use super::super::sys;

const MAX_REQUEST: usize = 64 * 1024;
const MAX_REPLY: usize = 16 * 1024;

#[derive(Clone, Copy)]
pub enum View {
    Running(libc::pid_t),
    Stopped,
    Cleaning,
}

enum Grant {
    Root(OwnedFd),
    Text(Vec<u8>),
}

enum Phase {
    Awaiting,
    Pending(Vec<u8>),
    Grant(Grant),
    Working,
    Finished,
}

struct Worker {
    pid: libc::pid_t,
    client: OwnedFd,
    control: Option<OwnedFd>,
    phase: Phase,
    lease: Option<String>,
    reply: Option<Vec<u8>>,
    written: usize,
    exited: Option<i32>,
    preparing: bool,
}

pub struct Broker<'a> {
    roots: &'a Roots,
    workers: Vec<Worker>,
}

impl<'a> Broker<'a> {
    pub fn new(roots: &'a Roots) -> Self { Self { roots, workers: Vec::new() } }

    pub fn accept(&mut self, listener: &OwnedFd, paths: &Paths) {
        loop {
            // SAFETY: an owned listening socket; accepted descriptors are
            // immediately owned. The listener is nonblocking.
            let fd = unsafe { libc::accept4(listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
            if fd < 0 { return; }
            // SAFETY: accept4 returned a new descriptor.
            let client = unsafe { OwnedFd::from_raw_fd(fd) };
            if let Err(error) = self.spawn(client, paths) {
                eprintln!("asagiri-init: copy worker: {error}");
            }
        }
    }

    fn spawn(&mut self, client: OwnedFd, paths: &Paths) -> io::Result<()> {
        let (ours, child) = channel()?;
        sys::set_nonblocking(ours.as_raw_fd())?;
        // SAFETY: the single-threaded agent forks an owned child, which closes
        // other descriptors and exits without running parent destructors.
        let parent = unsafe { libc::getpid() };
        let pid = unsafe { libc::fork() };
        if pid < 0 { return Err(io::Error::last_os_error()); }
        if pid == 0 {
            super::die_with_parent(parent);
            drop(ours);
            let code = match job(client, child, paths) {
                Ok(()) => 0,
                Err(error) => { eprintln!("asagiri-init: copy: {error}"); 1 }
            };
            // SAFETY: leave the fork without copied parent destructors.
            unsafe { libc::_exit(code); }
        }
        drop(child);
        self.workers.push(Worker { pid, client, control: Some(ours), phase: Phase::Awaiting,
            lease: None, reply: None, written: 0, exited: None, preparing: false });
        Ok(())
    }

    pub fn interest(&self, fds: &mut Vec<libc::pollfd>) {
        for worker in &self.workers {
            if let Some(control) = &worker.control {
                let events = libc::POLLIN | if matches!(worker.phase, Phase::Grant(_)) { libc::POLLOUT } else { 0 };
                fds.push(libc::pollfd { fd: control.as_raw_fd(), events, revents: 0 });
            }
            if worker.exited.is_some() && worker.reply.as_ref().is_some_and(|r| worker.written < r.len()) {
                fds.push(libc::pollfd { fd: worker.client.as_raw_fd(), events: libc::POLLOUT, revents: 0 });
            }
        }
    }

    pub fn owns(&self, fd: RawFd) -> bool {
        self.workers.iter().any(|worker| worker.client.as_raw_fd() == fd
            || worker.control.as_ref().is_some_and(|control| control.as_raw_fd() == fd))
    }

    pub fn readable(&mut self, fd: RawFd) {
        if let Some(worker) = self.workers.iter_mut().find(|worker| worker.control.as_ref().is_some_and(|c| c.as_raw_fd() == fd)) {
            worker.read(self.roots);
        }
    }

    /// Also revisits a pending root after its prior cleanup completes. View
    /// lookup happens here, never from a snapshot taken at accept/fork time.
    pub fn advance(&mut self, paths: &Paths, view: impl Fn(&str) -> View) {
        for worker in &mut self.workers {
            if worker.exited.is_none() {
                if let Phase::Pending(request) = &worker.phase {
                    match grant(self.roots, paths, request, &view) {
                        Ok(Some((reply, id, preparing))) => {
                            worker.lease = Some(id);
                            worker.preparing = preparing;
                            worker.phase = Phase::Grant(reply);
                        }
                        Ok(None) => {}
                        Err(error) => worker.phase = Phase::Grant(Grant::Text(format!("error: {error}").chars().take(120).collect::<String>().into_bytes())),
                    }
                }
                worker.send_grant();
            } else {
                worker.flush();
            }
        }
        self.workers.retain(|worker| worker.exited.is_none()
            || worker.reply.as_ref().is_some_and(|reply| worker.written < reply.len()));
    }

    pub fn reaped(&mut self, pid: libc::pid_t, code: i32) -> bool {
        let Some(worker) = self.workers.iter_mut().find(|worker| worker.pid == pid && worker.exited.is_none()) else { return false };
        // Mark reaped before reading queued packets: an invalid packet must
        // never signal a PID that waitpid has already made reusable.
        worker.exited = Some(code);
        worker.read(self.roots);
        worker.control = None;
        worker.phase = Phase::Finished;
        if code != 0 || worker.reply.is_none() {
            worker.reply = Some(error_reply("container filesystem copy worker did not complete"));
        }
        if std::mem::take(&mut worker.preparing) { self.roots.copy_ready(); }
        if let Some(id) = worker.lease.take() { self.roots.release(&id); }
        true
    }

    pub fn shutdown(&mut self) {
        for worker in &mut self.workers {
            if worker.exited.is_none() { join(worker.pid); }
            worker.phase = Phase::Finished;
            if std::mem::take(&mut worker.preparing) { self.roots.copy_ready(); }
            if let Some(id) = worker.lease.take() { self.roots.release(&id); }
        }
        self.workers.clear();
    }
}

impl Drop for Broker<'_> {
    fn drop(&mut self) { self.shutdown(); }
}

impl Worker {
    fn fail(&mut self, message: &str) {
        self.reply = Some(error_reply(message));
        self.phase = Phase::Finished;
        self.control = None;
        if self.exited.is_none() {
            // SAFETY: only the agent reaps workers; this PID is still its
            // unreaped child and cannot have been reused.
            unsafe { libc::kill(self.pid, libc::SIGKILL); }
        }
    }

    fn read(&mut self, roots: &Roots) {
        loop {
            let Some(control) = &self.control else { break };
            match receive(control.as_raw_fd()) {
                Ok(Some(packet)) if packet.is_empty() => { self.control = None; break; }
                Ok(Some(packet)) if packet[0] == b'R' && matches!(self.phase, Phase::Awaiting) => {
                    self.phase = Phase::Pending(packet[1..].to_vec());
                }
                Ok(Some(packet)) if packet == b"N" && self.preparing => {
                    self.preparing = false;
                    roots.copy_ready();
                }
                Ok(Some(packet)) if packet[0] == b'F' && packet.len() <= MAX_REPLY + 1 && self.reply.is_none() => {
                    self.reply = Some(packet[1..].to_vec());
                    self.phase = Phase::Finished;
                }
                Ok(Some(_)) => { self.fail("invalid copy worker control packet"); break; }
                Ok(None) => break,
                Err(_) => { self.fail("could not read copy worker control packet"); break; }
            }
        }
    }

    fn send_grant(&mut self) {
        let (Phase::Grant(grant), Some(control)) = (&self.phase, &self.control) else { return };
        let result = match grant {
            Grant::Root(root) => sys::send_fd(control.as_raw_fd(), root.as_raw_fd()),
            Grant::Text(message) => send(control.as_raw_fd(), message),
        };
        match result {
            Ok(()) => self.phase = Phase::Working,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => self.fail("could not send copy worker root"),
        }
    }

    fn flush(&mut self) {
        let Some(reply) = &self.reply else { return };
        while self.written < reply.len() {
            // The child and parent share the client's open file description:
            // use per-call nonblocking I/O, not O_NONBLOCK on the socket.
            // SAFETY: a live socket and a slice of our reply buffer.
            let n = unsafe { libc::send(self.client.as_raw_fd(), reply[self.written..].as_ptr().cast(),
                reply.len() - self.written, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
            if n > 0 { self.written += n as usize; continue; }
            let error = io::Error::last_os_error();
            if n < 0 && error.kind() == io::ErrorKind::Interrupted { continue; }
            if n < 0 && error.kind() == io::ErrorKind::WouldBlock { break; }
            self.written = reply.len();
        }
    }
}

fn grant(roots: &Roots, paths: &Paths, request: &[u8], view: &impl Fn(&str) -> View) -> io::Result<Option<(Grant, String, bool)>> {
    let spec = match asagiri_proto::decode(request) {
        Ok(Msg::Copy(spec)) => spec,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid root request")),
    };
    super::super::container::valid_id(spec.id)?;
    if roots.removing(spec.id) {
        return Err(io::Error::new(io::ErrorKind::NotFound, "container is being removed"));
    }
    let view = view(spec.id);
    if matches!(view, View::Cleaning) || roots.cleaning(spec.id) { return Ok(None); }
    let root = match spec.op {
        copy_op::DIFF | copy_op::COMMIT => roots.hold_upper(paths, spec.id),
        copy_op::IN | copy_op::OUT | copy_op::STAT | copy_op::EXPORT => roots.acquire(paths, spec.id, spec.layers),
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "unknown copy operation")),
    }?;
    let mut preparing = false;
    let result = (|| {
        if matches!(spec.op, copy_op::DIFF | copy_op::COMMIT) {
            Ok(Grant::Text(b"upper".to_vec()))
        } else if let View::Running(pid) = view && spec.op != copy_op::EXPORT {
            // Pin the live root while the PID is still owned. A delayed copy
            // may outlive it without looking up a reused /proc PID later.
            let name = sys::cstr(std::path::Path::new(&format!("/proc/{pid}/root")))?;
            // SAFETY: a valid path, immediately owning a returned descriptor.
            let fd = unsafe { libc::open(name.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
            if fd < 0 { Err(io::Error::last_os_error()) }
            else { Ok(Grant::Root(unsafe { OwnedFd::from_raw_fd(fd) })) }
        } else {
            // Reopened after unsharing: a parent namespace FD would target
            // the wrong mount namespace for stopped-container volumes.
            debug_assert_eq!(root, paths.containers.join(spec.id).join("rootfs"));
            preparing = true;
            Ok(Grant::Text(b"stopped".to_vec()))
        }
    })();
    match result {
        Ok(grant) => {
            if preparing { roots.begin_copy(); }
            Ok(Some((grant, spec.id.to_owned(), preparing)))
        }
        Err(error) => { roots.release(spec.id); Err(error) }
    }
}

fn job(client: OwnedFd, control: OwnedFd, paths: &Paths) -> io::Result<()> {
    close_others(client.as_raw_fd(), control.as_raw_fd())?;
    let mut conn = File::from(client);
    let result = (|| {
        let mut frame = Vec::new();
        super::read_frame(&mut conn, &mut frame)?;
        let spec = match asagiri_proto::decode(&frame) {
            Ok(Msg::Copy(spec)) => spec,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "expected a copy request")),
        };
        // mount_rootfs also keeps only the first occurrence. Compact the
        // request here so repeated empty layers cannot exhaust its bound.
        let mut seen = std::collections::HashSet::new();
        let layers: Vec<&str> = spec.layers.iter().filter(|layer| seen.insert(*layer)).collect();
        let mut request = vec![b'R'];
        asagiri_proto::encode_copy(&mut request, spec.op, spec.id, "", 0, &layers, "", &[]);
        if request.len() > MAX_REQUEST {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "copy layer list exceeds the root request limit"));
        }
        send(control.as_raw_fd(), &request)?;
        let grant = sys::recv_fd(control.as_raw_fd())?;
        match grant {
            sys::Received::Message(ref message) if message == "upper" => {
                super::layer_job(&mut conn, &spec, paths)?;
                Ok(Vec::new())
            }
            sys::Received::Message(ref message) if message == "stopped" => {
                super::enter_stopped(&spec, paths)?;
                send(control.as_raw_fd(), b"N")?;
                super::archive_job(&mut conn, &spec)
            }
            sys::Received::Fd(root) => {
                // SAFETY: an owned directory FD sent by the agent. Drop it
                // after chroot/chdir so no outside directory stays open.
                unsafe {
                    sys::check(libc::fchdir(root.as_raw_fd()) as _, "copy root")?;
                    sys::check(libc::chroot(c".".as_ptr()) as _, "copy chroot")?;
                    sys::check(libc::chdir(c"/".as_ptr()) as _, "copy chdir")?;
                }
                drop(root);
                super::archive_job(&mut conn, &spec)
            }
            sys::Received::Message(message) => Err(io::Error::other(message)),
        }
    })();
    let reply = match result {
        Ok(reply) if reply.len() <= MAX_REPLY => reply,
        Ok(_) => error_reply("copy metadata exceeds the reply limit"),
        Err(error) => error_reply(&error.to_string()),
    };
    let mut packet = Vec::with_capacity(reply.len() + 1);
    packet.push(b'F');
    packet.extend_from_slice(&reply);
    send(control.as_raw_fd(), &packet)
}

fn error_reply(message: &str) -> Vec<u8> {
    let mut reply = Vec::new();
    Msg::Error(&message.chars().take(2048).collect::<String>()).encode(&mut reply);
    reply
}

fn channel() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut pair = [-1; 2];
    // SAFETY: a two-element output array; successful descriptors are fresh.
    sys::check(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, pair.as_mut_ptr()) } as _, "copy channel")?;
    Ok(unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) })
}

fn send(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    // SAFETY: our socket and a readable slice; packet writes are atomic.
    let n = unsafe { libc::send(fd, bytes.as_ptr().cast(), bytes.len(), libc::MSG_NOSIGNAL) };
    if n < 0 { return Err(io::Error::last_os_error()); }
    if n as usize != bytes.len() { return Err(io::Error::new(io::ErrorKind::WriteZero, "short copy control packet")); }
    Ok(())
}

fn receive(fd: RawFd) -> io::Result<Option<Vec<u8>>> {
    let mut packet = vec![0u8; MAX_REQUEST + 1];
    // MSG_TRUNC returns the original packet length, so oversized messages
    // cannot be mistaken for a complete prefix.
    // SAFETY: a socket and writable buffer that we own.
    let n = unsafe { libc::recv(fd, packet.as_mut_ptr().cast(), packet.len(), libc::MSG_DONTWAIT | libc::MSG_TRUNC) };
    if n < 0 {
        let error = io::Error::last_os_error();
        return if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) { Ok(None) } else { Err(error) };
    }
    if n as usize > packet.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized copy control packet")); }
    packet.truncate(n as usize);
    Ok(Some(packet))
}

fn close_others(a: RawFd, b: RawFd) -> io::Result<()> {
    let mut next = 3u32;
    for keep in [a.min(b), a.max(b)] {
        if keep < 3 { continue; }
        if next < keep as u32 {
            // SAFETY: closing only this child's unrelated descriptors.
            sys::check(unsafe { libc::syscall(libc::SYS_close_range, next, keep as u32 - 1, 0) }, "copy close_range")?;
        }
        next = keep as u32 + 1;
    }
    // SAFETY: preserves both required descriptors and stdio.
    sys::check(unsafe { libc::syscall(libc::SYS_close_range, next, u32::MAX, 0) }, "copy close_range").map(drop)
}

fn join(pid: libc::pid_t) {
    let mut status = 0;
    let observed = loop {
        // SAFETY: first establish whether this is still our unreaped child.
        let n = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if n >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted { break n; }
    };
    if observed != 0 { return; }
    // SAFETY: waitpid established that this PID is still owned and unreaped.
    unsafe { libc::kill(pid, libc::SIGKILL); }
    loop {
        // SAFETY: wait for that exact child before releasing its root.
        if unsafe { libc::waitpid(pid, &mut status, 0) } >= 0
            || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted { break; }
    }
}
