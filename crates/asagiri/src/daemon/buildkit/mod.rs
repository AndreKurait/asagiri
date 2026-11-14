//! Docker's built-in BuildKit driver on the existing engine socket.
//!
//! `/grpc` carries the control API. `/session` is an HTTP/2 server at the
//! client, transported inside stock BuildKit's bidirectional Control.Session
//! RPC. Proxy its context/auth/secret/SSH services unchanged; only a Moby
//! local-image export is received here and streamed into the image store.

mod transport;
mod wire;

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use bytes::Bytes;
use h2::server::SendResponse;
use h2::RecvStream;
use hyper::http::{HeaderMap, HeaderName, HeaderValue, Request as H2Request, Response};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc};

use super::api::Next;
use super::engine::{Engine, Error};
use super::http::{self, Request};
use transport::{MAX_DATA_MESSAGE, Prefixed, error};

const SOLVE: &str = "/moby.buildkit.v1.Control/Solve";
const WORKERS: &str = "/moby.buildkit.v1.Control/ListWorkers";
const SESSION: &str = "/moby.buildkit.v1.Control/Session";
const FILE_SEND: &str = "/moby.filesync.v1.FileSend/DiffCopy";
const SESSION_ID: &str = "x-docker-expose-session-uuid";
const SESSION_METHOD: &str = "x-docker-expose-session-grpc-method";

static CONNECTIONS: Semaphore = Semaphore::const_new(32);
static CALLS: Semaphore = Semaphore::const_new(256);
static IMPORTS: Semaphore = Semaphore::const_new(8);
static RUNTIME: LazyLock<io::Result<tokio::runtime::Runtime>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).max_blocking_threads(16)
        .thread_name("asa-buildkit").enable_all().build()
});
static SESSIONS: LazyLock<Mutex<HashMap<String, Weak<Session>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

struct Session {
    alive: AtomicBool,
    exports: Mutex<HashMap<usize, Arc<AtomicBool>>>,
}

struct SessionConnection(Arc<Session>);

impl Drop for SessionConnection {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::Release);
    }
}

fn register(metadata: &HeaderMap) -> io::Result<SessionConnection> {
    let mut ids = metadata.get_all(SESSION_ID).iter();
    let id = ids.next().and_then(|id| id.to_str().ok()).unwrap_or_default();
    if ids.next().is_some() || id.is_empty() || id.len() > 256 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)) {
        return Err(error("invalid BuildKit session ID"));
    }
    let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    sessions.retain(|_, value| value.strong_count() != 0);
    if sessions.get(id).and_then(Weak::upgrade).is_some() {
        return Err(error("BuildKit session ID is already connected"));
    }
    if sessions.len() >= 32 {
        return Err(error("too many BuildKit sessions"));
    }
    let session = Arc::new(Session { alive: AtomicBool::new(true), exports: Mutex::new(HashMap::new()) });
    sessions.insert(id.into(), Arc::downgrade(&session));
    Ok(SessionConnection(session))
}

struct Exports {
    session: Arc<Session>,
    entries: Vec<(usize, Arc<AtomicBool>)>,
}

impl Drop for Exports {
    fn drop(&mut self) {
        let mut exports = self.session.exports.lock().unwrap_or_else(|e| e.into_inner());
        for (id, state) in &self.entries {
            if exports.get(id).is_some_and(|old| Arc::ptr_eq(old, state)) {
                exports.remove(id);
            }
        }
    }
}

