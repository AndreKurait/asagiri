//! The window and `asagiri` subcommands talk to `asagiri daemon` the way the docker CLI does: HTTP over
//! the daemon's unix socket. One short connection per request, and one
//! long-lived one for `/events`.

// Parts of this serve only the Mac (the window, boot timings).
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use crate::wsl_paths::valid_distro_name;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

#[path = "client/health.rs"]
mod health;
#[path = "client/health_response.rs"]
mod health_response;

/// A small background GET with a deadline covering the entire exchange.
pub fn probe(path: &str, timeout: Duration) -> io::Result<Response> {
    health::request_at(&socket()?, path, timeout)
}

/// The privileged Linux setup helper probes its fixed service socket.
#[cfg(target_os = "linux")]
pub fn probe_socket(socket: &std::path::Path, path: &str, timeout: Duration) -> io::Result<Response> {
    health::request_at(socket, path, timeout)
}

#[cfg(target_os = "macos")]
pub fn probe_connection(timeout: Duration) -> io::Result<UnixStream> {
    health::connect(&socket()?, std::time::Instant::now() + timeout)
}

/// Product version for updates and About; older engines used Version alone.
pub fn engine_version(version: &Value) -> Option<&str> {
    version["AsagiriVersion"].as_str().or_else(|| version["Version"].as_str())
}

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn update_checks_use_the_product_version_with_an_older_engine_fallback() {
        assert_eq!(engine_version(&serde_json::json!({"Version": "27.5.1-asagiri", "AsagiriVersion": "0.1.2"})), Some("0.1.2"));
        assert_eq!(engine_version(&serde_json::json!({"Version": "0.1.0"})), Some("0.1.0"));
        assert_eq!(engine_version(&Value::Null), None);
    }
}

pub fn socket() -> io::Result<PathBuf> {
    let asagiri_socket = std::env::var_os("ASAGIRI_SOCKET");
    let home = std::env::var_os("HOME");
    #[cfg(target_os = "linux")]
    {
        let wsl_distro = std::env::var_os("WSL_DISTRO_NAME");
        let wsl_sockets_dir = PathBuf::from(crate::wsl_paths::wsl_asagiri_dir()).join("sockets");
        socket_for(
            asagiri_socket.as_deref(),
            Some(&service_socket()),
            wsl_distro.as_deref(),
            Some(wsl_sockets_dir.as_path()),
            &[Path::new("/var/run/docker.sock"), Path::new("/run/docker.sock")],
            home.as_deref(),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        socket_for(asagiri_socket.as_deref(), None, None, None, &[], home.as_deref())
    }
}

fn is_wsl_socket_path(target: &Path, wsl_sockets_dir: &Path) -> bool {
    let Ok(rel) = target.strip_prefix(wsl_sockets_dir) else {
        return false;
    };
    let mut comps = rel.components();
    match (comps.next(), comps.next(), comps.next()) {
        (Some(Component::Normal(distro)), Some(Component::Normal(file)), None) => {
            distro.to_str().is_some_and(valid_distro_name) && file == "docker.sock"
        }
        _ => false,
    }
}

/// Whether a socket path is present on disk, treating `EACCES` on a parent
/// directory (such as a `0750` WSL integration socket directory) as present so
/// `check_alive` surfaces a clear permission-denied error instead of falling
/// back to a missing home socket.
fn socket_present(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(_) => true,
        Err(e) => e.kind() == io::ErrorKind::PermissionDenied,
    }
}

fn socket_for(
    asagiri_socket: Option<&OsStr>,
    service_socket: Option<&Path>,
    wsl_distro: Option<&OsStr>,
    wsl_sockets_dir: Option<&Path>,
    docker_sock_links: &[&Path],
    home: Option<&OsStr>,
) -> io::Result<PathBuf> {
    if let Some(s) = asagiri_socket {
        return Ok(PathBuf::from(s));
    }
    // Linux: the engine is the `asagiri` system service (contrib/linux/asagiri.service)
    // or a root-run `sudo asagiri daemon` at `/run/asagiri/docker.sock`.
    if let Some(service) = service_socket
        && socket_present(service)
    {
        return Ok(service.to_path_buf());
    }
    // WSL 2 integration: Asagiri for Windows exposes per-distro sockets under
    // `/mnt/wsl/asagiri/sockets/<distro>/docker.sock` and symlinks
    // `/var/run/docker.sock` to them.
    if let Some(wsl_dir) = wsl_sockets_dir {
        if let Some(distro) = wsl_distro.and_then(|d| d.to_str()).filter(|d| valid_distro_name(d)) {
            let candidate = wsl_dir.join(distro).join("docker.sock");
            if socket_present(&candidate) {
                return Ok(candidate);
            }
        }
        for link in docker_sock_links {
            if let Ok(target) = std::fs::read_link(link)
                && is_wsl_socket_path(&target, wsl_dir)
                && socket_present(&target)
            {
                return Ok(target);
            }
        }
    }
    let home = home.map(PathBuf::from).ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(home.join(crate::flavor::STATE.get()).join("run/docker.sock"))
}


