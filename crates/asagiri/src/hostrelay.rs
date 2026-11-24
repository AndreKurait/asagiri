//! `asagiri hostrelay`: lets Asagiri's engine in WSL2 reach a proxy on Windows' own
//! loopback, `127.0.0.1:7892`, where most Windows proxy apps listen. In
//! WSL's default NAT networking the distro's 127.0.0.1 is its own, and
//! Windows' firewall keeps the WSL network away from Windows' ports, so a
//! proxy the browser uses was refused to every image pull.
//!
//! The connections take the way asagiri-win already talks to WSL: a wsl.exe
//! process's stdin and stdout. asagiri-win (crates/asagiri-win/src/relay.rs)
//! starts this and asks it to LISTEN for each proxy port; it listens on the
//! distro's 127.0.0.1 and sends each connection to it back as frames, and
//! asagiri-win dials the proxy on Windows. asagiri-win dials only the ports it
//! asked for, so nothing in WSL can reach any other Windows port this way.
//! The engine pulls through `127.0.0.1:<the port here>` (daemon/proxy.rs).
//!
//! Both ends share this file. Frames are the bridge's (crates/asagiri/src/bridge.rs):
//! kind, connection id (u32 BE), length (u32 BE), then the data.

// Each build uses one end: the WSL end in Linux's asagiri, the Windows end in
// asagiri-win. The tests run both.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// WSL → Windows: a connection to the proxy on the port in the data (u16 BE).
pub const OPEN: u8 = 1;
pub const DATA: u8 = 2;
/// No more data from this side (a half-close).
pub const EOF: u8 = 3;
/// The connection is gone, both ways.
pub const CLOSE: u8 = 4;
/// Windows → WSL: listen for this Windows port (u16 BE).
pub const LISTEN: u8 = 6;
/// WSL → Windows: the Windows port, then the distro's port that leads to
/// it (both u16 BE); 0 when it could not listen.
pub const LISTENING: u8 = 7;

/// Neither end sends more data than this in a frame; a longer one is refused
/// before it is allocated.
const CHUNK: usize = 64 * 1024;
const MAX_FRAME: usize = 1 << 20;
/// Frames waiting for one connection's writer. A full queue closes that
/// connection so the relay can keep reading control frames and detect EOF.
const QUEUE: usize = 64;

pub type Out = Arc<Mutex<dyn Write + Send>>;

pub fn send(out: &Out, kind: u8, id: u32, data: &[u8]) -> io::Result<()> {
    write_frame(&mut *out.lock().unwrap_or_else(PoisonError::into_inner), kind, id, data)
}

fn write_frame(w: &mut dyn Write, kind: u8, id: u32, data: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(9 + data.len());
    frame.push(kind);
    frame.extend_from_slice(&id.to_be_bytes());
    frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
    frame.extend_from_slice(data);
    w.write_all(&frame)?;
    w.flush()
}

