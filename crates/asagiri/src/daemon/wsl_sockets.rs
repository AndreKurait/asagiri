use super::{binds, caller, engine};
use crate::diag;
use std::{io, path::PathBuf, os::unix::{fs::PermissionsExt, net::{UnixStream, UnixListener}}};

/// One WSL 2 distro enabled for WSL integration: its name and socket group GID.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WslDistroConfig {
    pub name: String,
    pub gid: u32,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct WslConfigFile {
    #[serde(default)]
    distros: Vec<WslDistroConfig>,
}

struct ActiveWslSocket {
    gid: u32,
    dir: PathBuf,
    socket: PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Manages per-distro Unix sockets under `/mnt/wsl/asagiri/sockets/<NAME>/docker.sock`.
/// Each socket has a `0750` parent directory and a `0660` socket file owned by
/// `0:<gid>`, tagging accepted HTTP requests with `Caller::Wsl("<NAME>")`.
pub struct WslSockets {
    base: PathBuf,
    active: std::sync::Mutex<std::collections::HashMap<String, ActiveWslSocket>>,
    serve: std::sync::Arc<dyn Fn(UnixStream, String) + Send + Sync>,
}

static WSL_SOCKETS: std::sync::OnceLock<WslSockets> = std::sync::OnceLock::new();

fn wsl_base_dir() -> PathBuf {
    PathBuf::from(binds::wsl_asagiri_dir())
}

pub(super) fn initialize(engine: std::sync::Arc<engine::Engine>, limit: usize) {
    let ws = WSL_SOCKETS.get_or_init(|| {
        WslSockets::new(wsl_base_dir(), move |conn, distro| {
            let engine = engine.clone();
            super::serve_connection(conn, move |conn| super::api::serve_as(conn, engine, caller::Caller::Wsl(distro)), &None, limit);
        })
    });
    if let Err(e) = ws.load_config() {
        diag::warn!("WSL integration sockets not loaded", error = e);
    }
}

pub fn wsl_report() -> serde_json::Value {
    let distros = WSL_SOCKETS.get().map_or_else(Vec::new, WslSockets::list);
    serde_json::json!({ "distros": distros })
}

pub fn sync_wsl_sockets(distros: &[WslDistroConfig]) -> io::Result<()> {
    let Some(ws) = WSL_SOCKETS.get() else {
        return Err(io::Error::other("WSL socket manager is not initialized"));
    };
    ws.sync(distros)
}

fn chown_gid(path: &std::path::Path, gid: u32) -> io::Result<()> {
    // SAFETY: geteuid only reads this process's effective user ID.
    let uid = if unsafe { libc::geteuid() } == 0 { Some(0) } else { None };
    std::os::unix::fs::chown(path, uid, Some(gid))
}

impl WslSockets {
    pub fn new(base: PathBuf, serve: impl Fn(UnixStream, String) + Send + Sync + 'static) -> Self {
        WslSockets {
            base,
            active: std::sync::Mutex::new(std::collections::HashMap::new()),
            serve: std::sync::Arc::new(serve),
        }
    }

    pub fn load_config(&self) -> io::Result<()> {
        let cfg_path = self.base.join("config.json");
        if !cfg_path.is_file() {
            return Ok(());
        }
        let bytes = std::fs::read(&cfg_path)?;
        let cfg: WslConfigFile = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid {}: {e}", cfg_path.display())))?;
        self.sync(&cfg.distros)
    }