/// Where the Linux service's engine listens, for members of the `asagiri` group.
#[cfg(target_os = "linux")]
pub const SERVICE_SOCKET: &str = "/run/asagiri/docker.sock";

/// The same for this Asagiri (`/run/asagiri-dev/docker.sock` for Asagiri Dev): where
/// an engine run as root listens.
#[cfg(target_os = "linux")]
pub fn service_socket() -> PathBuf {
    PathBuf::from("/run").join(crate::flavor::CONTEXT.get()).join("docker.sock")
}

/// An answer: status and body.
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// The daemon's `{"message": …}` on failure, or the body as text.
    pub fn error(&self) -> String {
        self.json()["message"].as_str().map(str::to_owned).unwrap_or_else(|| String::from_utf8_lossy(&self.body).into_owned())
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

fn connect(timeout: Option<Duration>) -> io::Result<UnixStream> {
    let s = UnixStream::connect(socket()?)?;
    s.set_read_timeout(timeout)?;
    Ok(s)
}

fn head(r: &mut impl BufRead) -> io::Result<(u16, Option<usize>, bool)> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad status line"))?;
    let (mut len, mut chunked) = (None, false);
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            match k.to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().ok(),
                "transfer-encoding" => chunked = v.trim().eq_ignore_ascii_case("chunked"),
                _ => {}
            }
        }
    }
    Ok((status, len, chunked))
}

/// Reads one chunk of a chunked body; `None` at the end.
fn chunk(r: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let n = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk"))?;
    if n == 0 {
        return Ok(None);
    }
    let mut buf = vec![0; n + 2];
    r.read_exact(&mut buf)?;
    buf.truncate(n);
    Ok(Some(buf))
}

/// One request; the whole body is read, chunked or not.
pub fn request(method: &str, path: &str, body: Option<&Value>) -> io::Result<Response> {
    send(method, path, body, "")
}

/// A GET the window makes on its own (status, lists): it doesn't count as
/// use, so it never keeps an idle VM awake.
pub fn get_background(path: &str) -> io::Result<Value> {
    let r = send("GET", path, None, "X-Asagiri-Background: 1\r\n")?;
    if !r.ok() {
        return Err(io::Error::other(r.error()));
    }
    Ok(r.json())
}

fn send(method: &str, path: &str, body: Option<&Value>, extra: &str) -> io::Result<Response> {
    let mut s = connect(Some(Duration::from_secs(120)))?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    // The window's action this is part of, for the engine's log lines.
    let op = crate::diag::op_header();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: asagiri\r\nConnection: close\r\n{extra}{op}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    )?;
    let mut r = BufReader::new(s);
    let (status, len, chunked) = head(&mut r)?;
    let mut body = Vec::new();
    // These never have a body, and the daemon keeps the connection open.
    if status == 204 || status == 304 {
        return Ok(Response { status, body });
    }
    if chunked {
        while let Some(c) = chunk(&mut r)? {
            body.extend_from_slice(&c);
        }
    } else if let Some(n) = len {
        body.resize(n, 0);
        r.read_exact(&mut body)?;
    } else {
        r.read_to_end(&mut body)?;
    }
    Ok(Response { status, body })
}

pub fn get(path: &str) -> io::Result<Value> {
    let r = request("GET", path, None)?;
    if !r.ok() {
        return Err(io::Error::other(r.error()));
    }
    Ok(r.json())
}

/// Whether the daemon answers at all.
pub fn alive() -> bool {
    send("GET", "/_ping", None, "X-Asagiri-Background: 1\r\n").is_ok_and(|r| r.ok())
}

/// A POST whose answer is JSON lines arriving over time (a pull's
/// progress): `on_line` sees each as it comes. Returns the HTTP status.
/// `auth` is a registry sign-in for a pull (`X-Registry-Auth`, base64url).
/// `opened` gets the connection once it is made, so another thread can
/// shut it down to stop the request (Cancel on a pull).
pub fn post_lines_closable(path: &str, auth: Option<&str>, opened: impl FnOnce(&UnixStream), mut on_line: impl FnMut(Value)) -> io::Result<u16> {
    if auth.is_some_and(|a| a.contains(['\r', '\n'])) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "header contains a line break"));
    }
    let auth = auth.map(|a| format!("X-Registry-Auth: {a}\r\n")).unwrap_or_default();
    let mut s = connect(Some(Duration::from_secs(120)))?;
    opened(&s);
    write!(s, "POST {path} HTTP/1.1\r\nHost: asagiri\r\nConnection: close\r\n{auth}{}Content-Type: application/json\r\nContent-Length: 0\r\n\r\n", crate::diag::op_header())?;
    let mut r = BufReader::new(s);
    let (status, len, chunked) = head(&mut r)?;
    let mut pending = Vec::new();
    let mut lines = |pending: &mut Vec<u8>| {
        while let Some(i) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=i).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                on_line(v);
            }
        }
    };
    if chunked {
        while let Some(c) = chunk(&mut r)? {
            pending.extend_from_slice(&c);
            lines(&mut pending);
        }
    } else if let Some(n) = len {
        pending.resize(n, 0);
        r.read_exact(&mut pending)?;
    } else if status != 204 {
        r.read_to_end(&mut pending)?;
    }
    // A last line with no newline, or a plain error body.
    pending.push(b'\n');
    lines(&mut pending);
    Ok(status)
}

