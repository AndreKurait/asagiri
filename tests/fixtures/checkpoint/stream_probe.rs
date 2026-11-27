//! The client opens one TCP connection to the server at startup. Each host request
//! byte advances both open volume files and returns both in-memory identities.
//! A `?` queries the socket endpoints and resolves the backend name again.
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};

struct State {
    nonce: String,
    file: File,
    count: usize,
}

impl State {
    fn new() -> io::Result<Self> {
        Ok(Self {
            nonce: fs::read_to_string("/proc/sys/kernel/random/uuid")?
                .trim()
                .into(),
            file: OpenOptions::new()
                .create_new(true)
                .write(true)
                .open("/data/stream")?,
            count: 0,
        })
    }

    fn record(&mut self, byte: u8) -> io::Result<()> {
        self.file.write_all(&[byte])?;
        self.file.sync_data()?;
        self.count += 1;
        println!("stdout {} {byte}", self.nonce);
        eprintln!("stderr {} {byte}", self.nonce);
        Ok(())
    }
}

fn serve(mut state: State) -> io::Result<()> {
    let (mut stream, _) = TcpListener::bind("0.0.0.0:17061")?.accept()?;
    let mut request = [0];
    while stream.read(&mut request)? == 1 {
        state.record(request[0])?;
        writeln!(stream, "{} {}", state.nonce, state.count)?;
    }
    Ok(())
}

struct Proxy {
    state: State,
    upstream: BufReader<TcpStream>,
    backend: String,
}

impl Proxy {
    fn reply(&mut self, byte: u8, client: &mut TcpStream) -> io::Result<()> {
        if byte == b'P' {
            unsafe extern "C" { fn ioctl(fd: i32, request: usize, ...) -> i32; }
            let mut available = 0i32;
            // Linux FIONREAD on the fixture's own input pipe, without consuming the byte.
            if unsafe { ioctl(0, 0x541b, &mut available) } != 0 { return Err(io::Error::last_os_error()); }
            return writeln!(client, "pending {available}");
        }
        if byte == b'I' {
            let mut value = [0];
            io::stdin().read_exact(&mut value)?;
            return writeln!(client, "stdin {} {}", self.state.nonce, value[0]);
        }
        if byte == b'?' {
            let address = (self.backend.as_str(), 17061)
                .to_socket_addrs()?
                .find(|addr| addr.is_ipv4())
                .ok_or(io::ErrorKind::AddrNotAvailable)?;
            let stream = self.upstream.get_ref();
            return writeln!(
                client,
                "{} {} {}",
                stream.local_addr()?,
                stream.peer_addr()?,
                address
            );
        }
        self.upstream.get_mut().write_all(&[byte])?;
        let mut reply = String::new();
        if self.upstream.read_line(&mut reply)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.state.record(byte)?;
        write!(
            client,
            "{} {} {}",
            self.state.nonce, self.state.count, reply
        )
    }
}

fn handle_client(mut client: TcpStream, proxy: &Mutex<Proxy>) -> io::Result<()> {
    let mut request = [0];
    while client.read(&mut request)? == 1 {
        proxy.lock().unwrap().reply(request[0], &mut client)?;
    }
    Ok(())
}

fn proxy(state: State, backend: &str) -> io::Result<()> {
    let upstream = BufReader::new(TcpStream::connect((backend, 17061))?);
    let shared = Arc::new(Mutex::new(Proxy {
        state,
        upstream,
        backend: backend.into(),
    }));
    for incoming in TcpListener::bind("0.0.0.0:17060")?.incoming() {
        let client = incoming?;
        let shared = shared.clone();
        std::thread::spawn(move || {
            if let Err(error) = handle_client(client, &shared) {
                eprintln!("connection ended: {error}");
            }
        });
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let state = State::new()?;
    match std::env::args().nth(1).as_deref() {
        Some("server") => serve(state),
        Some(backend) => proxy(state, backend),
        None => Err(io::Error::other("expected server or the server's DNS name")),
    }
}
