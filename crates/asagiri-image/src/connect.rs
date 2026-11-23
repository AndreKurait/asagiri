//! Opens TCP connections to registries the way browsers and curl do, Happy
//! Eyeballs (RFC 8305): the resolved addresses are tried with the families
//! taking turns, a new attempt starts every 250 ms or as soon as one fails,
//! and the first to connect wins.
//!
//! ureq on its own tries the addresses one after another and splits the
//! connect timeout between them, half to the first. A host or VM with an
//! IPv6 address but no working IPv6 route (WSL2 and many home networks)
//! then spends most of the 15 s on `registry-1.docker.io`'s AAAA records,
//! which drop the SYN, before an A record gets the little time left, and
//! a pull stalls or fails although IPv4 works.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Weak, mpsc};
use std::time::{Duration, Instant};
use std::{fmt, thread};

use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, RustlsConnector, ShutdownHandle,
    Transport,
};
use ureq::{Error, Timeout};

/// How long to wait for an attempt before starting the next, RFC 8305's
/// recommended Connection Attempt Delay.
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// How long one address gets to answer. Attempts overlap, so this caps a
/// single silent address, not the whole connect.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest one read or write may wait with nothing moving, whatever
/// ureq allows for the call as a whole (by default no limit at all). A
/// connection that goes silent mid-body (a stalled CDN, a NAT that dropped
/// it) then fails, and the pull resumes the blob on a new one, rather than
/// waiting forever with the builder's lock held.
pub(crate) const STALL: Duration = Duration::from_secs(60);

/// A registry transport with a short stall timeout for the network tests.
#[cfg(test)]
pub(crate) fn connector_with(stall: Duration) -> impl Connector<(), Out = impl Transport> {
    ().chain(ConnectProxyConnector::default()).chain(HappyConnector { stall }).chain(RustlsConnector::default())
}

/// Certificate exceptions are selected for each actual TLS destination,
/// including redirects and token servers. They are never an agent-wide
/// disable switch. The caller creates a new pool when this policy changes.
pub(crate) fn registry_connector(settings: crate::RegistrySettings) -> impl Connector<(), Out = impl Transport> {
    let trusted = settings.ca_certificates().iter().map(|(host, pem)| {
        let roots = crate::settings::ca_roots(pem).expect("admitted CA bundle");
        let config = ureq::Agent::config_builder().tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).build()).build();
        (host.clone(), (RustlsConnector::default(), config))
    }).collect();
    ().chain(ConnectProxyConnector::default()).chain(HappyConnector { stall: STALL }).chain(RegistryTls {
        settings,
        trusted,
        secure: RustlsConnector::default(),
        insecure: RustlsConnector::default(),
        insecure_config: ureq::Agent::config_builder()
            .tls_config(ureq::tls::TlsConfig::builder().disable_verification(true).build()).build(),
    })
}

#[derive(Debug)]
struct RegistryTls {
    settings: crate::RegistrySettings,
    trusted: std::collections::BTreeMap<String, (RustlsConnector, ureq::config::Config)>,
    secure: RustlsConnector,
    insecure: RustlsConnector,
    insecure_config: ureq::config::Config,
}

impl<In: Transport> Connector<In> for RegistryTls {
    type Out = <RustlsConnector as Connector<In>>::Out;

    fn connect(&self, details: &ConnectionDetails, chained: Option<In>) -> Result<Option<Self::Out>, Error> {
        if let Some((connector, config)) = details.uri.authority()
            .and_then(|a| crate::settings::https_authority(a.as_str()).ok())
            .and_then(|host| self.trusted.get(&host)) {
            let scoped = ConnectionDetails {
                uri: details.uri, addrs: details.addrs.clone(), config, request_level: false,
                resolver: details.resolver, now: details.now, timeout: details.timeout,
                current_time: details.current_time.clone(), run_connector: details.run_connector.clone(),
                register_shutdown: details.register_shutdown.clone(),
            };
            return connector.connect(&scoped, chained);
        }
        let allowed = details.uri.authority().is_some_and(|a| self.settings.allows_insecure(a.as_str()));
        if !allowed {
            return self.secure.connect(details, chained);
        }
        // A separate RustlsConnector has its own cached TLS config. Setting
        // request_level=false cannot change the secure connector's cache.
        let scoped = ConnectionDetails {
            uri: details.uri, addrs: details.addrs.clone(), config: &self.insecure_config, request_level: false,
            resolver: details.resolver, now: details.now, timeout: details.timeout,
            current_time: details.current_time.clone(), run_connector: details.run_connector.clone(),
            register_shutdown: details.register_shutdown.clone(),
        };
        self.insecure.connect(&scoped, chained)
    }
}

