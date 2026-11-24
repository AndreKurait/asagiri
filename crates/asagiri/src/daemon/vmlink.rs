//! One vsock connection to the guest agent, shared by every daemon thread.
//!
//! The agent answers requests in the order it receives them, and tags
//! container output and exits with the container id. So: senders take the
//! write lock, queue a reply slot and write; a single reader thread hands each
//! reply to the oldest waiting slot and each container event to the engine.

// The file-view replies serve the Finder view, which only macOS has.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex};

use asagiri_proto::{HEADER, LayerBegin, Msg, Stats, Stream};
use asagiri_vmm::Vm;

/// A reply to one request.
#[derive(Debug)]
pub enum Reply {
    LayerStatus(bool),
    LayerStored,
    /// The process's PID, as the agent's kernel numbers it.
    Started(u32),
    StartRejected(String),
    Stats(Stats),
    Pong,
    FsStat(FileInfo),
    FsList(Vec<FileInfo>),
    FsData(Vec<u8>),
    Ack,
    ContainerStats(asagiri_proto::ContainerStats),
    Top(Vec<String>),
    Listening(Vec<String>),
    UsbNodes(Vec<String>),
    Uplink([u8; 4], [u8; 4]),
    Error(String),
}

#[derive(Debug)]
pub(super) struct StartRejected(pub String);

impl std::fmt::Display for StartRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}
impl std::error::Error for StartRejected {}

/// One file inside a container, as the file browser sees it.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    pub mode: u32,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: u32,
}

impl From<asagiri_proto::FsEntry<'_>> for FileInfo {
    fn from(e: asagiri_proto::FsEntry<'_>) -> FileInfo {
        FileInfo { name: e.name.to_owned(), mode: e.mode, size: e.size, mtime: e.mtime, mtime_nsec: e.mtime_nsec }
    }
}


/// Something a container did, routed to the engine.
#[derive(Debug)]
pub enum Event<'a> {
    Output { id: &'a str, stream: Stream, data: &'a [u8] },
    /// `oom` when the kernel killed a process in it for memory.
    Exited { id: &'a str, code: i32, oom: bool },
    /// The guest is short of memory right now.
    Pressure,
    /// The connection is gone: the VM stopped.
    Lost,
}

pub struct VmLink {
    writer: Mutex<Writer>,
    /// Closes the transport without waiting for a writer blocked in the guest.
    control: File,
    /// Stdin credit per running process; see `input`.
    windows: Arc<Windows>,
    /// The guest's latest `Usage` lines, for `usage`.
    usage: Arc<UsageSlot>,
    pending: Arc<Mutex<VecDeque<SyncSender<Reply>>>>,
    cleanup: Arc<CleanupBarrier>,
    /// Owns the VM until retirement has confirmed shutdown and cleanup.
    _vm: Mutex<Vm>,
    /// Opens vsock streams without that mutex, so connects run side by side.
    connector: asagiri_vmm::Connector,
}

impl std::fmt::Debug for VmLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VmLink")
    }
}

/// The transport reader never waits here: it installs the barrier before
/// delivering Exited, then completes it independently of request replies.
#[derive(Default)]
struct CleanupBarrier {
    state: Mutex<(HashSet<String>, bool)>,
    changed: Condvar,
}
impl CleanupBarrier {
    fn pending(&self, id: &str) { self.state.lock().unwrap_or_else(|e| e.into_inner()).0.insert(id.to_owned()); }
    fn done(&self, id: &str) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0.remove(id);
        self.changed.notify_all();
    }
    fn lost(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.changed.notify_all();
    }
    fn wait(&self, id: &str) -> io::Result<()> {
        self.wait_for(id, std::time::Duration::from_secs(30))
    }
    fn wait_for(&self, id: &str, timeout: std::time::Duration) -> io::Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.0.contains(id) && !state.1 {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                // Keep the barrier: a later request may retry, but no request
                // may reuse these paths until the agent confirms completion.
                return Err(io::Error::new(io::ErrorKind::TimedOut,
                    format!("container {id} is still completing filesystem cleanup; retry shortly")));
            };
            state = self.changed.wait_timeout(state, remaining).unwrap_or_else(|e| e.into_inner()).0;
        }
        if state.1 { Err(io::Error::new(io::ErrorKind::BrokenPipe, "VM stopped during filesystem cleanup")) } else { Ok(()) }
    }
}

struct Writer {
    conn: File,
    out: Vec<u8>,
    blob: Vec<u8>,
}