/// The next frame, or None when the other end has gone.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, u32, Vec<u8>)>> {
    let mut h = [0u8; 9];
    loop {
        match r.read(&mut h[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    r.read_exact(&mut h[1..])?;
    let id = u32::from_be_bytes([h[1], h[2], h[3], h[4]]);
    let len = u32::from_be_bytes([h[5], h[6], h[7], h[8]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes")));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)?;
    Ok(Some((h[0], id, data)))
}

fn port_of(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

enum Op {
    Data(Vec<u8>),
    Eof,
}

#[derive(Clone)]
struct Connection {
    id: u32,
    owner: Arc<()>,
}

struct Entry {
    owner: Arc<()>,
    tx: SyncSender<Op>,
    stream: Option<TcpStream>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        if let Some(stream) = &self.stream {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

#[derive(Default)]
struct State {
    closed: bool,
    entries: HashMap<u32, Entry>,
    listeners: HashMap<u16, TcpListener>,
}

#[derive(Default)]
pub struct Conns(Mutex<State>);

struct CloseAll(Arc<Conns>);

impl Drop for CloseAll {
    fn drop(&mut self) {
        self.0.close_all();
    }
}

impl Conns {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn open(&self, id: u32) -> Option<(Connection, Receiver<Op>)> {
        let mut state = self.state();
        if state.closed || state.entries.contains_key(&id) {
            return None;
        }
        let (tx, rx) = sync_channel(QUEUE);
        let owner = Arc::new(());
        state.entries.insert(id, Entry { owner: owner.clone(), tx, stream: None });
        Some((Connection { id, owner }, rx))
    }

    fn close_all(&self) {
        let entries = {
            let mut state = self.state();
            state.closed = true;
            state.listeners.clear();
            std::mem::take(&mut state.entries)
        };
        drop(entries);
    }

    fn close(&self, connection: &Connection) -> bool {
        let mut state = self.state();
        if state.owns(connection) {
            state.entries.remove(&connection.id);
            return true;
        }
        false
    }

    // Serialize worker frames with frames from any replacement. Never hold the
    // state lock during transport I/O, so teardown can still shut down sockets.
    fn send(&self, out: &Out, connection: &Connection, kind: u8, data: &[u8]) -> io::Result<()> {
        let mut writer = out.lock().unwrap_or_else(PoisonError::into_inner);
        let owned = if kind == CLOSE {
            self.close(connection)
        } else {
            self.state().owns(connection)
        };
        if !owned {
            return Ok(());
        }
        let result = write_frame(&mut *writer, kind, connection.id, data);
        if result.is_err() {
            self.close_all();
        }
        result
    }

    fn dispatch(&self, out: &Out, kind: u8, id: u32, data: Vec<u8>) -> io::Result<()> {
        let op = match kind {
            DATA => Op::Data(data),
            EOF => Op::Eof,
            CLOSE => {
                self.state().entries.remove(&id);
                return Ok(());
            }
            _ => return Ok(()),
        };
        let overflow = {
            let state = self.state();
            state.entries.get(&id).and_then(|entry| {
                match entry.tx.try_send(op) {
                    Err(TrySendError::Full(_)) => Some(Connection { id, owner: entry.owner.clone() }),
                    _ => None,
                }
            })
        };
        if let Some(connection) = overflow {
            self.send(out, &connection, CLOSE, &[])?;
        }
        Ok(())
    }
}

impl State {
    fn owns(&self, connection: &Connection) -> bool {
        self.entries.get(&connection.id).is_some_and(|entry| Arc::ptr_eq(&entry.owner, &connection.owner))
    }
}

/// Joins `stream` to connection `id`: what it reads goes to the other end
/// as DATA, then EOF; what arrives for it (`rx`) is written to it.
fn attach(conns: &Arc<Conns>, out: &Out, connection: Connection, rx: Receiver<Op>, stream: TcpStream, target: Option<u16>) {
    let _ = stream.set_nodelay(true);
    let (Ok(reader), Ok(writer)) = (stream.try_clone(), stream.try_clone()) else {
        let _ = conns.send(out, &connection, CLOSE, &[]);
        return;
    };
    {
        let mut state = conns.state();
        if !state.owns(&connection) {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        state.entries.get_mut(&connection.id).unwrap().stream = Some(stream);
    }
    if let Some(target) = target {
        if conns.send(out, &connection, OPEN, &target.to_be_bytes()).is_err() {
            return;
        }
    }
    // Each half that ends counts; the second one forgets the connection.
    let halves = Arc::new(AtomicU8::new(0));
    let done = {
        let conns = conns.clone();
        let connection = connection.clone();
        move || {
            if halves.fetch_add(1, Ordering::SeqCst) == 1 {
                conns.close(&connection);
            }
        }
    };
    let done2 = done.clone();
    let (c, o) = (conns.clone(), out.clone());
    let writer_connection = connection.clone();
    std::thread::spawn(move || {
        let mut writer = writer;
        for op in rx {
            match op {
                Op::Data(d) => {
                    if writer.write_all(&d).is_err() {
                        let _ = c.send(&o, &writer_connection, CLOSE, &[]);
                        return;
                    }
                }
                Op::Eof => {
                    let _ = writer.shutdown(Shutdown::Write);
                    break;
                }
            }
        }
        done2();
    });
    let o = out.clone();
    let c = conns.clone();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = vec![0u8; CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = c.send(&o, &connection, EOF, &[]);
                    break;
                }
                Ok(n) => {
                    if c.send(&o, &connection, DATA, &buf[..n]).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    // Gone while the other end may still be sending: say so.
                    let _ = c.send(&o, &connection, CLOSE, &[]);
                    break;
                }
            }
        }
        done();
    });
}

/// The WSL end (`asagiri hostrelay`): listens as asagiri-win asks and sends each
/// connection over `out`, until asagiri-win goes, and the listeners with it.
pub fn listen_side(input: impl Read, out: &Out) -> io::Result<()> {
    let conns = Arc::new(Conns::default());
    let _close = CloseAll(conns.clone());
    let next = Arc::new(AtomicU32::new(1));
    let mut listening: HashMap<u16, u16> = HashMap::new();
    let mut r = BufReader::with_capacity(CHUNK, input);
    while let Some((kind, id, data)) = read_frame(&mut r)? {
        if kind != LISTEN {
            conns.dispatch(out, kind, id, data)?;
            continue;
        }
        let Some(target) = port_of(&data, 0) else { continue };
        let local = match listening.get(&target) {
            Some(&p) => p,
            None => {
                let p = listen(target, &conns, out, &next).unwrap_or(0);
                if p != 0 {
                    listening.insert(target, p);
                }
                p
            }
        };
        let mut reply = target.to_be_bytes().to_vec();
        reply.extend_from_slice(&local.to_be_bytes());
        send(out, LISTENING, 0, &reply)?;
    }
    Ok(())
}

/// Listens on the distro's loopback for `target`; its port.
fn listen(target: u16, conns: &Arc<Conns>, out: &Out, next: &Arc<AtomicU32>) -> io::Result<u16> {
    let l = TcpListener::bind(("127.0.0.1", 0))?;
    l.set_nonblocking(true)?;
    let port = l.local_addr()?.port();
    {
        let mut state = conns.state();
        if state.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        state.listeners.insert(port, l);
    }
    let (conns, out, next) = (conns.clone(), out.clone(), next.clone());
    std::thread::spawn(move || {
        loop {
            let accepted = {
                let state = conns.state();
                let Some(listener) = state.listeners.get(&port) else { return };
                listener.accept()
            };
            let stream = match accepted {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    conns.state().listeners.remove(&port);
                    return;
                }
            };
            let id = next.fetch_add(1, Ordering::SeqCst);
            let Some((connection, rx)) = conns.open(id) else { continue };
            if stream.set_nonblocking(false).is_err() {
                conns.close(&connection);
                continue;
            }
            attach(&conns, &out, connection, rx, stream, Some(target));
        }
    });
    Ok(port)
}

/// The Windows end: dials the proxy for each connection the WSL end sends,
/// on the ports `allowed` names, and hands LISTENING replies to `listening`.
pub fn dial_side(input: impl Read, out: &Out, allowed: impl Fn(u16) -> bool, listening: impl Fn(u16, u16)) -> io::Result<()> {
    let conns = Arc::new(Conns::default());
    let _close = CloseAll(conns.clone());
    let mut r = BufReader::with_capacity(CHUNK, input);
    while let Some((kind, id, data)) = read_frame(&mut r)? {
        match kind {
            OPEN => {
                let Some((connection, rx)) = conns.open(id) else { continue };
                let target = port_of(&data, 0).filter(|&p| allowed(p));
                let Some(target) = target else {
                    conns.send(out, &connection, CLOSE, &[])?;
                    continue;
                };
                // Data that arrives while it dials waits in the queue.
                let (c, o) = (conns.clone(), out.clone());
                std::thread::spawn(move || match dial(target) {
                    Ok(s) => attach(&c, &o, connection, rx, s, None),
                    Err(_) => {
                        let _ = c.send(&o, &connection, CLOSE, &[]);
                    }
                });
            }
            LISTENING => {
                if let (Some(target), Some(local)) = (port_of(&data, 0), port_of(&data, 2)) {
                    listening(target, local);
                }
            }
            _ => conns.dispatch(out, kind, id, data)?,
        }
    }
    Ok(())
}

/// This machine's loopback at `port`: IPv4, then IPv6.
fn dial(port: u16) -> io::Result<TcpStream> {
    let wait = Duration::from_secs(5);
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), wait).or_else(|e| TcpStream::connect_timeout(&(std::net::Ipv6Addr::LOCALHOST, port).into(), wait).map_err(|_| e))
}

/// Where in a proxy address its host and port are, when the host is this
/// machine's loopback: `http://user:pw@127.0.0.1:7892/` → (7892, the span of
/// `127.0.0.1:7892`). Without a port there is nothing to relay.
pub fn loopback(url: &str) -> Option<(u16, std::ops::Range<usize>)> {
    let start = url.find("://").map_or(0, |i| i + 3);
    let rest = &url[start..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let at = rest[..end].rfind('@').map_or(0, |i| i + 1);
    let hostport = &rest[at..end];
    let (host, port) = hostport.rsplit_once(':')?;
    let port: u16 = port.parse().ok().filter(|&p| p != 0)?;
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    let is_loopback = host == "localhost" || host == "::1" || host.parse::<std::net::Ipv4Addr>().is_ok_and(|a| a.is_loopback());
    is_loopback.then_some((port, start + at..start + end))
}

/// The distro's own address for a Windows loopback proxy, through the relay.
pub fn relayed(url: &str, map: &HashMap<u16, u16>) -> Option<String> {
    let (port, span) = loopback(url)?;
    let local = map.get(&port)?;
    Some(format!("{}127.0.0.1:{local}{}", &url[..span.start], &url[span.end..]))
}

pub fn run() -> io::Result<()> {
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    listen_side(io::stdin(), &out)
}

// Over unix socket pairs: these run with asagiri's tests, not asagiri-win's.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn proxy_addresses_on_loopback() {
        assert_eq!(loopback("127.0.0.1:7892").map(|l| l.0), Some(7892));
        let url = "http://user:p@ss@localhost:8080/";
        let (port, span) = loopback(url).unwrap();
        assert_eq!((port, &url[span]), (8080, "localhost:8080"));
        assert_eq!(loopback("http://[::1]:3128").map(|l| l.0), Some(3128));
        assert_eq!(loopback("socks5://127.0.0.2:1080").map(|l| l.0), Some(1080));
        for none in ["http://10.0.0.1:3128", "http://proxy.corp:8080", "http://127.0.0.1", "http://127.0.0.1:0", "", "http://127.0.0.1:x"] {
            assert_eq!(loopback(none), None, "{none}");
        }
        let map = HashMap::from([(7892, 41234)]);
        assert_eq!(relayed("http://u:p@127.0.0.1:7892/x", &map).as_deref(), Some("http://u:p@127.0.0.1:41234/x"));
        assert_eq!(relayed("127.0.0.1:7892", &map).as_deref(), Some("127.0.0.1:41234"));
        assert_eq!(relayed("http://127.0.0.1:9999", &map), None);
        assert_eq!(relayed("http://proxy.corp:7892", &map), None);
    }

    /// Both ends over a pair of sockets, as wsl.exe's stdin and stdout carry
    /// them: a "proxy" on this machine's loopback, reached through the relay.
    fn relay(allowed: u16) -> (Arc<Mutex<HashMap<u16, u16>>>, Out) {
        let (wsl, win) = UnixStream::pair().unwrap();
        let wsl_out: Out = Arc::new(Mutex::new(wsl.try_clone().unwrap()));
        let win_out: Out = Arc::new(Mutex::new(win.try_clone().unwrap()));
        std::thread::spawn(move || listen_side(wsl, &wsl_out));
        let ports = Arc::new(Mutex::new(HashMap::new()));
        let p = ports.clone();
        let o = win_out.clone();
        std::thread::spawn(move || dial_side(win, &o, move |t| t == allowed, move |t, l| { p.lock().unwrap().insert(t, l); }));
        (ports, win_out)
    }

    fn listen_for(ports: &Arc<Mutex<HashMap<u16, u16>>>, out: &Out, target: u16) -> u16 {
        send(out, LISTEN, 0, &target.to_be_bytes()).unwrap();
        for _ in 0..200 {
            if let Some(&l) = ports.lock().unwrap().get(&target) {
                return l;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no LISTENING for {target}");
    }

    #[test]
    fn connections_reach_the_proxy_and_back() {
        // The proxy: echoes each connection, upper-cased, until its half-close.
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = proxy.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in proxy.incoming() {
                let mut s = s.unwrap();
                std::thread::spawn(move || {
                    let mut all = Vec::new();
                    s.read_to_end(&mut all).unwrap();
                    s.write_all(&all.to_ascii_uppercase()).unwrap();
                });
            }
        });
        let (ports, out) = relay(target);
        let local = listen_for(&ports, &out, target);
        assert_ne!(local, 0);
        assert_eq!(listen_for(&ports, &out, target), local, "asking again keeps the port");
        // Several at once, one bigger than many frames and the queue.
        let big: Vec<u8> = (0..3_000_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let handles: Vec<_> = (0..6)
            .map(|i| {
                let body = if i == 0 { big.clone() } else { format!("hello {i}").into_bytes() };
                std::thread::spawn(move || {
                    let mut c = TcpStream::connect(("127.0.0.1", local)).unwrap();
                    let mut w = c.try_clone().unwrap();
                    let b = body.clone();
                    let t = std::thread::spawn(move || {
                        w.write_all(&b).unwrap();
                        w.shutdown(Shutdown::Write).unwrap();
                    });
                    let mut got = Vec::new();
                    c.read_to_end(&mut got).unwrap();
                    t.join().unwrap();
                    assert_eq!(got, body.to_ascii_uppercase());
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn only_the_ports_asked_for_are_dialled() {
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = other.local_addr().unwrap().port();
        // asagiri-win allows some other port; the WSL end listens for this one anyway.
        let (ports, out) = relay(1);
        let local = listen_for(&ports, &out, target);
        let mut c = TcpStream::connect(("127.0.0.1", local)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 1];
        assert!(matches!(c.read(&mut buf), Ok(0) | Err(_)), "closed without reaching the port");
        other.set_nonblocking(true).unwrap();
        assert!(other.accept().is_err(), "nothing dialled the port");
    }

    #[test]
    fn a_proxy_that_is_not_running_closes_the_connection() {
        let gone = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = gone.local_addr().unwrap().port();
        drop(gone);
        let (ports, out) = relay(target);
        let local = listen_for(&ports, &out, target);
        let mut c = TcpStream::connect(("127.0.0.1", local)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut buf = [0u8; 1];
        assert!(matches!(c.read(&mut buf), Ok(0) | Err(_)));
    }
}

#[cfg(test)]
mod teardown_tests {
    use super::*;
    use std::sync::mpsc::TryRecvError;

    #[test]
    fn duplicate_and_stale_owners_cannot_close_replacements() {
        let conns = Conns::default();
        let (mut input, output) = io::pipe().unwrap();
        let out: Out = Arc::new(Mutex::new(output));
        let (original, rx) = conns.open(7).unwrap();
        assert!(conns.open(7).is_none());
        conns.dispatch(&out, CLOSE, 7, vec![]).unwrap();
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Disconnected)));
        let (replacement, rx) = conns.open(7).unwrap();
        assert!(!conns.close(&original));
        conns.send(&out, &original, CLOSE, &[]).unwrap();
        conns.dispatch(&out, DATA, 7, vec![1]).unwrap();
        assert!(matches!(rx.try_recv(), Ok(Op::Data(data)) if data == [1]));
        assert!(conns.close(&replacement));
        let (_, rx) = conns.open(7).unwrap();
        conns.close_all();
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Disconnected)));
        assert!(conns.open(8).is_none());
        drop(out);
        assert_eq!(read_frame(&mut input).unwrap(), None);
    }

    #[test]
    fn full_queue_releases_only_its_connection() {
        let conns = Conns::default();
        let (mut input, output) = io::pipe().unwrap();
        let out: Out = Arc::new(Mutex::new(output));
        let (_blocked, rx) = conns.open(1).unwrap();
        let (other, _) = conns.open(2).unwrap();
        for _ in 0..=QUEUE {
            conns.dispatch(&out, DATA, 1, vec![1]).unwrap();
        }
        assert_eq!(read_frame(&mut input).unwrap(), Some((CLOSE, 1, vec![])));
        assert_eq!(rx.into_iter().count(), QUEUE);
        assert!(conns.state().owns(&other));
    }

    #[test]
    fn distinguishes_clean_eof_and_truncated_frames() {
        assert_eq!(read_frame(&mut &[][..]).unwrap(), None);
        for truncated in [&[DATA][..], &[DATA, 0, 0, 0, 1, 0, 0, 0, 1][..]] {
            assert_eq!(read_frame(&mut &truncated[..]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        }
    }

    #[cfg(unix)]
    #[test]
    fn quiet_proxy_is_closed_and_workers_release_output() {
        use std::os::unix::net::UnixStream;
        use std::time::Instant;

        for tail in [&[][..], &[DATA][..]] {
            let proxy = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = proxy.local_addr().unwrap();
            let (input, mut commands) = UnixStream::pair().unwrap();
            let (output, mut replies) = UnixStream::pair().unwrap();
            replies.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let out: Out = Arc::new(Mutex::new(output));
            let output_lifetime = Arc::downgrade(&out);
            let (finished, result) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                finished.send(dial_side(input, &out, |p| p == address.port(), |_, _| {})).unwrap();
            });
            write_frame(&mut commands, OPEN, 1, &address.port().to_be_bytes()).unwrap();
            proxy.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut socket = loop {
                match proxy.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("proxy accept: {e}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            socket.write_all(b"ready").unwrap();
            assert_eq!(read_frame(&mut replies).unwrap(), Some((DATA, 1, b"ready".to_vec())));
            commands.write_all(tail).unwrap();
            commands.shutdown(Shutdown::Write).unwrap();
            let relay_result = result.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(relay_result.err().map(|e| e.kind()), (!tail.is_empty()).then_some(io::ErrorKind::UnexpectedEof));
            let closed = socket.read(&mut [0]);
            assert!(matches!(closed, Ok(0)) || matches!(closed, Err(ref e) if e.kind() == io::ErrorKind::ConnectionReset), "{closed:?}");
            let deadline = Instant::now() + Duration::from_secs(2);
            while output_lifetime.strong_count() > 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(output_lifetime.strong_count(), 0);
        }
    }

    #[test]
    fn listener_port_is_released_on_eof_and_read_error() {
        TcpListener::bind(("127.0.0.1", 0)).expect("loopback bind required");
        for tail in [&[][..], &[DATA][..]] {
            let (mut replies, output) = io::pipe().unwrap();
            let out: Out = Arc::new(Mutex::new(output));
            let mut frames = Vec::new();
            write_frame(&mut frames, LISTEN, 0, &7892u16.to_be_bytes()).unwrap();
            frames.extend_from_slice(tail);
            let result = listen_side(frames.as_slice(), &out);
            assert_eq!(result.err().map(|err| err.kind()), (!tail.is_empty()).then_some(io::ErrorKind::UnexpectedEof));
            let (_, _, data) = read_frame(&mut replies).unwrap().unwrap();
            let port = port_of(&data, 2).unwrap();
            assert_ne!(port, 0);
            assert!(TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(500)).is_err());
        }
    }
}