/// The addresses in the order to try them: the family the resolver put
/// first leads (it sorts by RFC 6724, so that is IPv6 where IPv6 looks
/// usable), then the families take turns, each keeping its own order.
pub(crate) fn interleave(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    let Some(first) = addrs.first() else {
        return Vec::new();
    };
    let (mut lead, mut other): (Vec<SocketAddr>, Vec<SocketAddr>) = addrs.iter().partition(|a| a.is_ipv6() == first.is_ipv6());
    lead.reverse();
    other.reverse();
    let mut out = Vec::with_capacity(addrs.len());
    while !lead.is_empty() || !other.is_empty() {
        out.extend(lead.pop());
        out.extend(other.pop());
    }
    out
}

/// One connect attempt: the address, and how long it may take.
type Attempt<T> = dyn Fn(SocketAddr, Duration) -> io::Result<T> + Send + Sync;

/// Races `attempt` over `addrs` in [`interleave`] order: a new attempt
/// starts `delay` after the last one started, or straight away when one
/// fails, and the first to succeed is returned. Attempts still running
/// carry on in their threads and close what they open; each is bounded by
/// its own timeout. `budget` bounds the whole race, if there is one.
pub(crate) fn race<T: Send + 'static>(addrs: &[SocketAddr], delay: Duration, budget: Option<Duration>, attempt: Arc<Attempt<T>>) -> io::Result<T> {
    let order = interleave(addrs);
    if order.is_empty() {
        return Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "no addresses to connect to"));
    }
    let start = Instant::now();
    let end = budget.map(|b| start + b);
    let (tx, rx) = mpsc::channel::<io::Result<T>>();
    let mut next = 0;
    let mut running = 0;
    let mut next_start = start;
    let mut last_err = None;
    loop {
        let now = Instant::now();
        if end.is_some_and(|e| now >= e) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out"));
        }
        if next < order.len() && (running == 0 || now >= next_start) {
            let addr = order[next];
            let limit = end.map_or(ATTEMPT_TIMEOUT, |e| (e - now).min(ATTEMPT_TIMEOUT));
            let (tx, attempt) = (tx.clone(), attempt.clone());
            let spawned = thread::Builder::new().name("asagiri-connect".into()).spawn(move || {
                // Nobody listens once another attempt has won; what this
                // one opened is then dropped, and so closed.
                let _ = tx.send(attempt(addr, limit));
            });
            next += 1;
            next_start = now + delay;
            match spawned {
                Ok(_) => running += 1,
                Err(e) => last_err = Some(e),
            }
            continue;
        }
        if running == 0 {
            return Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused")));
        }
        // Wake for the next attempt's turn, or the end of the budget.
        let wake = if next < order.len() { Some(next_start) } else { end };
        let got = match wake {
            Some(w) => rx.recv_timeout(w.saturating_duration_since(now)),
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match got {
            Ok(Ok(won)) => return Ok(won),
            Ok(Err(e)) => {
                running -= 1;
                last_err = Some(e);
                // A failure hands its turn to the next address now.
                next_start = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // We hold a sender, so the channel cannot close.
            Err(mpsc::RecvTimeoutError::Disconnected) => unreachable!(),
        }
    }
}

fn connect_one(addr: SocketAddr, limit: Duration, no_delay: bool) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, limit)?;
    if no_delay {
        stream.set_nodelay(true)?;
    }
    Ok(stream)
}

/// The TCP step of [`connector`].
#[derive(Debug)]
struct HappyConnector {
    stall: Duration,
}

impl<In: Transport> Connector<In> for HappyConnector {
    type Out = Either<In, TcpTransport>;