async fn exports(id: &str, indices: &[usize]) -> io::Result<Option<Exports>> {
    if indices.is_empty() { return Ok(None); }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    // The official client starts Session and Solve in separate goroutines.
    // Match BuildKit's own wait for that session to arrive.
    let session = loop {
        let session = SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).get(id).and_then(Weak::upgrade);
        if let Some(session) = session {
            if !session.alive.load(Ordering::Acquire) {
                return Err(error("BuildKit session has disconnected"));
            }
            break session;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(error("local image export session did not connect"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let mut current = session.exports.lock().unwrap_or_else(|e| e.into_inner());
    if indices.len() > 32 || current.len() + indices.len() > 32 || indices.iter().any(|id| current.contains_key(id)) {
        return Err(error("overlapping or excessive local image exporters in one session"));
    }
    let entries: Vec<_> = indices.iter().map(|id| (*id, Arc::new(AtomicBool::new(false)))).collect();
    current.extend(entries.iter().cloned());
    drop(current);
    Ok(Some(Exports { session, entries }))
}

/// The HTTP upgrade is accepted only on the already authorized Docker socket.
pub(super) fn upgrade(req: &Request, engine: &Arc<Engine>, w: &mut UnixStream, r: &mut BufReader<UnixStream>) -> io::Result<Next> {
    let valid = req.header("Upgrade").is_some_and(|value| value.eq_ignore_ascii_case("h2c"))
        && req.header("Connection").is_some_and(|value| value.split(',').any(|word| word.trim().eq_ignore_ascii_case("upgrade")))
        && !req.has_body();
    if !valid {
        super::api::error_with(w, Error::BadRequest("BuildKit requires an empty HTTP/1.1 h2c upgrade request".into()), "Connection: close\r\n")?;
        return Ok(Next::Close);
    }
    let Ok(_permit) = CONNECTIONS.try_acquire() else {
        http::respond_with(w, 503, "application/json", "Connection: close\r\n",
            br#"{"message":"too many BuildKit connections"}"#)?;
        return Ok(Next::Close);
    };
    let mut metadata = HeaderMap::new();
    for (name, value) in req.all_headers() {
        if name.to_ascii_lowercase().starts_with("x-docker-expose-session-")
            || matches!(name.to_ascii_lowercase().as_str(), "traceparent" | "tracestate" | "baggage") {
            metadata.append(HeaderName::from_bytes(name.as_bytes()).map_err(error)?, HeaderValue::from_str(value).map_err(error)?);
        }
    }
    let session = if req.path == "/session" {
        let registration = match register(&metadata) {
            Ok(session) => session,
            Err(e) => {
                super::api::error_with(w, Error::BadRequest(e.to_string()), "Connection: close\r\n")?;
                return Ok(Next::Close);
            }
        };
        if !metadata.get_all(SESSION_METHOD).iter().any(|value| value == FILE_SEND) {
            metadata.append(SESSION_METHOD, HeaderValue::from_static(FILE_SEND));
        }
        Some(registration)
    } else { None };
    let (backend, _busy) = match super::builder::connection(engine) {
        Ok(connection) => connection,
        Err(e) => {
            super::api::error_with(w, e, "Connection: close\r\n")?;
            return Ok(Next::Close);
        }
    };
    let runtime = RUNTIME.as_ref().map_err(|e| error(e.to_string()))?;
    let client = w.try_clone()?;
    let guest = UnixStream::from(backend.fd.try_clone()?);
    guest.set_nonblocking(true)?;
    // Write the upgrade before changing the shared socket's blocking mode.
    w.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: h2c\r\n\r\n")?;
    w.flush()?;
    client.set_nonblocking(true)?;
    let prefix = Bytes::copy_from_slice(r.buffer());
    let engine = engine.clone();
    let result = runtime.block_on(async move {
        let client = Prefixed { io: tokio::net::UnixStream::from_std(client)?, prefix };
        let guest = transport::Framed::new(tokio::net::UnixStream::from_std(guest)?, backend);
        match session {
            Some(session) => session_proxy(client, guest, metadata, engine, session).await,
            None => control_proxy(client, guest, engine).await,
        }
    });
    // Disconnects cancel the affected RPCs and drop all relay ownership.
    if let Err(error) = result {
        crate::diag::debug!("BuildKit connection ended", error = error);
    }
    Ok(Next::Close)
}

#[derive(Clone)]
enum Context {
    Control { gateway: String },
    Session { engine: Arc<Engine>, session: Arc<Session> },
}

async fn serve<T>(io: T, upstream: h2::client::SendRequest<Bytes>, context: Context) -> io::Result<()>
where T: AsyncRead + AsyncWrite + Unpin {
    let mut connection = transport::server(io).await?;
    let mut calls = tokio::task::JoinSet::new();
    while let Some(next) = connection.accept().await {
        let (request, mut respond) = next.map_err(error)?;
        while calls.try_join_next().is_some() {}
        let permit = match CALLS.try_acquire() {
            Ok(permit) => permit,
            Err(_) => {
                status(&mut respond, 8, "too many BuildKit RPCs")?;
                continue;
            }
        };
        let (context, upstream) = (context.clone(), upstream.clone());
        calls.spawn(async move {
            let _permit = permit;
            if let Context::Session { engine, session } = &context
                && request.uri().path() == FILE_SEND {
                let value = request.headers().get("buildkit-attachable-exporter-id");
                let id = value.and_then(|value| value.to_str().ok()).unwrap_or("0").parse::<usize>();
                if let Ok(id) = id {
                    let load = session.exports.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
                    if let Some(load) = load {
                        if load.swap(true, Ordering::AcqRel) {
                            return status(&mut respond, 6, "local image exporter already used");
                        }
                        return load_image(request.into_body(), respond, engine.clone()).await;
                    }
                }
            }
            proxy(request, respond, upstream, context).await
        });
    }
    // JoinSet aborts pending calls on a disconnect, propagating HTTP/2 resets
    // to the backend and removing their export registrations.
    Ok(())
}

async fn control_proxy<C, B>(client: C, backend: B, engine: Arc<Engine>) -> io::Result<()>
where C: AsyncRead + AsyncWrite + Unpin,
      B: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    let (upstream, mut driver) = transport::client(backend).await?;
    let gateway = super::mirror::host_addr(&engine);
    tokio::select! {
        result = serve(client, upstream, Context::Control { gateway }) => result,
        _ = &mut driver.0 => Err(error("BuildKit backend connection ended")),
    }
}

async fn session_proxy<C, B>(client: C, backend: B, metadata: HeaderMap, engine: Arc<Engine>, session: SessionConnection) -> io::Result<()>
where C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
      B: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    let (upstream, mut client_driver) = transport::client(client).await?;
    let (backend, mut backend_driver) = transport::client(backend).await?;
    let mut request = H2Request::builder().method("POST").uri(format!("http://buildkit{SESSION}"))
        .header("content-type", "application/grpc").header("te", "trailers").body(()).map_err(error)?;
    request.headers_mut().extend(metadata);
    let (response, mut output) = backend.ready().await.map_err(error)?.send_request(request, false).map_err(error)?;
    let response = tokio::time::timeout(Duration::from_secs(15), response).await.map_err(error)?.map_err(error)?;
    if response.status() != 200 {
        return Err(error("BuildKit rejected its session transport"));
    }
    let (relay, server) = tokio::io::duplex(transport::WINDOW);
    let (mut read, mut write) = tokio::io::split(relay);
    let mut outgoing = transport::spawn(async move {
        let mut bytes = vec![0; 32 << 10];
        loop {
            let n = read.read(&mut bytes).await?;
            if n == 0 { break; }
            transport::send(&mut output, transport::frame(&wire::message(&bytes[..n]))).await?;
        }
        transport::finish(&mut output, None)
    });
    let mut incoming = transport::spawn(async move {
        let mut reader = transport::Grpc::new(response.into_body(), MAX_DATA_MESSAGE);
        while let Some(message) = reader.next().await? {
            write.write_all(wire::payload(&message)?).await?;
        }
        write.shutdown().await
    });
    tokio::select! {
        result = serve(server, upstream, Context::Session { engine, session: session.0.clone() }) => result,
        _ = &mut incoming.0 => Err(error("BuildKit session receiver ended")),
        _ = &mut outgoing.0 => Err(error("BuildKit session sender ended")),
        _ = &mut backend_driver.0 => Err(error("BuildKit session backend ended")),
        _ = &mut client_driver.0 => Err(error("BuildKit session client disconnected")),
    }
}

fn escaped(message: &str) -> String {
    let mut output = String::new();
    for byte in message.bytes().take(1024) {
        if (32..127).contains(&byte) && byte != b'%' {
            output.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(&mut output, "%{byte:02X}");
        }
    }
    output
}

fn status(respond: &mut SendResponse<Bytes>, code: u8, message: &str) -> io::Result<()> {
    let response = Response::builder().header("content-type", "application/grpc")
        .header("grpc-status", code.to_string()).header("grpc-message", escaped(message)).body(()).map_err(error)?;
    respond.send_response(response, true).map_err(error)?;
    Ok(())
}

fn trailers(code: u8, message: &str) -> HeaderMap {
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", HeaderValue::from_str(&code.to_string()).expect("status number"));
    trailers.insert("grpc-message", HeaderValue::from_str(&escaped(message)).expect("escaped ASCII"));
    trailers
}

async fn proxy(request: H2Request<RecvStream>, mut respond: SendResponse<Bytes>,
    upstream: h2::client::SendRequest<Bytes>, context: Context) -> io::Result<()> {
    let path = request.uri().path().to_owned();
    let (mut parts, body) = request.into_parts();
    parts.uri = format!("http://buildkit{path}").parse().map_err(error)?;
    let mut registration = None;
    let mut replacement = None;
    let mut original = Some(body);
    if matches!(context, Context::Control { .. }) && path == SOLVE {
        let plan = async {
            let (data, _) = tokio::time::timeout(Duration::from_secs(15), transport::unary(original.take().expect("request body")))
                .await.map_err(error)??;
            let plan = wire::solve(&data.ok_or_else(|| error("missing BuildKit Solve request"))?)?;
            registration = exports(&plan.session, &plan.load_exporters).await?;
            replacement = Some(transport::frame(&plan.data));
            Ok::<(), io::Error>(())
        };
        let planned = tokio::select! {
            result = plan => result,
            _ = std::future::poll_fn(|cx| respond.poll_reset(cx)) => return Err(error("Solve cancelled before dispatch")),
        };
        if let Err(e) = planned {
            return status(&mut respond, 3, &e.to_string());
        }
        parts.headers.remove("content-length");
    }
    let request = H2Request::from_parts(parts, ());
    let (response, mut output) = upstream.ready().await.map_err(error)?.send_request(request, false).map_err(error)?;
    let _upload = transport::spawn(async move {
        if let Some(data) = replacement {
            transport::send(&mut output, data).await?;
            transport::finish(&mut output, None)
        } else {
            transport::pump(original.expect("unmodified request body"), output).await
        }
    });
    let response = tokio::select! {
        response = response => response.map_err(error)?,
        _ = std::future::poll_fn(|cx| respond.poll_reset(cx)) => return Err(error("BuildKit request cancelled")),
    };
    let (mut parts, body) = response.into_parts();
    let rewrite = match &context {
        Context::Control { gateway } if path == WORKERS => Some(gateway.as_str()),
        _ => None,
    };
    if rewrite.is_some() { parts.headers.remove("content-length"); }
    let mut output = respond.send_response(Response::from_parts(parts, ()), false).map_err(error)?;
    if let Some(gateway) = rewrite {
        let result = tokio::select! {
            result = transport::unary(body) => result,
            _ = std::future::poll_fn(|cx| output.poll_reset(cx)) => return Err(error("worker query cancelled")),
        };
        let (data, end) = result?;
        if let Some(data) = data {
            transport::send(&mut output, transport::frame(&wire::workers(&data, gateway)?)).await?;
        }
        transport::finish(&mut output, end)?;
    } else {
        transport::pump(body, output).await?;
    }
    drop(registration);
    Ok(())
}

enum Chunk {
    Data(Bytes),
    End,
}

struct ArchiveReader {
    input: mpsc::Receiver<Chunk>,
    pending: Bytes,
    ended: bool,
}

impl Read for ArchiveReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() { return Ok(0); }
        while self.pending.is_empty() && !self.ended {
            match self.input.blocking_recv() {
                Some(Chunk::Data(data)) => self.pending = data,
                Some(Chunk::End) => self.ended = true,
                // A cancelled RPC is not a successful end of an archive.
                None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "image export was interrupted")),
            }
        }
        let n = output.len().min(self.pending.len());
        output[..n].copy_from_slice(&self.pending.split_to(n));
        Ok(n)
    }
}