impl Writer {
    fn check_frame(&self) -> io::Result<()> {
        if self.out.len() > asagiri_proto::MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                format!("agent request is {} bytes, larger than the {}-byte frame limit", self.out.len(), asagiri_proto::MAX_FRAME)));
        }
        Ok(())
    }

    fn request(&mut self, windows: &Windows, pending: &Mutex<VecDeque<SyncSender<Reply>>>, body: Option<(&Path, u64)>) -> io::Result<mpsc::Receiver<Reply>> {
        // Refusing an unsent frame must not consume the next request's reply.
        self.check_frame()?;
        let (tx, rx) = mpsc::sync_channel(1);
        // Queue the slot before writing, so the reply can never arrive first.
        {
            let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
            if windows.lock().lost {
                return Err(io::Error::other("the VM stopped"));
            }
            pending.push_back(tx);
        }
        self.write(windows, body)?;
        Ok(rx)
    }

    fn write(&mut self, windows: &Windows, body: Option<(&Path, u64)>) -> io::Result<()> {
        // Also bounds messages without replies. A local size error leaves the
        // stream usable; only a write failure can have sent a partial frame.
        self.check_frame()?;
        let result = self.write_request(body);
        if result.is_err() {
            // A partial frame cannot be retried on this stream. Publish
            // loss now, then wake the reader to do the exit accounting.
            windows.lose();
            use std::os::fd::AsRawFd;
            // SAFETY: conn owns a live stream socket for this entire call.
            unsafe { libc::shutdown(self.conn.as_raw_fd(), libc::SHUT_RDWR); }
        }
        result
    }

    fn write_request(&mut self, body: Option<(&Path, u64)>) -> io::Result<()> {
        self.conn.write_all(&self.out)?;
        if let Some((path, size)) = body {
            let mut file = File::open(path)?;
            let mut left = size;
            while left > 0 {
                let want = left.min(self.blob.len() as u64) as usize;
                let n = file.read(&mut self.blob[..want])?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "layer blob shrank while sending"));
                }
                self.conn.write_all(&self.blob[..n])?;
                left -= n as u64;
            }
        }
        Ok(())
    }
}

/// How much stdin each running process may still be sent before the guest
/// reports some of it consumed. An entry exists from `run`/`exec` until
/// `Exited`; `input` for a process without one is discarded.
#[derive(Default)]
struct Windows {
    credit: Mutex<Credits>,
    changed: Condvar,
}

#[derive(Default)]
struct Credits {
    by_id: HashMap<String, usize>,
    /// The link is gone; nothing will ever be consumed.
    lost: bool,
    loss_handled: bool,
}

impl Windows {
    fn lock(&self) -> std::sync::MutexGuard<'_, Credits> {
        self.credit.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn open(&self, id: &str) {
        self.lock().by_id.insert(id.to_owned(), asagiri_proto::INPUT_WINDOW);
    }
    fn close(&self, id: &str) {
        self.lock().by_id.remove(id);
        self.changed.notify_all();
    }
    fn add(&self, id: &str, n: usize) {
        if let Some(c) = self.lock().by_id.get_mut(id) {
            *c = (*c + n).min(asagiri_proto::INPUT_WINDOW);
        }
        self.changed.notify_all();
    }
    fn lose(&self) {
        let mut g = self.lock();
        g.lost = true;
        g.by_id.clear();
        drop(g);
        self.changed.notify_all();
    }
    fn finish_loss(&self) {
        self.lock().loss_handled = true;
        self.changed.notify_all();
    }

