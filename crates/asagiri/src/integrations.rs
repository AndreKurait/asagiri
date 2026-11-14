//! Per-user permissions shared by the native window and `asagiri mcp`.
//! Reading a missing file disables access; an unreadable or malformed file
//! fails closed. Every tool request reads it again.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_SETTINGS: u64 = 16 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Permissions {
    pub enabled: bool,
    pub allow_container_changes: bool,
}

#[derive(Clone, Debug)]
pub struct Settings {
    directory: PathBuf,
}

impl Settings {
    pub fn current() -> io::Result<Self> {
        Ok(Self { directory: directory()? })
    }

    #[cfg(test)]
    pub fn at(directory: PathBuf) -> Self { Self { directory } }

    pub fn load(&self) -> io::Result<Permissions> {
        let path = self.directory.join("mcp.json");
        let file = match open(&path, false) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Permissions::default()),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.take(MAX_SETTINGS + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SETTINGS {
            return Err(io::Error::other("MCP settings file is too large"));
        }
        serde_json::from_slice(&bytes).map_err(|e| io::Error::other(format!("invalid MCP settings: {e}")))
    }

    pub fn update(&self, change: impl FnOnce(&mut Permissions)) -> io::Result<Permissions> {
        private_directory(&self.directory)?;
        let lock = open(&self.directory.join("mcp.lock"), true)?;
        lock.lock()?;
        let mut permissions = self.load()?;
        change(&mut permissions);
        if !permissions.enabled { permissions.allow_container_changes = false; }
        let mut data = serde_json::to_vec_pretty(&permissions)?;
        data.push(b'\n');
        private_write(&self.directory.join("mcp.json"), &data)?;
        Ok(permissions)
    }
}

pub fn directory() -> io::Result<PathBuf> {
    #[cfg(unix)]
    let path = std::env::var_os("HOME").map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is not set"))?
        .join(crate::flavor::STATE.get()).join("integrations");
    #[cfg(windows)]
    let path = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        .ok_or_else(|| io::Error::other("LOCALAPPDATA is not set"))?
        .join("Asagiri").join("integrations");
    Ok(path)
}

pub fn private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(io::Error::other("integration settings directory must be a real directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "integration settings directory must be owned by this user with mode 0700"));
        }
    }
    Ok(())
}

pub(crate) fn open(path: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(create).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() { return Err(io::Error::other("integration settings must be a regular file")); }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "integration settings must be owned by this user with mode 0600"));
        }
    }
    Ok(file)
}

pub fn private_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| io::Error::other("settings have no parent directory"))?;
    let temporary = parent.join(format!(".settings-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // If the name already exists, leave that file untouched.
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&temporary); }
    result
}

pub fn connection_config() -> io::Result<Value> {
    #[allow(unused_mut)]
    let mut executable = std::env::current_exe()?;
    #[cfg(windows)]
    executable.set_file_name("asagiri.exe");
    // The Linux window is a separate binary; clients need the installed CLI.
    #[cfg(target_os = "linux")]
    if executable.file_name().is_some_and(|n| n == "asagiri-window") {
        executable = PathBuf::from("/usr/bin/asagiri");
    }
    Ok(json!({"mcpServers": {"asagiri": {"command": executable, "args": ["mcp"]}}}))
}

/// Shared native UI command. The web/MCP tools cannot change permissions.
pub fn report(save: Option<Value>) -> Value {
    let result = (|| -> io::Result<Value> {
        let settings = Settings::current()?;
        let permissions = match save {
            Some(value) => {
                let requested: Permissions = serde_json::from_value(value)?;
                settings.update(|p| *p = requested)?
            }
            None => settings.load()?,
        };
        Ok(json!({"type": "integrations", "permissions": permissions, "config": connection_config()?}))
    })();
    result.unwrap_or_else(|e| json!({"type": "integrations", "error": e.to_string()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_default_off_and_revocation_reaches_other_readers() {
        let directory = std::env::temp_dir().join(format!("asagiri-integrations-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let settings = Settings::at(directory.clone());
        assert_eq!(settings.load().unwrap(), Permissions::default());
        settings.update(|p| { p.enabled = true; p.allow_container_changes = true; }).unwrap();
        assert!(Settings::at(directory.clone()).load().unwrap().allow_container_changes);
        settings.update(|p| p.enabled = false).unwrap();
        assert_eq!(Settings::at(directory.clone()).load().unwrap(), Permissions::default());
        private_write(&directory.join("mcp.json"), b"{\"enabled\":true,\"unknown\":true}").unwrap();
        assert!(settings.load().is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn settings_refuse_symlinks_and_other_users_access() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let directory = std::env::temp_dir().join(format!("asagiri-integrations-private-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let settings = Settings::at(directory.clone());
        settings.update(|p| p.enabled = true).unwrap();
        let path = directory.join("mcp.json");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::rename(&path, directory.join("other")).unwrap();
        symlink("other", &path).unwrap();
        assert!(settings.load().is_err());
        fs::remove_file(&path).unwrap();
        fs::rename(directory.join("other"), &path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(settings.load().is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
