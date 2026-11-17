//! Detach a removed volume's name before reclaiming its files off the control
//! loop. A newly created volume with that name can never be deleted by an old
//! worker. Interrupted reclamation resumes when the agent starts again.
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Instant;
use super::{container, removal::Removals};

const PREFIX: &str = ".removing-";

pub(super) struct Volumes {
    root: PathBuf,
    next: u64,
    workers: Removals,
}

impl Volumes {
    pub fn new(root: &Path) -> io::Result<Self> {
        let mut this = Self { root: root.to_owned(), next: 0, workers: Removals::default() };
        match fs::read_dir(root) {
            Ok(entries) => for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue; };
                if garbage_name(name) && entry.file_type()?.is_dir() { this.workers.enqueue(name); }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error),
        }
        Ok(this)
    }

    pub fn remove(&mut self, name: &str) -> io::Result<()> {
        self.detach(name, |data| super::volmount::release(&data.to_string_lossy()))
    }

    fn detach(&mut self, name: &str, release: impl FnOnce(&Path) -> bool) -> io::Result<()> {
        if !name.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad volume name"));
        }
        let source = self.root.join(name);
        match fs::symlink_metadata(&source) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
            Ok(_) => {},
        }
        // An NFS export or host bind is not ours to delete. Preserve the old
        // refusal if it cannot be unmounted before the rename.
        if !release(&source.join("_data")) {
            return Err(io::Error::other("volume is still mounted, not deleting it"));
        }
        let (id, trash) = loop {
            let id = format!("{PREFIX}{}-{}", std::process::id(), self.next);
            self.next += 1;
            let trash = self.root.join(&id);
            match fs::DirBuilder::new().mode(0o700).create(&trash) {
                Ok(()) => break (id, trash),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        if let Err(error) = fs::rename(&source, trash.join("volume")) {
            let _ = fs::remove_dir(&trash);
            return Err(error);
        }
        self.workers.enqueue(&id);
        Ok(())
    }

    pub fn start(&mut self, now: Instant) {
        let root = &self.root;
        self.workers.start(now, |id| container::begin_cleanup(|| fs::remove_dir_all(root.join(id))));
    }

    pub fn needs_retry_poll(&self) -> bool { self.workers.needs_retry_poll() }
    pub fn reaped(&mut self, pid: libc::pid_t, code: i32, now: Instant) -> bool {
        self.workers.reaped(pid, code, now)
    }
}

fn garbage_name(name: &str) -> bool {
    name.strip_prefix(PREFIX).and_then(|rest| rest.split_once('-'))
        .is_some_and(|(pid, count)| !pid.is_empty() && !count.is_empty()
            && pid.bytes().all(|b| b.is_ascii_digit()) && count.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("asagiri-volume-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            fs::create_dir_all(path.join("reuse/_data")).unwrap();
            fs::write(path.join("reuse/_data/old"), b"old").unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

    #[test]
    fn deletion_cannot_reach_a_recreated_volume_and_recovers_after_restart() {
        let f = Fixture::new();
        let mut volumes = Volumes::new(&f.0).unwrap();
        volumes.detach("reuse", |_| true).unwrap();
        assert!(!f.0.join("reuse").exists());
        fs::create_dir_all(f.0.join("reuse/_data")).unwrap();
        fs::write(f.0.join("reuse/_data/new"), b"new").unwrap();
        drop(volumes);
        let recovered = Volumes::new(&f.0).unwrap();
        let pending: Vec<_> = recovered.workers.ids().collect();
        assert_eq!(pending.len(), 1);
        assert!(f.0.join(pending[0]).join("volume/_data/old").is_file());
        fs::remove_dir_all(f.0.join(pending[0])).unwrap();
        assert_eq!(fs::read(f.0.join("reuse/_data/new")).unwrap(), b"new");
    }

    #[test]
    fn mounted_data_and_invalid_names_are_preserved() {
        let f = Fixture::new();
        let mut volumes = Volumes::new(&f.0).unwrap();
        assert!(volumes.detach("reuse", |_| false).is_err());
        for name in ["../reuse", ".", ".removing-1-0", ""] {
            assert!(volumes.detach(name, |_| panic!("invalid name reached unmount")).is_err());
        }
        assert!(f.0.join("reuse/_data/old").is_file());
        assert!(volumes.workers.is_empty());
    }
}