    fn connect(&self, details: &ConnectionDetails, chained: Option<In>) -> Result<Option<Self::Out>, Error> {
        // A proxy connector already made the connection.
        if chained.is_some() {
            return Ok(chained.map(Either::A));
        }
        let config = details.config;
        let no_delay = config.no_delay();
        let budget = details.timeout.not_zero().map(|t| *t);
        let stream = race(&details.addrs, ATTEMPT_DELAY, budget, Arc::new(move |addr, limit| connect_one(addr, limit, no_delay)))
            .map_err(|e| if e.kind() == io::ErrorKind::TimedOut { Error::Timeout(Timeout::Connect) } else { Error::Io(e) })?;
        let transport = TcpTransport {
            stream: Arc::new(stream),
            buffers: LazyBuffers::new(config.input_buffer_size(), config.output_buffer_size()),
            timeout_read: None,
            timeout_write: None,
            stall: self.stall,
        };
        (details.register_shutdown)(Arc::downgrade(&transport.stream) as Weak<dyn ShutdownHandle>)?;
        Ok(Some(Either::B(transport)))
    }
}

/// A plain TCP connection, as ureq's own `TcpTransport` (which it does not
/// export) handles one.
struct TcpTransport {
    /// Shared with the agent's shutdown handle, which holds it weakly.
    stream: Arc<TcpStream>,
    buffers: LazyBuffers,
    timeout_read: Option<Duration>,
    timeout_write: Option<Duration>,
    /// See [`STALL`].
    stall: Duration,
}

impl fmt::Debug for TcpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpTransport").field("addr", &self.stream.peer_addr().ok()).finish()
    }
}

/// The timeout left, or the error to give when it has run out.
fn remaining(timeout: NextTimeout) -> Result<Option<Duration>, Error> {
    if !timeout.after.is_not_happening() && timeout.after.is_zero() {
        return Err(Error::Timeout(timeout.reason));
    }
    Ok(timeout.not_zero().map(|t| *t))
}

/// `limit`, never longer than `stall`.
fn capped(limit: Option<Duration>, stall: Duration) -> Option<Duration> {
    Some(limit.map_or(stall, |l| l.min(stall)))
}

/// Socket I/O errors as ureq reports them: a timeout (which a socket
/// timeout shows as `WouldBlock` on some systems) names its reason.
fn io_error(e: io::Error, reason: Timeout) -> Error {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Error::Timeout(reason),
        _ => Error::Io(e),
    }
}

/// Windows cancellable pulls cannot depend on shutdown waking a blocking
/// recv. Each poll spends the same full read budget, including interrupted
/// reads. Restore blocking mode before returning so the pooled connection's
/// next write or uncancellable request retains its normal socket semantics.
#[cfg(any(windows, test))]
fn cancellable_read(stream: &Arc<TcpStream>, input: &mut [u8], limit: Duration) -> io::Result<usize> {
    crate::cancel::watch(stream)?;
    stream.set_nonblocking(true)?;
    let started = Instant::now();
    let result = loop {
        if crate::cancel::cancelled() {
            break Err(io::Error::other("pull cancelled"));
        }
        let left = limit.saturating_sub(started.elapsed());
        if left.is_zero() {
            break Err(io::ErrorKind::TimedOut.into());
        }
        match (&**stream).read(input) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(crate::cancel::POLL.min(limit.saturating_sub(started.elapsed())));
            }
            result => break if crate::cancel::cancelled() { Err(io::Error::other("pull cancelled")) } else { result },
        }
    };
    // Attempt restoration on failures too, preserving the original read or
    // cancellation error if both operations fail.
    let restored = stream.set_nonblocking(false);
    result.and_then(|amount| restored.map(|()| amount))
}