async fn load_image(body: RecvStream, mut respond: SendResponse<Bytes>, engine: Arc<Engine>) -> io::Result<()> {
    let _permit = tokio::select! {
        permit = IMPORTS.acquire() => permit.map_err(error)?,
        _ = std::future::poll_fn(|cx| respond.poll_reset(cx)) => return Err(error("image import cancelled")),
    };
    let mut output = respond.send_response(Response::builder().header("content-type", "application/grpc")
        .body(()).map_err(error)?, false).map_err(error)?;
    let (send, input) = mpsc::channel(8);
    let store_engine = engine.clone();
    let mut loader = tokio::task::spawn_blocking(move || {
        asagiri_image::archive::load(&store_engine.store, ArchiveReader { input, pending: Bytes::new(), ended: false })
    });
    let receive = async {
        let mut reader = transport::Grpc::new(body, MAX_DATA_MESSAGE);
        while let Some(message) = reader.next().await? {
            for chunk in wire::payload(&message)?.chunks(64 << 10) {
                send.send(Chunk::Data(Bytes::copy_from_slice(chunk))).await.map_err(|_| error("image loader stopped"))?;
            }
        }
        send.send(Chunk::End).await.map_err(|_| error("image loader stopped"))?;
        Ok::<(), io::Error>(())
    };
    let result = tokio::select! {
        result = receive => result,
        _ = std::future::poll_fn(|cx| output.poll_reset(cx)) => Err(error("image import cancelled")),
    };
    drop(send);
    // Dropping the sender wakes a blocked loader with UnexpectedEof. An
    // archive cannot publish before its complete gRPC request was received.
    let loaded = (&mut loader).await.map_err(error)?;
    let loaded = result.and(loaded);
    match loaded {
        Ok(images) if !images.is_empty() => {
            for image in images {
                engine.emit_image("load", &image.config_digest, &image.name);
            }
            transport::finish(&mut output, Some(trailers(0, "")))
        }
        Ok(_) => transport::finish(&mut output, Some(trailers(13, "image export contained no images"))),
        Err(e) => transport::finish(&mut output, Some(trailers(13, &e.to_string()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interrupted_archive_is_not_a_clean_end() {
        let (send, input) = mpsc::channel(2);
        send.blocking_send(Chunk::Data(Bytes::from_static(b"archive"))).unwrap();
        drop(send);
        let mut reader = ArchiveReader { input, pending: Bytes::new(), ended: false };
        let mut bytes = Vec::new();
        assert_eq!(reader.read_to_end(&mut bytes).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(bytes, b"archive");
    }

    #[tokio::test]
    async fn exporter_claims_are_scoped_to_the_session_and_released_on_cancellation() {
        let mut metadata = HeaderMap::new();
        metadata.insert(SESSION_ID, HeaderValue::from_static("unit-test-session"));
        let connection = register(&metadata).unwrap();
        assert!(register(&metadata).is_err());
        let claim = exports("unit-test-session", &[0, 2]).await.unwrap().unwrap();
        assert!(exports("unit-test-session", &[2]).await.is_err());
        drop(claim);
        assert!(connection.0.exports.lock().unwrap().is_empty());
        let claim = exports("unit-test-session", &[0]).await.unwrap().unwrap();
        drop(connection);
        assert!(exports("unit-test-session", &[1]).await.is_err());
        drop(claim);
        assert_eq!(SESSIONS.lock().unwrap()["unit-test-session"].strong_count(), 0);
    }

    #[test]
    fn duplicate_or_invalid_session_ids_are_rejected() {
        for values in [&[""][..], &["one", "two"], &["contains/slash"]] {
            let mut metadata = HeaderMap::new();
            for value in values { metadata.append(SESSION_ID, HeaderValue::from_str(value).unwrap()); }
            assert!(register(&metadata).is_err());
        }
    }
}