    /// Wait for exit accounting before replacing the VM. The caller must
    /// hold no state lock needed by Lost, and Lost must not wait for link.
    fn is_lost(&self) -> bool {
        let mut g = self.lock();
        while g.lost && !g.loss_handled {
            g = self.changed.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        g.lost
    }

    fn wait_for_loss(&self, timeout: std::time::Duration) -> io::Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        let mut g = self.lock();
        while !g.loss_handled {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "the previous VM's exit accounting did not finish"));
            }
            g = self.changed.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        Ok(())
    }
    /// Waits until `n` bytes may be sent to `id` and takes them. `Ok(false)`:
    /// the process is not running, the bytes should be dropped.
    fn take(&self, id: &str, n: usize) -> io::Result<bool> {
        let mut g = self.lock();
        loop {
            if g.lost {
                return Err(io::Error::other("the VM stopped"));
            }
            let Some(c) = g.by_id.get_mut(id) else { return Ok(false) };
            if *c >= n {
                *c -= n;
                return Ok(true);
            }
            g = self.changed.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Where the reader thread leaves `Usage` for whoever asked: a count of
/// answers so far, and the latest one.
#[derive(Default)]
struct UsageSlot {
    lines: Mutex<(u64, Option<Vec<String>>)>,
    arrived: Condvar,
}

impl VmLink {
    pub fn is_lost(&self) -> bool {
        self.windows.is_lost()
    }

    /// Observes disconnection without waiting for the reader's cleanup.
    pub fn is_disconnected(&self) -> bool {
        self.windows.lock().lost
    }

    /// Starts the reader thread. `on_event` runs on that thread, so it must
    /// not block for long: it holds up every reply behind it.
    pub fn new(vm: Vm, conn: File, on_event: impl Fn(Event<'_>) + Send + 'static) -> io::Result<VmLink> {
        let pending: Arc<Mutex<VecDeque<SyncSender<Reply>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let mut reader = conn.try_clone()?;
        let control = conn.try_clone()?;
        let queue = pending.clone();
        let windows = Arc::new(Windows::default());
        let wins = windows.clone();
        let usage = Arc::new(UsageSlot::default());
        let slot = usage.clone();
        let cleanup = Arc::new(CleanupBarrier::default());
        let cleaning = cleanup.clone();
        std::thread::Builder::new().name("asa-vm-reader".into()).spawn(move || {
            let mut frame = Vec::with_capacity(64 * 1024);
            while read_frame(&mut reader, &mut frame).is_ok() {
                let msg = asagiri_proto::decode(&frame);
                let reply = match msg {
                    Ok(Msg::Output { id, stream, data }) => {
                        on_event(Event::Output { id, stream, data });
                        continue;
                    }
                    // Not a reply: the guest measures on a thread and sends
                    // this when done, between other replies.
                    Ok(Msg::Usage(lines)) => {
                        let mut g = slot.lines.lock().unwrap_or_else(|e| e.into_inner());
                        g.0 += 1;
                        g.1 = Some(lines.iter().map(str::to_owned).collect());
                        drop(g);
                        slot.arrived.notify_all();
                        continue;
                    }
                    Ok(Msg::CleanupPending(id)) => { cleaning.pending(id); continue; }
                    Ok(Msg::CleanupDone(id)) => { cleaning.done(id); continue; }
                    Ok(Msg::Exited(e)) => {
                        wins.close(e.id);
                        on_event(Event::Exited { id: e.id, code: e.code, oom: e.oom_killed });
                        continue;
                    }
                    Ok(Msg::InputConsumed { id, bytes }) => {
                        wins.add(id, bytes as usize);
                        continue;
                    }
                    Ok(Msg::MemoryPressure) => {
                        on_event(Event::Pressure);
                        continue;
                    }
                    Ok(Msg::LayerStatus(have)) => Reply::LayerStatus(have),
                    Ok(Msg::LayerStored(_)) => Reply::LayerStored,
                    Ok(Msg::Started { pid, .. }) => Reply::Started(pid),
                    Ok(Msg::StartRejected(message)) => Reply::StartRejected(message.to_owned()),
                    Ok(Msg::Stats(s)) => Reply::Stats(s),
                    Ok(Msg::Pong(_)) => Reply::Pong,
                    Ok(Msg::FsStat(e)) => Reply::FsStat(FileInfo::from(e)),
                    Ok(Msg::FsList { count, raw }) => Reply::FsList(
                        asagiri_proto::fs_entries(count, raw)
                            .map(FileInfo::from)
                            .collect(),
                    ),
                    Ok(Msg::FsData(d)) => Reply::FsData(d.to_vec()),
                    Ok(Msg::Ack) => Reply::Ack,
                    Ok(Msg::ContainerStats(s)) => Reply::ContainerStats(s),
                    Ok(Msg::Top(lines)) => Reply::Top(lines.iter().map(str::to_owned).collect()),
                    Ok(Msg::Listening(lines)) => Reply::Listening(lines.iter().map(str::to_owned).collect()),
                    Ok(Msg::UsbNodes(lines)) => Reply::UsbNodes(lines.iter().map(str::to_owned).collect()),
                    Ok(Msg::Uplink { ip, gateway }) => Reply::Uplink(ip, gateway),
                    Ok(Msg::Error(e)) => Reply::Error(e.to_owned()),
                    Ok(other) => Reply::Error(format!("unexpected message from guest: {other:?}")),
                    Err(e) => Reply::Error(format!("bad frame from guest: {e:?}")),
                };
                if let Some(slot) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front() {
                    let _ = slot.send(reply);
                }
            }
            // Publish loss before waking callers: a retry must not borrow
            // this dead link while the engine is still accounting exits.
            wins.lose();
            cleaning.lost();
            for slot in queue.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
                let _ = slot.send(Reply::Error("the VM stopped".into()));
            }
            on_event(Event::Lost);
            wins.finish_loss();
        })?;
        Ok(VmLink {
            writer: Mutex::new(Writer { conn, out: Vec::with_capacity(4096), blob: vec![0; 256 * 1024] }),
            control,
            pending,
            cleanup,
            windows,
            usage,
            connector: vm.connector(),
            _vm: Mutex::new(vm),
        })
    }

    /// Sends one request and waits for its reply.
    fn call(&self, encode: impl FnOnce(&mut Vec<u8>), body: Option<(&Path, u64)>) -> io::Result<Reply> {
        let rx = {
            let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            w.out.clear();
            encode(&mut w.out);
            w.request(&self.windows, &self.pending, body)?
        };
        match rx.recv() {
            Ok(Reply::Error(e)) => Err(io::Error::other(e)),
            Ok(r) => Ok(r),
            Err(_) => Err(io::Error::other("the VM stopped")),
        }
    }

    /// Sends a message that has no reply.
    fn send(&self, encode: impl FnOnce(&mut Vec<u8>)) -> io::Result<()> {
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.out.clear();
        encode(&mut w.out);
        w.write(&self.windows, None)
    }

    /// What the guest's layers, containers and volumes take on disk: the
    /// `KIND NAME BYTES` lines of `Msg::Usage`, or None if they don't come
    /// within `wait` or the VM stops.
    pub fn usage(&self, wait: std::time::Duration) -> Option<Vec<String>> {
        let asked = self.usage.lines.lock().unwrap_or_else(|e| e.into_inner()).0;
        self.send(|o| Msg::UsageQuery.encode(o)).ok()?;
        let g = self.usage.lines.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = self.usage.arrived.wait_timeout_while(g, wait, |g| g.0 == asked).unwrap_or_else(|e| e.into_inner());
        if g.0 == asked { None } else { g.1.clone() }
    }

    pub fn has_layer(&self, digest: &str) -> io::Result<bool> {
        match self.call(|o| Msg::LayerQuery(digest).encode(o), None)? {
            Reply::LayerStatus(have) => Ok(have),
            other => Err(io::Error::other(format!("expected LayerStatus, got {other:?}"))),
        }
    }

    pub fn send_layer(&self, digest: &str, media_type: &str, blob: &Path) -> io::Result<()> {
        let size = std::fs::metadata(blob)?.len();
        let socket = self.connect(asagiri_proto::LAYER_PORT)?;
        // Keep the framework's connection object alive on macOS as well.
        let mut connection = File::from(socket.fd.try_clone()?);
        let mut frame = Vec::new();
        Msg::LayerBegin(LayerBegin { digest, media_type, size }).encode(&mut frame);
        connection.write_all(&frame)?;
        let copied = io::copy(&mut File::open(blob)?.take(size), &mut connection)?;
        if copied != size {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "layer blob shrank while sending"));
        }
        read_frame(&mut connection, &mut frame)?;
        match asagiri_proto::decode(&frame) {
            Ok(Msg::LayerStored(_)) => Ok(()),
            Ok(Msg::Error(error)) => Err(io::Error::other(error.to_owned())),
            other => Err(io::Error::other(format!("expected LayerStored, got {other:?}"))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &self,
        id: &str,
        layers: &[String],
        argv: &[String],
        env: &[String],
        cwd: &str,
        user: &str,
        hostname: &str,
        flags: u32,
        net: asagiri_proto::Net<'_, String>,
        mounts: &[String],
        limits: &asagiri_proto::Limits,
        caps: u64,
        opts: &[String],
        extra_networks: &[asagiri_proto::RunNetwork<'_, String>],
    ) -> io::Result<u32> {
        self.wait_cleanup(id)?;
        self.windows.open(id);
        let r = loop {
            let r = self.call(|o| asagiri_proto::encode_run(o, id, layers, argv, env, cwd, user, hostname, flags, net.clone(), mounts, limits, caps, opts, extra_networks), None).and_then(started);
            if !matches!(&r, Err(error) if error.to_string() == asagiri_proto::ROOT_CLEANUP_PENDING) { break r; }
            // A copy reply travels on another socket, so the next Run may
            // arrive before its root's CleanupPending was read. The guest
            // sends Pending before the retryable error, preserving the
            // ordinary reply queue. Other calls can proceed during this wait.
            if let Err(error) = self.wait_cleanup(id) { break Err(error); }
        };
        if r.is_err() {
            self.windows.close(id);
        }
        r
    }

    fn ack(&self, encode: impl FnOnce(&mut Vec<u8>)) -> io::Result<()> {
        match self.call(encode, None)? {
            Reply::Ack => Ok(()),
            other => Err(io::Error::other(format!("expected Ack, got {other:?}"))),
        }
    }

    pub fn update(&self, id: &str, limits: asagiri_proto::Limits) -> io::Result<()> {
        self.ack(|o| Msg::Update { id, limits }.encode(o))
    }

    pub fn freeze(&self, id: &str, frozen: bool) -> io::Result<()> {
        self.ack(|o| Msg::Freeze { id, frozen }.encode(o))
    }

    pub fn container_stats(&self, id: &str) -> io::Result<asagiri_proto::ContainerStats> {
        match self.call(|o| Msg::StatsQuery { id }.encode(o), None)? {
            Reply::ContainerStats(s) => Ok(s),
            other => Err(io::Error::other(format!("expected ContainerStats, got {other:?}"))),
        }
    }

    pub fn top(&self, id: &str) -> io::Result<Vec<String>> {
        match self.call(|o| Msg::TopQuery { id }.encode(o), None)? {
            Reply::Top(lines) => Ok(lines),
            other => Err(io::Error::other(format!("expected Top, got {other:?}"))),
        }
    }

    /// The container's LISTEN lines from /proc/net/tcp and tcp6.
    pub fn listening(&self, id: &str) -> io::Result<Vec<String>> {
        match self.call(|o| Msg::ListenQuery { id }.encode(o), None)? {
            Reply::Listening(lines) => Ok(lines),
            other => Err(io::Error::other(format!("expected Listening, got {other:?}"))),
        }
    }

    /// `PORT\tPATH\tWHAT` for the nodes of the USB devices attached to the
    /// VM (`Msg::UsbNodes`).
    pub fn usb_nodes(&self) -> io::Result<Vec<String>> {
        match self.call(|o| Msg::UsbNodesQuery.encode(o), None)? {
            Reply::UsbNodes(lines) => Ok(lines),
            other => Err(io::Error::other(format!("expected UsbNodes, got {other:?}"))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// Routes `dst/prefix` through `via` in the VM, or removes that route.
    pub fn route(&self, dst: [u8; 4], prefix: u8, via: [u8; 4], add: bool) -> io::Result<()> {
        self.ack(|o| Msg::Route { dst, prefix, via, add }.encode(o))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn net_attach(&self, id: &str, n: u32, bridge: &str, ip: [u8; 4], prefix: u8, gateway: [u8; 4], dns_names: &[String], opts: &[String]) -> io::Result<()> {
        let ifname = format!("eth{n}");
        self.ack(|o| asagiri_proto::encode_net_attach(o, id, &ifname, bridge, ip, prefix, gateway, dns_names, opts))
    }

    pub fn dns_rename(&self, id: &str, bridge: &str, ip: [u8; 4], names: &[String]) -> io::Result<()> {
        self.ack(|o| asagiri_proto::encode_dns_rename(o, id, bridge, ip, names))
    }

    pub fn net_detach(&self, id: &str, n: u32, bridge: &str, ip: [u8; 4]) -> io::Result<()> {
        let ifname = format!("eth{n}");
        self.ack(|o| Msg::NetDetach { id, ifname: &ifname, bridge, ip }.encode(o))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn exec(&self, container_id: &str, exec_id: &str, argv: &[String], env: &[String], cwd: &str, user: &str, flags: u32, caps: u64, opts: &[String]) -> io::Result<u32> {
        self.windows.open(exec_id);
        let r = self.call(|o| asagiri_proto::encode_exec(o, container_id, exec_id, argv, env, cwd, user, flags, caps, opts), None).and_then(started);
        if r.is_err() {
            self.windows.close(exec_id);
        }
        r
    }

    /// Sends stdin for a process. Blocks while the guest already holds
    /// `INPUT_WINDOW` bytes it has not consumed, so the pressure reaches the
    /// client (its write blocks) instead of the agent dropping input.
    pub fn input(&self, id: &str, data: &[u8]) -> io::Result<()> {
        // Frames are bounded, so large writes go in pieces.
        for piece in data.chunks(asagiri_proto::MAX_OUTPUT_CHUNK) {
            if !self.windows.take(id, piece.len())? {
                return Ok(());
            }
            self.send(|o| Msg::Input { id, data: piece }.encode(o))?;
        }
        Ok(())
    }

    pub fn close_stdin(&self, id: &str) -> io::Result<()> {
        self.send(|o| Msg::CloseStdin { id }.encode(o))
    }

    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> io::Result<()> {
        self.send(|o| Msg::Resize { id, rows, cols }.encode(o))
    }

    /// The VM's address on the NAT network and its gateway (the Mac's own
    /// address there). `Err` with no lease yet, or from an older agent.
    pub fn uplink(&self) -> io::Result<([u8; 4], [u8; 4])> {
        match self.call(|o| Msg::UplinkQuery.encode(o), None)? {
            Reply::Uplink(ip, gateway) => Ok((ip, gateway)),
            Reply::Error(e) => Err(io::Error::other(e)),
            other => Err(io::Error::other(format!("expected Uplink, got {other:?}"))),
        }
    }

    /// Configures the VM's uplink (`eth0`) directly when macOS DHCP did not
    /// answer. Ignored by the guest if it already obtained a DHCP lease.
    pub fn set_uplink(&self, ip: [u8; 4], prefix: u8, gateway: [u8; 4], dns: [u8; 4]) -> io::Result<()> {
        self.ack(|o| Msg::SetUplink { ip, prefix, gateway, dns }.encode(o))
    }

    /// Forwards a published port in the kernel (on a Linux host; in the VM,
    /// from the NAT network). `Err` means the agent could not; the caller
    /// falls back to relaying.
    pub fn publish(&self, p: asagiri_proto::Publish<'_>) -> io::Result<()> {
        match self.call(|o| Msg::Publish(p).encode(o), None)? {
            Reply::Ack => Ok(()),
            Reply::Error(e) => Err(io::Error::other(e)),
            other => Err(io::Error::other(format!("expected Ack, got {other:?}"))),
        }
    }
    pub fn unpublish(&self, id: &str) -> io::Result<()> {
        // No reply: this runs when a container exits, on the link's own reader
        // thread, which could not wait for one.
        self.send(|o| Msg::Unpublish { id }.encode(o))
    }
    pub fn remove_volume(&self, name: &str) -> io::Result<()> {
        self.send(|o| Msg::RemoveVolume { name }.encode(o))
    }

    /// A removed network's bridge goes, with its addresses, routes and DNS
    /// server (and the IPv6 NAT table with the last IPv6 network).
    pub fn remove_bridge(&self, bridge: &str) -> io::Result<()> {
        self.send(|o| Msg::RemoveBridge { bridge }.encode(o))
    }

    /// Files the Mac changed under bind mounts (`OP\tPATH` lines, PATH in
    /// the VM), for the guest to raise as inotify events. No reply.
    pub fn fs_events<S: AsRef<str>>(&self, lines: &[S]) -> io::Result<()> {
        self.send(|o| asagiri_proto::encode_fs_events(o, lines))
    }

    /// The Mac's wall clock, for the guest. Sent at boot, and again while it
    /// runs: the VM's clock stops while the Mac sleeps.
    pub fn set_time(&self) -> io::Result<()> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        self.send(|o| Msg::SetTime(now.as_nanos() as u64).encode(o))
    }

    /// Tells the guest which stopped containers still exist; it deletes the rest.
    pub fn retain(&self, ids: &[String]) -> io::Result<()> {
        self.send(|o| asagiri_proto::encode_retain(o, ids))
    }

    /// Tells the guest which unpacked layers are still used; it deletes the rest.
    pub fn retain_layers(&self, digests: &[String]) -> io::Result<()> {
        self.send(|o| asagiri_proto::encode_retain_layers(o, digests))
    }

    pub fn kill(&self, id: &str, signal: u32) -> io::Result<()> {
        self.send(|o| Msg::Kill { id, signal }.encode(o))
    }

    pub fn wait_cleanup(&self, id: &str) -> io::Result<()> { self.cleanup.wait(id) }

    pub fn remove(&self, id: &str) -> io::Result<()> {
        self.wait_cleanup(id)?;
        self.send(|o| Msg::Remove { id }.encode(o))
    }

    /// Sets the balloon so the guest runs in `bytes`; returns the old target.
    pub fn set_memory_target(&self, bytes: u64) -> Option<u64> {
        self._vm.lock().unwrap_or_else(|e| e.into_inner()).set_memory_target(bytes)
    }

    /// Accepts guest-initiated streams on `port`.
    pub fn listen(&self, port: u32, handler: asagiri_vmm::Handler) -> io::Result<()> {
        self._vm.lock().unwrap_or_else(|e| e.into_inner()).listen(port, handler)
    }

    /// A new stream to a port the guest agent listens on.
    pub fn connect(&self, port: u32) -> io::Result<asagiri_vmm::VsockStream> {
        self.connector.connect(port)
    }

    pub fn fs_stat(&self, id: &str, path: &str) -> io::Result<FileInfo> {
        match self.call(|o| Msg::FsStatRequest { id, path }.encode(o), None)? {
            Reply::FsStat(f) => Ok(f),
            other => Err(io::Error::other(format!("expected FsStat, got {other:?}"))),
        }
    }

    pub fn fs_list(&self, id: &str, path: &str) -> io::Result<Vec<FileInfo>> {
        match self.call(|o| Msg::FsListRequest { id, path }.encode(o), None)? {
            Reply::FsList(v) => Ok(v),
            other => Err(io::Error::other(format!("expected FsList, got {other:?}"))),
        }
    }

    pub fn fs_read(&self, id: &str, path: &str, offset: u64, len: u32) -> io::Result<Vec<u8>> {
        match self.call(|o| Msg::FsRead { id, path, offset, len }.encode(o), None)? {
            Reply::FsData(d) => Ok(d),
            other => Err(io::Error::other(format!("expected FsData, got {other:?}"))),
        }
    }

    pub fn stats(&self) -> io::Result<Stats> {
        match self.call(|o| Msg::StatsRequest.encode(o), None)? {
            Reply::Stats(s) => Ok(s),
            other => Err(io::Error::other(format!("expected Stats, got {other:?}"))),
        }
    }

    /// Asks the guest to power off. The VM itself is released when the link drops.
    pub fn shutdown(&self) {
        let _ = self.send(|o| Msg::Shutdown.encode(o));
    }

    /// Success confirms both shutdown and release of the old generation's
    /// resources, even while relay threads retain this link.
    pub fn retire(&self) -> io::Result<()> {
        self._vm.lock().unwrap_or_else(|e| e.into_inner())
            .retire(std::time::Duration::from_secs(10), std::time::Duration::from_secs(10))?;
        // The backend can finish before the reader sees EOF. Release its
        // mutex, wake the reader, then wait for Lost to clear old state.
        // Otherwise a late Lost could stop containers in the replacement.
        use std::os::fd::AsRawFd;
        // SAFETY: control owns a stream socket. Shutdown also wakes any
        // blocked writer without needing its lock; an already closed peer
        // is harmless. The accounting barrier below confirms completion.
        unsafe { libc::shutdown(self.control.as_raw_fd(), libc::SHUT_RDWR); }
        self.windows.wait_for_loss(std::time::Duration::from_secs(10))
    }

    #[cfg(test)]
    pub(super) fn hold_retirement(&self) -> std::sync::MutexGuard<'_, Vm> {
        self._vm.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    pub(super) fn interrupted_layer(&self, blob: &Path) -> io::Result<()> {
        self.call(|out| Msg::LayerBegin(LayerBegin {
            digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip", size: 1,
        }).encode(out), Some((blob, 1))).map(|_| ())
    }

    /// Waits for the guest to stop after `shutdown`, and forces it if it has
    /// not within `timeout`. False if it had to be forced.
    ///
    /// In a VM the guest flushes its disk before powering off; stopping the
    /// VM before then (as dropping it does) loses what was still cached, and
    /// files written shortly before come back empty. On a Linux host the
    /// agent kills what is left and removes its bridges and nftables table
    /// first; an agent left behind kept its containers, ports and sockets,
    /// and the next daemon could not start.
    pub fn wait_stopped(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let vm = self._vm.lock().unwrap_or_else(|e| e.into_inner());
        while vm.is_running() {
            if std::time::Instant::now() >= deadline {
                vm.force_stop();
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        true
    }
}

/// The PID a `Run` or `Exec` was answered with.
fn started(r: Reply) -> io::Result<u32> {
    match r {
        Reply::Started(pid) => Ok(pid),
        Reply::StartRejected(message) => Err(io::Error::other(StartRejected(message))),
        other => Err(io::Error::other(format!("expected Started, got {other:?}"))),
    }
}

fn read_frame(conn: &mut File, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.resize(4, 0);
    conn.read_exact(&mut buf[..4])?;
    let len = asagiri_proto::frame_len(buf[..4].try_into().expect("4 bytes"))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
    buf.resize(len.max(HEADER), 0);
    conn.read_exact(&mut buf[4..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    fn dense_seccomp_options() -> Vec<String> {
        // The Docker-valid 2,997-rule profile: three syscall families, a hole
        // at 0600, and a change of errno at 01000. Do not group or shrink it.
        let mut rules = Vec::new();
        for (name, argument) in [("chmod", 1), ("fchmodat", 2), ("fchmodat2", 2)] {
            for mode in (0..1000).filter(|mode| *mode != 0o600) {
                rules.push(serde_json::json!({
                    "names": [name], "action": "SCMP_ACT_ERRNO", "errnoRet": if mode < 0o1000 { 13 } else { 95 },
                    "args": [{"index": argument, "op": "SCMP_CMP_EQ", "value": mode}],
                }));
            }
        }
        assert_eq!(rules.len(), 2997);
        let profile = serde_json::json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": rules});
        super::super::engine::seccomp_lines(&[format!("seccomp={profile}")]).unwrap()
    }

    fn run_frame(opts: &[String], env: &[String]) -> Vec<u8> {
        let mut frame = Vec::new();
        let net = asagiri_proto::Net { bridge: "", ip: [0; 4], prefix: 0, gateway: [0; 4], extra_hosts: &[], dns_names: &[], opts: &[] };
        asagiri_proto::encode_run(&mut frame, "c1", &["sha256:layer".into()], &["sh".into(), "-c".into(), "true".into()],
            env, "/", "", "c1", 0, net, &["/source\t/target\tro".into()], &asagiri_proto::Limits::default(), 0, opts, &[]);
        frame
    }

    fn exchange_request(frame: Vec<u8>) -> Vec<u8> {
        let (socket, peer) = UnixStream::pair().unwrap();
        socket.set_write_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        let mut writer = Writer { conn: std::os::fd::OwnedFd::from(socket).into(), out: frame, blob: vec![0; 16] };
        let windows = Windows::default();
        let pending = Mutex::new(VecDeque::new());
        let mut received = Vec::new();
        std::thread::scope(|scope| {
            let caller = scope.spawn(|| {
                let reply = writer.request(&windows, &pending, None).unwrap();
                assert!(matches!(reply.recv_timeout(std::time::Duration::from_secs(10)).unwrap(), Reply::Started(42)));
            });
            let mut peer = File::from(std::os::fd::OwnedFd::from(peer));
            read_frame(&mut peer, &mut received).unwrap();
            pending.lock().unwrap().pop_front().unwrap().send(Reply::Started(42)).unwrap();
            caller.join().unwrap();
        });
        assert!(pending.lock().unwrap().is_empty());
        assert!(!windows.lock().lost);
        received
    }

    #[test]
    fn the_original_dense_seccomp_profile_reaches_the_agent_unchanged() {
        let options = dense_seccomp_options();
        assert_eq!(options[0].len(), 342_382);
        assert!(options[0].len() > asagiri_proto::MAX_FRAME / 4);
        let sent = run_frame(&options, &["A=1".into()]);
        let received = exchange_request(sent.clone());
        assert_eq!(received, sent);
        let Msg::Run(run) = asagiri_proto::decode(&received).unwrap() else { panic!("expected Run") };
        assert_eq!(run.opts.iter().collect::<Vec<_>>(), options);
        assert_eq!(run.env.iter().collect::<Vec<_>>(), ["A=1"]);
        assert_eq!(run.flags & asagiri_proto::run_flags::NO_SECCOMP, 0);
    }

    #[test]
    fn the_frame_budget_includes_every_field_and_the_header() {
        let options = dense_seccomp_options();
        let mut env: Vec<_> = (0..16).map(|i| format!("PADDING{i}=")).collect();
        let mut room = asagiri_proto::MAX_FRAME - run_frame(&options, &env).len();
        for (i, value) in env.iter_mut().enumerate() {
            let added = room / (16 - i);
            value.push_str(&"x".repeat(added));
            room -= added;
        }
        let exact = run_frame(&options, &env);
        assert_eq!(exact.len(), asagiri_proto::MAX_FRAME);
        assert_eq!(exchange_request(exact.clone()), exact);

        env[0].push('x');
        let oversized = run_frame(&options, &env);
        assert_eq!(oversized.len(), asagiri_proto::MAX_FRAME + 1);
        assert!(env.iter().all(|value| value.len() < 64 * 1024));
        assert!(options[0].len() < asagiri_proto::MAX_FRAME);
        assert!(asagiri_proto::frame_len(oversized[..4].try_into().unwrap()).is_err());
        assert_refused_without_stream_loss(oversized);
    }

    #[test]
    fn an_oversized_profile_is_refused_without_disabling_seccomp() {
        let profile = serde_json::json!({"defaultAction": "SCMP_ACT_ERRNO", "comment": "x".repeat(asagiri_proto::MAX_FRAME)});
        let options = super::super::engine::seccomp_lines(&[format!("seccomp={profile}")]).unwrap();
        assert!(options[0].contains("SCMP_ACT_ERRNO"));
        assert_refused_without_stream_loss(run_frame(&options, &[]));
    }

    fn assert_refused_without_stream_loss(frame: Vec<u8>) {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let mut writer = Writer { conn: std::os::fd::OwnedFd::from(socket).into(), out: frame, blob: vec![0; 16] };
        let windows = Windows::default();
        windows.open("unrelated");
        let (earlier, earlier_reply) = mpsc::sync_channel(1);
        let pending = Mutex::new(VecDeque::from([earlier]));
        // An oversized request must not open or send its streamed body either.
        let error = writer.request(&windows, &pending, Some((Path::new("/not-a-layer"), 1))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("frame limit"), "{error}");
        assert_eq!(pending.lock().unwrap().len(), 1, "no reply slot for the rejected request");
        assert_eq!(writer.write(&windows, None).unwrap_err().kind(), io::ErrorKind::InvalidInput, "messages without replies share the bound");
        assert!(!windows.lock().lost);
        assert_eq!(windows.lock().by_id["unrelated"], asagiri_proto::INPUT_WINDOW);
        peer.set_nonblocking(true).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap_err().kind(), io::ErrorKind::WouldBlock, "no bytes and no EOF");
        peer.set_nonblocking(false).unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();

        writer.out.clear();
        Msg::Ping(42).encode(&mut writer.out);
        let next = writer.request(&windows, &pending, None).unwrap();
        let mut received = Vec::new();
        read_frame(&mut std::os::fd::OwnedFd::from(peer).into(), &mut received).unwrap();
        assert_eq!(asagiri_proto::decode(&received).unwrap(), Msg::Ping(42));
        assert_eq!(pending.lock().unwrap().len(), 2);
        pending.lock().unwrap().pop_front().unwrap().send(Reply::Ack).unwrap();
        assert!(matches!(earlier_reply.try_recv().unwrap(), Reply::Ack));
        assert!(matches!(next.try_recv(), Err(mpsc::TryRecvError::Empty)));
        pending.lock().unwrap().pop_front().unwrap().send(Reply::Pong).unwrap();
        assert!(matches!(next.try_recv().unwrap(), Reply::Pong));
        assert!(pending.lock().unwrap().is_empty());
        assert!(!windows.lock().lost);
    }

    #[test]
    fn a_failed_write_disconnects_before_the_reader_accounts_loss() {
        let (socket, peer) = UnixStream::pair().unwrap();
        socket.shutdown(std::net::Shutdown::Write).unwrap();
        let mut writer = Writer { conn: std::os::fd::OwnedFd::from(socket).into(), out: vec![1], blob: vec![0; 16] };
        let windows = Windows::default();
        assert!(writer.write(&windows, None).is_err());
        assert!(windows.lock().lost, "a writer failure must publish loss before a start can record an ordinary failure");
        assert!(!windows.lock().loss_handled, "only the reader owns exit accounting");
        let mut peer = peer;
        assert_eq!(peer.read(&mut [0]).unwrap(), 0, "the failed transport is closed in both directions");
    }

    #[test]
    fn retirement_waits_for_loss_to_arrive_and_finish_accounting() {
        use std::time::Duration;
        let windows = Arc::new(Windows::default());
        let waiting = windows.clone();
        let (sent, received) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            sent.send(waiting.wait_for_loss(Duration::from_secs(2))).unwrap();
        });
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err(), "backend shutdown alone does not prove reader cleanup");
        windows.lose();
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err(), "publishing Lost precedes exit accounting");
        windows.finish_loss();
        received.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        waiter.join().unwrap();
        windows.wait_for_loss(Duration::ZERO).unwrap();
    }

    #[test]
    fn unfinished_accounting_does_not_authorize_a_replacement() {
        let windows = Windows::default();
        assert_eq!(windows.wait_for_loss(std::time::Duration::ZERO).unwrap_err().kind(), io::ErrorKind::TimedOut);
        windows.lose();
        assert_eq!(windows.wait_for_loss(std::time::Duration::ZERO).unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::CleanupBarrier;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[test]
    fn cleanup_blocks_only_its_container_and_completion_releases_waiters() {
        let barrier = Arc::new(CleanupBarrier::default());
        barrier.pending("cleaning");
        barrier.wait("unrelated").unwrap();
        let (sent, received) = mpsc::channel();
        let worker = barrier.clone();
        let thread = std::thread::spawn(move || { sent.send(worker.wait("cleaning").is_ok()).unwrap(); });
        assert!(received.recv_timeout(Duration::from_millis(20)).is_err());
        barrier.done("unrelated");
        assert!(received.recv_timeout(Duration::from_millis(20)).is_err());
        barrier.done("cleaning");
        assert!(received.recv_timeout(Duration::from_secs(2)).unwrap());
        thread.join().unwrap();
    }

    #[test]
    fn transport_loss_fails_pending_and_future_lifecycle_requests() {
        let barrier = Arc::new(CleanupBarrier::default());
        barrier.pending("cleaning");
        let (sent, received) = mpsc::channel();
        let worker = barrier.clone();
        let thread = std::thread::spawn(move || { sent.send(worker.wait("cleaning").unwrap_err().kind()).unwrap(); });
        assert!(received.recv_timeout(Duration::from_millis(20)).is_err());
        barrier.lost();
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), std::io::ErrorKind::BrokenPipe);
        assert!(barrier.wait("unrelated").is_err());
        thread.join().unwrap();
    }

    #[test]
    fn timeout_retains_the_barrier_until_cleanup_really_finishes() {
        let barrier = CleanupBarrier::default();
        barrier.pending("slow");
        for _ in 0..2 {
            assert_eq!(barrier.wait_for("slow", Duration::from_millis(1)).unwrap_err().kind(),
                std::io::ErrorKind::TimedOut);
            barrier.wait("unrelated").unwrap();
        }
        barrier.done("slow");
        barrier.wait("slow").unwrap();
    }
}