/// Calls `on_event` for each event until the connection ends or it returns false.
pub fn events(mut on_event: impl FnMut(Value) -> bool) -> io::Result<()> {
    let mut s = connect(None)?;
    write!(s, "GET /events HTTP/1.1\r\nHost: asagiri\r\nX-Asagiri-Background: 1\r\n\r\n")?;
    let mut r = BufReader::new(s);
    let (status, _, chunked) = head(&mut r)?;
    if status != 200 || !chunked {
        return Err(io::Error::other(format!("events: HTTP {status}")));
    }
    let mut pending = Vec::new();
    while let Some(c) = chunk(&mut r)? {
        pending.extend_from_slice(&c);
        while let Some(i) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=i).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                if !on_event(v) {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// A long-lived streamed response, such as `logs?follow=1`, read chunk by
/// chunk. `closer` ends it from another thread.
pub struct Stream {
    reader: BufReader<UnixStream>,
}

impl Stream {
    pub fn open(path: &str) -> io::Result<Stream> {
        let mut s = connect(None)?;
        write!(s, "GET {path} HTTP/1.1\r\nHost: asagiri\r\n\r\n")?;
        let mut reader = BufReader::new(s);
        let (status, _, chunked) = head(&mut reader)?;
        if status != 200 || !chunked {
            return Err(io::Error::other(format!("{path}: HTTP {status}")));
        }
        Ok(Stream { reader })
    }

    /// A handle whose `shutdown` makes `next` return `None`.
    pub fn closer(&self) -> io::Result<UnixStream> {
        self.reader.get_ref().try_clone()
    }

    pub fn next(&mut self) -> Option<Vec<u8>> {
        chunk(&mut self.reader).ok().flatten()
    }
}

/// Download the checkpoint without holding the process memory image in host RAM.
pub fn checkpoint_export(args: &[String]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let [container, name, output] = args else {
        return Err(io::Error::other("usage: asagiri checkpoint export CONTAINER NAME OUTPUT.tar"));
    };
    if [container, name].iter().any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) || matches!(s.as_str(), "." | "..")) {
        return Err(io::Error::other("invalid container or checkpoint name"));
    }
    let mut stream = Stream::open(&format!("/asagiri/checkpoints/{container}/{name}"))?;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(output)?;
    let result = (|| {
        loop {
            if stream.reader.fill_buf()?.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "incomplete checkpoint download"));
            }
            match chunk(&mut stream.reader)? {
                Some(bytes) => file.write_all(&bytes)?,
                None => break,
            }
        }
        file.sync_all()
    })();
    if result.is_err() { let _ = std::fs::remove_file(output); }
    result
}

/// Runs `cmd` in a running container and returns (exit code, output),
/// through the exec API with a terminal, so output arrives unframed.
pub fn exec(container: &str, cmd: &[&str]) -> io::Result<(i64, String)> {
    let created = request("POST", &format!("/containers/{container}/exec"), Some(&serde_json::json!({"Cmd": cmd, "AttachStdout": true, "AttachStderr": true, "Tty": true})))?;
    if !created.ok() {
        return Err(io::Error::other(created.error()));
    }
    let id = created.json()["Id"].as_str().unwrap_or_default().to_owned();
    let mut s = connect(Some(Duration::from_secs(120)))?;
    let body = r#"{"Detach":false,"Tty":true}"#;
    write!(
        s,
        "POST /exec/{id}/start HTTP/1.1\r\nHost: asagiri\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )?;
    let mut r = BufReader::new(s);
    let (status, _, _) = head(&mut r)?;
    if status != 101 && status != 200 {
        return Err(io::Error::other(format!("exec: HTTP {status}")));
    }
    let mut out = Vec::new();
    r.read_to_end(&mut out)?;
    let code = get(&format!("/exec/{id}/json"))?["ExitCode"].as_i64().unwrap_or(-1);
    Ok((code, String::from_utf8_lossy(&out).replace("\r\n", "\n")))
}

/// A docker CLI: on PATH, else the one bundled in Asagiri.app.
pub fn docker_cli() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("docker")).find(|d| d.is_file()));
    on_path
        .or_else(|| {
            let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
            Some(exe.parent()?.join("../Resources/bin/docker")).filter(|d| d.is_file())
        })
        .or_else(lite_cli)
}

/// Asagiri Lite's downloaded CLI.
#[cfg(target_os = "macos")]
fn lite_cli() -> Option<PathBuf> {
    crate::tools::dir().ok().map(|d| d.join("docker")).filter(|d| d.is_file())
}

#[cfg(not(target_os = "macos"))]
fn lite_cli() -> Option<PathBuf> {
    None
}