impl Transport for TcpTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        let timeout = timeout.for_write();
        let limit = capped(remaining(timeout)?, self.stall);
        if limit != self.timeout_write {
            self.stream.set_write_timeout(limit)?;
            self.timeout_write = limit;
        }
        crate::cancel::watch(&self.stream)?;
        let output = &self.buffers.output()[..amount];
        (&*self.stream).write_all(output).map_err(|e| io_error(e, timeout.reason))
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        let timeout = timeout.for_read();
        let limit = capped(remaining(timeout)?, self.stall);
        if limit != self.timeout_read {
            self.stream.set_read_timeout(limit)?;
            self.timeout_read = limit;
        }
        // A cancelled pull's reads end here, or at once if already waiting.
        crate::cancel::watch(&self.stream)?;
        let started = Instant::now();
        let input = self.buffers.input_append_buf();
        #[cfg(windows)]
        if crate::cancel::active() {
            let amount = cancellable_read(&self.stream, input, limit.unwrap_or(self.stall)).map_err(|e| io_error(e, timeout.reason))?;
            self.buffers.input_appended(amount);
            return Ok(amount > 0);
        }
        let amount = loop {
            match (&*self.stream).read(input) {
                // A signal: Linux does not restart a socket read that has a
                // timeout, so go again with what is left of it.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    let Some(limit) = limit else {
                        return Err(Error::Io(e));
                    };
                    let left = limit.saturating_sub(started.elapsed());
                    if left.is_zero() {
                        return Err(Error::Timeout(timeout.reason));
                    }
                    self.stream.set_read_timeout(Some(left))?;
                    self.timeout_read = Some(left);
                }
                result => break result.map_err(|e| io_error(e, timeout.reason))?,
            }
        };
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        // Nothing should be waiting on an idle connection: a byte means the
        // server sent something unasked, an error or EOF that it closed.
        let stream = &*self.stream;
        if stream.set_nonblocking(true).is_err() {
            return false;
        }
        let open = matches!((&*self.stream).read(&mut [0]), Err(e) if e.kind() == io::ErrorKind::WouldBlock);
        open && stream.set_nonblocking(false).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn families_take_turns_led_by_the_first() {
        let got = interleave(&[addr("[2001:db8::1]:443"), addr("[2001:db8::2]:443"), addr("[2001:db8::3]:443"), addr("192.0.2.1:443"), addr("192.0.2.2:443")]);
        let want = [addr("[2001:db8::1]:443"), addr("192.0.2.1:443"), addr("[2001:db8::2]:443"), addr("192.0.2.2:443"), addr("[2001:db8::3]:443")];
        assert_eq!(got, want);
        // The resolver put IPv4 first: it leads.
        let got = interleave(&[addr("192.0.2.1:443"), addr("[2001:db8::1]:443"), addr("[2001:db8::2]:443")]);
        assert_eq!(got, [addr("192.0.2.1:443"), addr("[2001:db8::1]:443"), addr("[2001:db8::2]:443")]);
        assert_eq!(interleave(&[addr("192.0.2.1:443")]), [addr("192.0.2.1:443")]);
        assert!(interleave(&[]).is_empty());
    }

    /// A fake connect: IPv6 per `v6`, IPv4 connects after 20 ms.
    fn fake(v6: fn(Duration) -> io::Result<SocketAddr>) -> Arc<Attempt<SocketAddr>> {
        Arc::new(move |a: SocketAddr, limit| {
            if a.is_ipv6() {
                v6(limit).map(|_| a)
            } else {
                thread::sleep(Duration::from_millis(20));
                Ok(a)
            }
        })
    }

    #[test]
    fn an_unreachable_ipv6_hands_over_to_ipv4_at_once() {
        let addrs = [addr("[2001:db8::1]:443"), addr("[2001:db8::2]:443"), addr("192.0.2.1:443")];
        // The stagger exceeds the entire budget. IPv4 can win only if the
        // IPv6 failure releases the next attempt before its scheduled turn.
        let won = race(&addrs, Duration::from_secs(30), Some(Duration::from_secs(5)), fake(|_| Err(io::ErrorKind::NetworkUnreachable.into()))).unwrap();
        assert_eq!(won, addr("192.0.2.1:443"));
    }

    #[test]
    fn a_silent_ipv6_gives_ipv4_its_turn_after_the_delay() {
        let addrs = [addr("[2001:db8::1]:443"), addr("192.0.2.1:443")];
        let start = Instant::now();
        let won = race(
            &addrs,
            ATTEMPT_DELAY,
            Some(Duration::from_secs(15)),
            fake(|limit| {
                // A dropped SYN: nothing until the attempt's timeout.
                thread::sleep(limit.min(Duration::from_secs(3)));
                Err(io::ErrorKind::TimedOut.into())
            }),
        )
        .unwrap();
        assert_eq!(won, addr("192.0.2.1:443"));
        let took = start.elapsed();
        assert!(took >= ATTEMPT_DELAY && took < Duration::from_secs(1), "{took:?}");
    }

    #[test]
    fn a_working_ipv6_wins_before_ipv4_starts() {
        let addrs = [addr("[2001:db8::1]:443"), addr("192.0.2.1:443")];
        let tried = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = tried.clone();
        let won = race(
            &addrs,
            ATTEMPT_DELAY,
            None,
            Arc::new(move |a: SocketAddr, _| {
                seen.lock().unwrap().push(a);
                Ok(a)
            }),
        )
        .unwrap();
        assert_eq!(won, addr("[2001:db8::1]:443"));
        assert_eq!(*tried.lock().unwrap(), [won]);
    }

    #[test]
    fn every_address_failing_reports_the_last_error() {
        let addrs = [addr("[2001:db8::1]:443"), addr("192.0.2.1:443")];
        let err = race(&addrs, ATTEMPT_DELAY, None, Arc::new(|_, _| Err::<(), _>(io::ErrorKind::ConnectionRefused.into()))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn the_budget_bounds_the_whole_race() {
        let addrs = [addr("[2001:db8::1]:443"), addr("192.0.2.1:443")];
        let start = Instant::now();
        let err = race(
            &addrs,
            Duration::from_millis(50),
            Some(Duration::from_millis(300)),
            Arc::new(|_, limit| {
                thread::sleep(limit);
                Err::<(), _>(io::ErrorKind::TimedOut.into())
            }),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
    }

    #[test]
    fn connects_past_a_refused_address_for_real() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap();
        // A port just freed: nothing listens there, so it is refused (or,
        // without IPv6 on loopback, unavailable); either way, move on.
        let closed = TcpListener::bind("[::1]:0").ok().map(|l| l.local_addr().unwrap()).unwrap_or(addr("[::1]:9"));
        let stream = race(&[closed, open], ATTEMPT_DELAY, Some(Duration::from_secs(5)), Arc::new(|a, limit| connect_one(a, limit, true))).unwrap();
        assert_eq!(stream.peer_addr().unwrap(), open);
    }

    #[test]
    fn cancellable_socket_read_ends_before_the_stall_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = Arc::new(TcpStream::connect(listener.local_addr().unwrap()).unwrap());
        let (_server, _) = listener.accept().unwrap();
        let cancel = crate::cancel::Cancel::new();
        let (ready, waiting) = mpsc::channel();
        let pull = cancel.clone();
        let reader = thread::spawn(move || {
            let _on = pull.enter();
            let started = Instant::now();
            ready.send(()).unwrap();
            let result = cancellable_read(&client, &mut [0; 16], Duration::from_secs(5));
            (result, started.elapsed())
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        thread::sleep(Duration::from_millis(100));
        cancel.cancel();
        let (result, elapsed) = reader.join().unwrap();
        assert_eq!(result.unwrap_err().to_string(), "pull cancelled");
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    }

    #[test]
    fn polling_keeps_the_read_budget_and_restores_blocking_after_data() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = Arc::new(TcpStream::connect(listener.local_addr().unwrap()).unwrap());
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let (next, resume) = mpsc::channel();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            server.write_all(b"a").unwrap();
            resume.recv_timeout(Duration::from_secs(2)).unwrap();
            thread::sleep(Duration::from_millis(300));
            server.write_all(b"b").unwrap();
        });
        let cancel = crate::cancel::Cancel::new();
        let _on = cancel.enter();
        let started = Instant::now();
        let mut first = [0; 1];
        let amount = cancellable_read(&client, &mut first, Duration::from_secs(2)).unwrap();
        assert_eq!((amount, first), (1, *b"a"));
        // A leaked nonblocking mode would return WouldBlock before b arrives.
        next.send(()).unwrap();
        let mut second = [0; 1];
        (&*client).read_exact(&mut second).unwrap();
        assert_eq!(&second, b"b");
        assert!(started.elapsed() >= Duration::from_millis(550), "{:?}", started.elapsed());
        writer.join().unwrap();
    }

    #[test]
    fn polling_timeout_uses_the_whole_budget_and_restores_blocking() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = Arc::new(TcpStream::connect(listener.local_addr().unwrap()).unwrap());
        let (_server, _) = listener.accept().unwrap();
        let cancel = crate::cancel::Cancel::new();
        let _on = cancel.enter();
        let started = Instant::now();
        let result = cancellable_read(&client, &mut [0; 1], Duration::from_millis(250));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= Duration::from_millis(250), "{:?}", started.elapsed());
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        client.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let started = Instant::now();
        let error = (&*client).read(&mut [0; 1]).unwrap_err();
        assert!(matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut), "{error}");
        assert!(started.elapsed() >= Duration::from_millis(150), "socket stayed nonblocking");
    }
}

