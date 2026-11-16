//! Atomic edits of Docker's shared configuration. Temporary files are private
//! before any credential bytes are written, and Asagiri writers share a lock.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub(super) fn read(path: &Path) -> io::Result<Value> {
    let value = match fs::read(path) {
        Ok(bytes) => super::parse_config(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e),
    };
    if !value.is_object() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "config.json isn't an object"));
    }
    if value.get("auths").is_some_and(|a| !a.is_null() && !a.is_object()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "config.json auths isn't an object"));
    }
    Ok(value)
}

pub(super) fn auths(value: &mut Value) -> io::Result<&mut Map<String, Value>> {
    let object = value.as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "config.json isn't an object"))?;
    let auths = object.entry("auths").or_insert_with(|| json!({}));
    if auths.is_null() { *auths = json!({}); }
    auths.as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "config.json auths isn't an object"))
}

fn destination(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        // Like Docker, update the target of an existing config symlink.
        // A dangling link is an error, never replaced with a new config.
        Ok(_) => path.canonicalize(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or_else(|| io::Error::other("config has no parent"))?;
            let name = path.file_name().ok_or_else(|| io::Error::other("config has no filename"))?;
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(parent)?;
            Ok(parent.canonicalize()?.join(name))
        }
        Err(e) => Err(e),
    }
}

fn lock(path: &Path) -> io::Result<File> {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().ok_or_else(|| io::Error::other("config has no filename"))?);
    name.push(".asagiri-lock");
    let lock_path = path.with_file_name(name);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(lock_path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("config lock isn't a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "config lock belongs to another user"));
        }
    }
    file.lock()?;
    // Leave the lock inode in place. Removing it after unlock would let a
    // waiting writer lock a different inode from the next arriving writer.
    Ok(file)
}

fn persist(path: &Path, temporary: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // create_new also rejects a pre-existing temporary symlink. On failure
    // its target and the pre-existing name must both remain untouched.
    let mut file = options.open(temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(temporary, path)?;
        #[cfg(unix)]
        File::open(path.parent().ok_or_else(|| io::Error::other("config has no parent"))?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(temporary); }
    result
}

pub(super) fn edit(path: &Path, change: impl FnOnce(&mut Value) -> io::Result<()>) -> io::Result<()> {
    let path = destination(path)?;
    let _lock = lock(&path)?;
    let mut value = read(&path)?;
    change(&mut value)?;
    let mut bytes = serde_json::to_vec_pretty(&value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let temporary = path.with_file_name(format!(".asagiri-config-{}-{}.tmp",
        std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    persist(&path, &temporary, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("asagiri-login-config-{}-{}",
                std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn config(&self) -> PathBuf { self.0.join("config.json") }
    }
    impl Drop for Directory {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn malformed_configs_are_preserved() {
        let dir = Directory::new();
        for bytes in [b"{broken".as_slice(), b"[]", br#"{"auths":"invalid","currentContext":"keep"}"#] {
            fs::write(dir.config(), bytes).unwrap();
            assert!(edit(&dir.config(), |value| {
                auths(value)?.insert("example.test".into(), json!({"auth":"fake"}));
                Ok(())
            }).is_err());
            assert_eq!(fs::read(dir.config()).unwrap(), bytes);
            assert!(!fs::read_dir(&dir.0).unwrap().any(|e| e.unwrap().path().extension().is_some_and(|e| e == "tmp")));
        }
    }

    #[test]
    fn concurrent_edits_retain_every_registry_and_other_settings() {
        let dir = Directory::new();
        fs::write(dir.config(), br#"{"currentContext":"asagiri","plugins":{"example":{"enabled":true}}}"#).unwrap();
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let path = dir.config();
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for entry in 0..8 {
                        edit(&path, |value| {
                            auths(value)?.insert(format!("worker-{worker}-{entry}.test"), json!({"auth":"fake"}));
                            Ok(())
                        }).unwrap();
                    }
                });
            }
        });
        let value = read(&dir.config()).unwrap();
        assert_eq!(value["auths"].as_object().unwrap().len(), 64);
        assert_eq!(value["currentContext"], "asagiri");
        assert_eq!(value["plugins"]["example"]["enabled"], true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(dir.config()).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn failed_rename_removes_only_the_new_temporary_file() {
        let dir = Directory::new();
        fs::create_dir(dir.config()).unwrap();
        let temporary = dir.0.join("owned.tmp");
        assert!(persist(&dir.config(), &temporary, b"fake credentials").is_err());
        assert!(dir.config().is_dir());
        assert!(!temporary.exists());
    }

    #[cfg(unix)]
    #[test]
    fn temporary_symlinks_are_rejected_and_config_symlinks_are_preserved() {
        use std::os::unix::fs::symlink;
        let dir = Directory::new();
        let target = dir.0.join("actual.json");
        fs::write(&target, br#"{"currentContext":"keep"}"#).unwrap();
        let temporary = dir.0.join("unowned.tmp");
        symlink(&target, &temporary).unwrap();
        assert_eq!(persist(&dir.config(), &temporary, b"fake credentials").unwrap_err().kind(),
                   io::ErrorKind::AlreadyExists);
        assert!(fs::symlink_metadata(&temporary).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), br#"{"currentContext":"keep"}"#);
        symlink("actual.json", dir.config()).unwrap();
        edit(&dir.config(), |value| {
            auths(value)?.insert("example.test".into(), json!({"auth":"fake"}));
            Ok(())
        }).unwrap();
        assert!(fs::symlink_metadata(dir.config()).unwrap().file_type().is_symlink());
        assert_eq!(read(&target).unwrap()["currentContext"], "keep");
        assert_eq!(read(&target).unwrap()["auths"]["example.test"]["auth"], "fake");
        fs::remove_file(&target).unwrap();
        assert!(edit(&dir.config(), |_| Ok(())).is_err());
        assert!(fs::symlink_metadata(dir.config()).unwrap().file_type().is_symlink());
        assert!(!target.exists());
    }
}