    pub fn list(&self) -> Vec<WslDistroConfig> {
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<WslDistroConfig> = active
            .iter()
            .map(|(name, a)| WslDistroConfig { name: name.clone(), gid: a.gid })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn remove_socket_files(&self) {
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        for a in active.values() {
            a.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = std::fs::remove_file(&a.socket);
            let _ = std::fs::remove_dir(&a.dir);
        }
    }

    pub fn sync(&self, distros: &[WslDistroConfig]) -> io::Result<()> {
        for d in distros {
            if !binds::valid_distro_name(&d.name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid WSL distro name: {}", d.name),
                ));
            }
        }
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let wanted: std::collections::HashMap<&str, u32> = distros.iter().map(|d| (d.name.as_str(), d.gid)).collect();
        let remove_names: Vec<String> = active
            .iter()
            .filter(|(name, a)| wanted.get(name.as_str()) != Some(&a.gid) || !a.socket.exists())
            .map(|(name, _)| name.clone())
            .collect();
        for name in remove_names {
            if let Some(old) = active.remove(&name) {
                old.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = UnixStream::connect(&old.socket);
                let _ = std::fs::remove_file(&old.socket);
                let _ = std::fs::remove_dir(&old.dir);
            }
        }
        let sockets_dir = self.base.join("sockets");
        if let Ok(entries) = std::fs::read_dir(&sockets_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name_str) = name.to_str() else { continue };
                if !wanted.contains_key(name_str) {
                    let p = entry.path();
                    let _ = std::fs::remove_file(p.join("docker.sock"));
                    let _ = std::fs::remove_dir(&p);
                }
            }
        }
        if !distros.is_empty() {
            let roots_dir = self.base.join("roots");
            for d in [&self.base, &sockets_dir, &roots_dir] {
                std::fs::create_dir_all(d)?;
                std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        for d in distros {
            let dir = sockets_dir.join(&d.name);
            let sock = dir.join("docker.sock");
            if active.contains_key(&d.name) {
                chown_gid(&sock, d.gid)?;
                std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o660))?;
                chown_gid(&dir, d.gid)?;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750))?;
                continue;
            }
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            let _ = std::fs::remove_file(&sock);
            let listener = UnixListener::bind(&sock)?;
            std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
            chown_gid(&sock, d.gid)?;
            std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o660))?;
            chown_gid(&dir, d.gid)?;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750))?;
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_loop = stop.clone();
            let serve = self.serve.clone();
            let distro_name = d.name.clone();
            std::thread::Builder::new().name("asagiri-api-wsl".into()).spawn(move || {
                for conn in listener.incoming() {
                    if stop_loop.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    let Ok(conn) = conn else { continue };
                    if stop_loop.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    serve(conn, distro_name.clone());
                }
            })?;
            active.insert(
                d.name.clone(),
                ActiveWslSocket {
                    gid: d.gid,
                    dir,
                    socket: sock,
                    stop,
                },
            );
        }
        if self.base.exists() {
            let cfg_path = self.base.join("config.json");
            let tmp_path = self.base.join("config.json.tmp");
            let data = serde_json::to_vec(&WslConfigFile { distros: distros.to_vec() }).unwrap_or_default();
            std::fs::write(&tmp_path, data)?;
            std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))?;
            std::fs::rename(&tmp_path, &cfg_path)?;
        }
        Ok(())
    }
}

pub(super) fn cleanup() {
    if let Some(manager) = WSL_SOCKETS.get() { manager.remove_socket_files(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    #[test]
    fn per_distro_wsl_sockets_have_0660_permissions_and_clean_teardown_on_disable() {
        let base = std::env::temp_dir().join(format!("asagiri-wsl-sockets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // SAFETY: getegid only reads this process's effective group ID.
        let gid = unsafe { libc::getegid() };
        let ws = WslSockets::new(base.clone(), |mut conn, distro| {
            let _ = conn.write_all(distro.as_bytes());
        });
        ws.sync(&[WslDistroConfig { name: "Ubuntu".into(), gid }]).unwrap();

        let dir = base.join("sockets/Ubuntu");
        let sock = dir.join("docker.sock");
        assert!(sock.exists());
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let sock_mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o750, "parent directory must be 0750, got {dir_mode:o}");
        assert_eq!(sock_mode, 0o660, "socket must be 0660, got {sock_mode:o}");
        assert_ne!(sock_mode, 0o666, "socket must never be 0666");

        let mut client = UnixStream::connect(&sock).unwrap();
        let mut tagged = String::new();
        client.read_to_string(&mut tagged).unwrap();
        assert_eq!(tagged, "Ubuntu");

        // Disabling removes the socket and its parent directory cleanly.
        ws.sync(&[]).unwrap();
        assert!(!sock.exists(), "socket must be removed when disabled");
        assert!(!dir.exists(), "parent dir must be removed when disabled");

        // Path-traversal distro names are rejected.
        assert!(ws.sync(&[WslDistroConfig { name: "../evil".into(), gid }]).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }
}