#[cfg(test)]
mod registry_ca_tests {
    use super::*;
    use std::net::TcpListener;
    use rcgen::{CertificateParams, CertifiedIssuer, KeyPair};

    fn ca() -> CertifiedIssuer<'static, KeyPair> {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::CrlSign];
        CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
    }

    fn server(ca: &CertifiedIssuer<'static, KeyPair>, expired: bool) -> (u16, thread::JoinHandle<()>) {
        // Keys exist only in this test's memory, never in source or logs.
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["127.0.0.1".into()]).unwrap();
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        if expired {
            params.not_before = rcgen::date_time_ymd(2000, 1, 1);
            params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        }
        let cert = params.signed_by(&key, ca).unwrap();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions().unwrap().with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                    Err(_) => return,
                }
            };
            // BSD accepts inherit the listener's nonblocking mode.
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
            let conn = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, stream);
            let mut request = [0; 4096];
            if tls.read(&mut request).is_ok() { let _ = tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"); }
        });
        (port, handle)
    }

    fn agent(policy: crate::RegistrySettings) -> ureq::Agent {
        ureq::Agent::with_parts(ureq::Agent::config_builder().proxy(None).timeout_global(Some(Duration::from_secs(3))).build(),
            registry_connector(policy), ureq::unversioned::resolver::DefaultResolver::default())
    }

    fn certificate_error(error: &Error) -> Option<&rustls::CertificateError> {
        let error = match error {
            Error::Rustls(error) => error,
            Error::Io(error) => error.get_ref()?.downcast_ref::<rustls::Error>()?,
            _ => return None,
        };
        match error {
            rustls::Error::InvalidCertificate(error) => Some(error),
            _ => None,
        }
    }

    #[test]
    fn scoped_ca_requires_correct_chain_name_authority_and_expiry() {
        let ca = ca();
        let other = self::ca().pem();
        for case in ["trusted", "default", "wrong-ca", "wrong-name", "wrong-port", "expired"] {
            let (port, worker) = server(&ca, case == "expired");
            let hostname = if case == "wrong-name" { "localhost" } else { "127.0.0.1" };
            let selected_port = if case == "wrong-port" { if port == 65535 { 65534 } else { port + 1 } } else { port };
            let mut bundles = std::collections::BTreeMap::new();
            if case != "default" { bundles.insert(format!("{hostname}:{selected_port}"), if case == "wrong-ca" { other.clone() } else { ca.pem() }); }
            let policy = crate::RegistrySettings::default().with_ca_certificates(&bundles).unwrap();
            let result = agent(policy).get(format!("https://{hostname}:{port}/v2/")).call();
            worker.join().unwrap();
            if case == "trusted" {
                assert!(result.is_ok(), "{case}: {result:?}");
            } else {
                let error = result.expect_err(case);
                assert!(certificate_error(&error).is_some(), "{case} must reject the certificate, not fail unrelated I/O: {error:?}");
            }
        }
    }

    #[test]
    fn trusted_connection_does_not_trust_another_destination_in_same_pool() {
        let ca = ca();
        let (first, first_worker) = server(&ca, false);
        let (second, second_worker) = server(&ca, false);
        let policy = crate::RegistrySettings::default().with_ca_certificates(&[(format!("127.0.0.1:{first}"), ca.pem())].into()).unwrap();
        let client = agent(policy);
        assert!(client.get(format!("https://127.0.0.1:{first}/v2/")).call().is_ok());
        let error = client.get(format!("https://127.0.0.1:{second}/v2/")).call().expect_err("unlisted destination");
        assert!(certificate_error(&error).is_some(), "{error:?}");
        first_worker.join().unwrap(); second_worker.join().unwrap();
    }
}
