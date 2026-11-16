//! Content-addressed blob store and the image index on the host.
//!
//! Layout under the data directory:
//!
//! ```text
//! blobs/sha256/<hex>     compressed layers and configs, exactly as the registry served them
//! images.json            the image index
//! tmp/                   in-flight downloads, renamed into blobs/ once verified
//! ```

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The most an image config may be: real ones are a few KB.
pub const MAX_CONFIG: u64 = 16 << 20;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::reference::Reference;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Layer {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
}

/// What a container needs from the image config to start.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunConfig {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub working_dir: String,
    #[serde(default)]
    pub user: String,
    /// The image's `config.Labels`, sorted so two loads of one image agree.
    /// None identifies an older index: its labels still live in the config blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<std::collections::BTreeMap<String, String>>,
}

impl RunConfig {
    /// The labels as the Docker Engine API shows them: the map for an image
    /// that has labels, `null` for one without, as Docker's inspect does for
    /// an image with no LABEL (alpine, say).
    pub fn labels_json(&self) -> serde_json::Value {
        self.labels.as_ref().filter(|labels| !labels.is_empty())
            .map_or(serde_json::Value::Null, |labels| serde_json::json!(labels))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Image {
    /// Short reference, e.g. `alpine:latest`.
    pub name: String,
    /// Digest of the platform manifest.
    pub digest: String,
    /// Digest of the multi-arch index the manifest was picked from, when the
    /// registry served one. Docker lists it in RepoDigests, and a pull by
    /// that digest must find the image by it.
    #[serde(default)]
    pub index_digest: Option<String>,
    /// This name was made locally, even if another tag in its repository
    /// has a registry digest. A pull or push of this exact name clears it.
    /// Old records lack tag provenance; leave their registry tags to the
    /// registry rather than hiding other platforms behind one local image.
    #[serde(default)]
    pub local_tag: bool,
    pub config_digest: String,
    pub layers: Vec<Layer>,
    /// Compressed size of all layers, which is what `docker images` reports too.
    pub size: u64,
    pub platform: String,
    /// When the image was built, as written in its config (RFC 3339).
    #[serde(default)]
    pub created: String,
    /// Unix seconds when this image was pulled.
    pub pulled_at: u64,
    pub config: RunConfig,
}

impl Image {
    /// The repository part of `name`: `alpine`, `ghcr.io/o/r`.
    pub fn repo(&self) -> &str {
        if let Some((repo, _)) = self.name.split_once('@') {
            return repo;
        }
        match self.name.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => repo,
            _ => &self.name,
        }
    }

    /// `repo@digest` as Docker shows it, by the index's digest if there was one.
    /// None for an image no registry has yet (built, loaded, committed, tagged
    /// for another repository): its digest is its config's, which a manifest's
    /// can never be, as the manifest holds it.
    pub fn repo_digest(&self) -> Option<String> {
        let d = self.index_digest.as_deref().unwrap_or(&self.digest);
        (d != self.config_digest).then(|| format!("{}@{d}", self.repo()))
    }

    /// This image under another name (`docker tag`). Its registry digests
    /// belong to its repository: in another one it has none until a push.
    pub fn tagged(&self, name: String) -> Image {
        let mut i = Image { name, ..self.clone() };
        if i.name != self.name {
            i.local_tag = true;
        }
        if i.repo() != self.repo() {
            i.digest = self.config_digest.clone();
            i.index_digest = None;
        }
        i
    }

    /// True if `r` names this image: its name, or a digest of its repository
    /// that is the image's manifest or the index it came from.
    pub fn matches(&self, r: &Reference) -> bool {
        if !r.is_digest() {
            return self.name == r.short();
        }
        let same_repo = Reference::parse(&self.name).is_ok_and(|n| n.registry == r.registry && n.repository == r.repository);
        same_repo && (self.digest == r.reference || self.index_digest.as_deref() == Some(r.reference.as_str()))
    }
}

#[derive(Debug)]
pub struct Store {
    root: PathBuf,
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// The filesystem-safe hexadecimal part of a SHA-256 digest.
pub fn digest_hex(digest: &str) -> io::Result<&str> {
    let h = digest.strip_prefix("sha256:").unwrap_or_default();
    if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("invalid sha256 digest {digest:?}")));
    }
    Ok(h)
}

impl Store {
    /// The per-user data directory: `~/Library/Application Support/Asagiri` on
    /// macOS, `$XDG_DATA_HOME/asagiri` (or `~/.local/share/asagiri`) elsewhere.
    /// `ASAGIRI_DATA_DIR` overrides both.
    pub fn default_root() -> io::Result<PathBuf> {
        if let Some(dir) = std::env::var_os("ASAGIRI_DATA_DIR") {
            return Ok(PathBuf::from(dir));
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| io::Error::other("HOME is not set"))?;
        Ok(if cfg!(target_os = "macos") {
            home.join("Library/Application Support/Asagiri")
        } else if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            PathBuf::from(xdg).join("asagiri")
        } else {
            home.join(".local/share/asagiri")
        })
    }

    pub fn open(root: impl Into<PathBuf>) -> io::Result<Store> {
        let root = root.into();
        fs::create_dir_all(root.join("blobs/sha256"))?;
        fs::create_dir_all(root.join("tmp"))?;
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// An image config, read whole for parsing. Configs are small JSON; a
    /// registry could serve a huge blob under that name, so anything over
    /// `MAX_CONFIG` is refused rather than loaded into memory.
    pub fn read_config(&self, digest: &str) -> std::io::Result<Vec<u8>> {
        let path = self.blob_path(digest)?;
        let len = std::fs::metadata(&path)?.len();
        if len > MAX_CONFIG {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("image config {digest} is {len} bytes; the limit is {MAX_CONFIG}")));
        }
        std::fs::read(path)
    }

    pub fn blob_path(&self, digest: &str) -> io::Result<PathBuf> {
        Ok(self.root.join("blobs/sha256").join(digest_hex(digest)?))
    }

    /// True if the blob is present with the expected size. Blobs are only
    /// renamed into place after their digest was verified, so presence is proof.
    pub fn has_blob(&self, digest: &str, size: u64) -> bool {
        self.blob_path(digest).is_ok_and(|p| fs::metadata(p).is_ok_and(|m| m.len() == size))
    }

    pub fn blob_writer(&self, digest: &str) -> io::Result<BlobWriter> {
        let tmp = self.root.join("tmp").join(format!("{}.{}.{}", digest_hex(digest)?, std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        Ok(BlobWriter {
            file: File::create(&tmp)?,
            hasher: Sha256::new(),
            tmp,
            dest: self.blob_path(digest)?,
            expected: digest.to_owned(),
            written: 0,
        })
    }

    /// A writer for a blob whose digest is not known yet (`docker load`):
    /// it is named after what was written once `finish` is called.
    pub fn ingest(&self) -> io::Result<Ingest> {
        let tmp = self.root.join("tmp").join(format!("ingest.{}.{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        Ok(Ingest { file: File::create(&tmp)?, hasher: Sha256::new(), tmp, root: self.root.clone(), written: 0 })
    }

    pub fn images(&self) -> io::Result<Vec<Image>> {
        match self.read_index() {
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let _index = self.lock_index()?;
                self.read_or_repair_index()
            }
            result => result,
        }
    }

    fn read_index(&self) -> io::Result<Vec<Image>> {
        match fs::read(self.root.join("images.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Separate handles, including another process using this store, share
    /// one lock for the entire read/modify/replace operation.
    fn lock_index(&self) -> io::Result<File> {
        let file = File::options().read(true).write(true).create(true).truncate(false).open(self.root.join("images.lock"))?;
        file.lock()?;
        Ok(file)
    }

    /// Called under the index lock. Old concurrent writers could leave the
    /// tail of a longer document after a complete array. Recover only that
    /// complete, typed array, and preserve every original byte in a backup.
    fn read_or_repair_index(&self) -> io::Result<Vec<Image>> {
        let path = self.root.join("images.json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        match serde_json::from_slice(&bytes) {
            Ok(images) => Ok(images),
            Err(error) => {
                let mut records = serde_json::Deserializer::from_slice(&bytes).into_iter::<Vec<Image>>();
                let Some(Ok(images)) = records.next() else {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: {error}; the original index is unchanged", path.display())));
                };
                let backup = self.root.join(format!("images.json.corrupt.{}.{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
                let mut file = File::options().write(true).create_new(true).open(&backup)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                self.write_index(&images)?;
                eprintln!("Asagiri recovered the image index; the original is saved at {}", backup.display());
                Ok(images)
            }
        }
    }

    pub fn find(&self, name: &str) -> io::Result<Option<Image>> {
        let r = Reference::parse(name).ok();
        Ok(self.images()?.into_iter().find(|i| i.name == name || i.digest == name || r.as_ref().is_some_and(|r| i.matches(r))))
    }

    /// Existing indexes predate cached labels. Read their immutable config
    /// instead of silently treating a previously pulled image as unlabelled.
    pub fn image_labels(&self, image: &Image) -> io::Result<serde_json::Value> {
        if image.config.labels.is_some() {
            return Ok(image.config.labels_json());
        }
        let stored: serde_json::Value = serde_json::from_slice(&self.read_config(&image.config_digest)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let labels = stored["config"]["Labels"].as_object().map(|labels| labels.iter()
            .filter(|(_, value)| value.is_string()).map(|(key, value)| (key.clone(), value.clone()))
            .collect::<serde_json::Map<_, _>>()).unwrap_or_default();
        Ok(if labels.is_empty() { serde_json::Value::Null } else { labels.into() })
    }

    /// Adds or replaces a name, retaining a displaced image's config by ID.
    pub fn put_image(&self, image: Image) -> io::Result<()> {
        let _index = self.lock_index()?;
        let mut images = self.read_or_repair_index()?;
        let displaced = images.iter().find(|i| i.name == image.name && i.config_digest != image.config_digest).cloned();
        // A config that gains a name no longer needs its dangling record.
        images.retain(|i| i.name != image.name && !(i.name == i.config_digest && i.config_digest == image.config_digest));
        if let Some(old) = displaced
            && !images.iter().any(|i| i.config_digest == old.config_digest)
        {
            // Containers still refer to this config and its layers. Existing
            // aliases retain their registry provenance; without one, keep an
            // untagged record. tagged() clears digests belonging to the former
            // repository instead of attributing them to the hash-only name.
            images.push(old.tagged(old.config_digest.clone()));
        }
        images.push(image);
        self.write_index(&images)
    }

    /// Records the manifest a push of `name` left in its registry, so
    /// RepoDigests names what the registry has, as Docker's does: deploy tools
    /// pin to it. Every tag of the image in that repository gets it, as there.
    /// Not if `name` became another image during the push.
    pub fn set_pushed(&self, name: &str, config_digest: &str, digest: &str) -> io::Result<()> {
        let _index = self.lock_index()?;
        let mut images = self.read_or_repair_index()?;
        let Some(repo) = images.iter().find(|i| i.name == name && i.config_digest == config_digest).map(|i| i.repo().to_owned()) else { return Ok(()) };
        for i in images.iter_mut().filter(|i| i.config_digest == config_digest && i.repo() == repo) {
            // Publishing one tag does not publish the other local names.
            // Preserve that distinction independently of RepoDigests.
            i.local_tag = i.name != name && (i.local_tag || i.repo_digest().is_none());
            i.digest = digest.to_owned();
            i.index_digest = None;
        }
        self.write_index(&images)
    }

    /// Removes the image and deletes blobs no remaining image uses.
    pub fn remove_image(&self, name: &str) -> io::Result<Option<Image>> {
        let _index = self.lock_index()?;
        let mut images = self.read_or_repair_index()?;
        let Some(pos) = images.iter().position(|i| i.name == name) else { return Ok(None) };
        let removed = images.remove(pos);
        self.write_index(&images)?;
        let in_use = |d: &str| images.iter().any(|i| i.config_digest == d || i.layers.iter().any(|l| l.digest == d));
        for d in std::iter::once(&removed.config_digest).chain(removed.layers.iter().map(|l| &l.digest)) {
            if !in_use(d)
                && let Ok(path) = self.blob_path(d)
            {
                let _ = fs::remove_file(path);
            }
        }
        Ok(Some(removed))
    }

    // Called only while the image-index lock is held. Readers see either the
    // previous complete index or this complete replacement after rename.
    fn write_index(&self, images: &[Image]) -> io::Result<()> {
        let tmp = self.root.join(format!("tmp/images.{}.{}.json", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        let bytes = serde_json::to_vec_pretty(images).map_err(io::Error::other)?;
        let result = (|| {
            let mut file = File::options().write(true).create_new(true).open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&tmp, self.root.join("images.json"))
        })();
        if result.is_err() { let _ = fs::remove_file(&tmp); }
        result
    }
}

/// Writes a blob to a temporary file, hashing as it goes, and only moves it
/// into the store if the digest matches.
#[derive(Debug)]
pub struct BlobWriter {
    file: File,
    hasher: Sha256,
    tmp: PathBuf,
    dest: PathBuf,
    expected: String,
    written: u64,
}

impl Write for BlobWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl BlobWriter {
    /// Verifies the digest and publishes the blob. Returns bytes written.
    pub fn finish(mut self) -> io::Result<u64> {
        let actual = format!("sha256:{}", hex(&std::mem::take(&mut self.hasher).finalize()));
        if actual != self.expected {
            let _ = fs::remove_file(&self.tmp);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("digest mismatch: expected {}, got {actual}", self.expected),
            ));
        }
        self.file.sync_all()?;
        fs::rename(&self.tmp, &self.dest)?;
        Ok(self.written)
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        // A writer dropped without finish() leaves no partial blob behind.
        // After a successful finish() the tmp path no longer exists.
        let _ = fs::remove_file(&self.tmp);
    }
}

/// See [`Store::ingest`].
#[derive(Debug)]
pub struct Ingest {
    file: File,
    hasher: Sha256,
    tmp: PathBuf,
    root: PathBuf,
    written: u64,
}

impl Write for Ingest {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Ingest {
    /// Publishes the blob under its digest. Returns (digest, size).
    pub fn finish(mut self) -> io::Result<(String, u64)> {
        let hex = hex(&std::mem::take(&mut self.hasher).finalize());
        self.file.sync_all()?;
        fs::rename(&self.tmp, self.root.join("blobs/sha256").join(&hex))?;
        Ok((format!("sha256:{hex}"), self.written))
    }
}

impl Drop for Ingest {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.tmp);
    }
}

pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_image(name: String) -> Image {
        Image {
            name, digest: format!("sha256:{}", "1".repeat(64)), config_digest: format!("sha256:{}", "2".repeat(64)),
            index_digest: None, local_tag: true, layers: Vec::new(), size: 0, platform: "linux/arm64".into(),
            created: String::new(), pulled_at: 0, config: RunConfig::default(),
        }
    }

    fn retention_store() -> Store {
        let root = std::env::temp_dir().join(format!("asagiri-image-retention-{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        Store::open(root).unwrap()
    }

    fn retention_image(store: &Store, arch: &str) -> Image {
        let ingest = |bytes: &[u8]| {
            let mut writer = store.ingest().unwrap();
            writer.write_all(bytes).unwrap();
            writer.finish().unwrap()
        };
        let config = format!(r#"{{"architecture":"{arch}","os":"linux","config":{{}}}}"#);
        let (config_digest, _) = ingest(config.as_bytes());
        let layers: Vec<_> = [b"shared-layer".as_slice(), arch.as_bytes()].into_iter().map(|bytes| {
            let (digest, size) = ingest(bytes);
            Layer { digest, size, media_type: "application/vnd.oci.image.layer.v1.tar".into() }
        }).collect();
        Image {
            name: "registry.example/java:17".into(),
            digest: sha256_digest(format!("manifest-{arch}").as_bytes()),
            index_digest: Some(sha256_digest(b"multiarch-index")),
            local_tag: false,
            config_digest,
            size: layers.iter().map(|l| l.size).sum(),
            layers,
            platform: format!("linux/{arch}"),
            created: String::new(),
            pulled_at: 1,
            config: RunConfig::default(),
        }
    }

    #[test]
    fn retention_tag_move_keeps_old_identity_and_shared_blobs() {
        let store = retention_store();
        let old = retention_image(&store, "amd64");
        let new = retention_image(&store, "arm64");
        store.put_image(old.clone()).unwrap();
        store.put_image(new.clone()).unwrap();
        assert_eq!(store.find(&new.name).unwrap(), Some(new.clone()));
        let retained = store.find(&old.config_digest).unwrap().expect("a displaced image ID must remain inspectable");
        assert_eq!(retained.platform, old.platform);
        assert_eq!(retained.layers, old.layers);
        assert_eq!(retained.config, old.config);
        assert_eq!(retained.name, old.config_digest);
        assert_eq!(retained.repo_digest(), None, "a hash-only name must not invent a repository digest");
        assert_eq!(retained.digest, old.config_digest);
        assert_eq!(retained.index_digest, None);
        assert!(retained.local_tag);
        let config: serde_json::Value = serde_json::from_slice(&store.read_config(&old.config_digest).unwrap()).unwrap();
        assert_eq!(config["architecture"], "amd64");
        store.remove_image(&new.name).unwrap();
        for layer in &old.layers {
            assert!(store.has_blob(&layer.digest, layer.size), "retained image layer was removed");
        }
        assert!(!store.blob_path(&new.config_digest).unwrap().exists());
        assert!(!store.blob_path(&new.layers[1].digest).unwrap().exists());
        store.remove_image(&old.config_digest).unwrap();
        assert!(store.images().unwrap().is_empty());
        assert!(!store.blob_path(&old.config_digest).unwrap().exists());
        for layer in &old.layers {
            assert!(!store.blob_path(&layer.digest).unwrap().exists());
        }
        fs::remove_dir_all(store.root()).unwrap();
    }

    #[test]
    fn retention_existing_alias_keeps_its_registry_provenance() {
        let store = retention_store();
        let old = retention_image(&store, "amd64");
        let alias = old.tagged("registry.example/java:retained".into());
        let new = retention_image(&store, "arm64");
        store.put_image(old.clone()).unwrap();
        store.put_image(alias.clone()).unwrap();
        store.put_image(new.clone()).unwrap();
        let images = store.images().unwrap();
        assert_eq!(images.len(), 2, "an existing alias already retains the old config");
        assert_eq!(store.find(&alias.name).unwrap(), Some(alias.clone()));
        assert_eq!(alias.repo_digest(), old.repo_digest());
        assert!(images.iter().all(|i| i.name != i.config_digest));
        assert_eq!(store.find(&new.name).unwrap(), Some(new));
        fs::remove_dir_all(store.root()).unwrap();
    }

    #[test]
    fn retention_same_config_refresh_does_not_create_dangling_copy() {
        let store = retention_store();
        let old = retention_image(&store, "amd64");
        store.put_image(old.clone()).unwrap();
        let updated = Image { pulled_at: 2, ..old };
        store.put_image(updated.clone()).unwrap();
        assert_eq!(store.images().unwrap(), vec![updated]);
        fs::remove_dir_all(store.root()).unwrap();
    }

    #[test]
    fn retention_moving_tag_back_removes_redundant_dangling_copy() {
        let store = retention_store();
        let first = retention_image(&store, "amd64");
        let second = retention_image(&store, "arm64");
        store.put_image(first.clone()).unwrap();
        store.put_image(second.clone()).unwrap();
        store.put_image(first.clone()).unwrap();
        store.put_image(first.clone()).unwrap();
        let images = store.images().unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images.iter().filter(|i| i.config_digest == first.config_digest).count(), 1);
        assert_eq!(store.find(&first.name).unwrap(), Some(first.clone()));
        assert_eq!(store.find(&second.config_digest).unwrap().unwrap().platform, second.platform);
        assert_eq!(store.find(&first.name).unwrap().unwrap().repo_digest(), first.repo_digest());
        fs::remove_dir_all(store.root()).unwrap();
    }

    #[test]
    fn retention_tagging_an_untagged_image_replaces_its_dangling_record() {
        let store = retention_store();
        let named = retention_image(&store, "amd64");
        store.put_image(named.tagged(named.config_digest.clone())).unwrap();
        store.put_image(named.clone()).unwrap();
        assert_eq!(store.images().unwrap(), vec![named]);
        fs::remove_dir_all(store.root()).unwrap();
    }

    #[test]
    fn retention_concurrent_tag_replacement_keeps_every_config() {
        let store = retention_store();
        let images: Vec<_> = (0..8).map(|n| retention_image(&store, &format!("platform-{n}"))).collect();
        let barrier = std::sync::Barrier::new(images.len());
        std::thread::scope(|scope| {
            for image in images.iter().cloned() {
                let (root, barrier) = (store.root(), &barrier);
                scope.spawn(move || {
                    let handle = Store::open(root).unwrap();
                    barrier.wait();
                    handle.put_image(image).unwrap();
                });
            }
        });
        let indexed = store.images().unwrap();
        assert_eq!(indexed.len(), images.len());
        for image in &images {
            assert_eq!(indexed.iter().filter(|i| i.config_digest == image.config_digest).count(), 1);
        }
        assert_eq!(indexed.iter().filter(|i| i.name == images[0].name).count(), 1);
        fs::remove_dir_all(store.root()).unwrap();
    }

    #[test]
    fn independent_store_handles_preserve_concurrent_updates_and_shorter_rewrites() {
        let root = std::env::temp_dir().join(format!("asagiri-store-concurrent-{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        let store = Store::open(&root).unwrap();
        let start = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let (root, start) = (&root, &start);
                scope.spawn(move || {
                    let store = Store::open(root).unwrap();
                    start.wait();
                    for n in 0..8 {
                        let name = format!("worker-{worker}:tag-{n}");
                        let mut image = index_image(name.clone());
                        image.config.env = vec!["LONG=".to_owned() + &"x".repeat(1024)];
                        store.put_image(image).unwrap();
                        store.put_image(index_image(name)).unwrap();
                        assert!(!store.images().unwrap().is_empty());
                    }
                });
            }
        });
        let images = store.images().unwrap();
        assert_eq!(images.len(), 64, "no completed tag may be lost to another writer");
        assert!(images.iter().all(|i| i.config.env.is_empty()));
        assert_eq!(fs::read_dir(root.join("tmp")).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_complete_index_with_stale_trailing_bytes_is_backed_up_and_recovered() {
        let root = std::env::temp_dir().join(format!("asagiri-store-recover-{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        let store = Store::open(&root).unwrap();
        let mut corrupt = serde_json::to_vec(&vec![index_image("recovered:latest".into())]).unwrap();
        corrupt.extend_from_slice(b",\n      \"user\": \"\"\n    }\n  }\n]");
        fs::write(root.join("images.json"), &corrupt).unwrap();
        assert_eq!(store.images().unwrap()[0].name, "recovered:latest");
        let backups: Vec<_> = fs::read_dir(&root).unwrap().map(Result::unwrap)
            .filter(|e| e.file_name().to_string_lossy().starts_with("images.json.corrupt.")).collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(backups[0].path()).unwrap(), corrupt);
        store.put_image(index_image("new:latest".into())).unwrap();
        assert_eq!(store.images().unwrap().len(), 2);
        // Incomplete records cannot be reconstructed safely; never erase them.
        fs::write(root.join("images.json"), b"[{\"name\":\"incomplete").unwrap();
        assert_eq!(store.put_image(index_image("refused:latest".into())).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(root.join("images.json")).unwrap(), b"[{\"name\":\"incomplete");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn image_index_lock_serializes_processes() {
        const CHILD_ROOT: &str = "ASAGIRI_IMAGE_LOCK_TEST_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let store = Store::open(root).unwrap();
            let _lock = store.lock_index().unwrap();
            fs::write(store.root.join("child-acquired"), b"locked").unwrap();
            return;
        }
        let root = std::env::temp_dir().join(format!("asagiri-store-process-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        let lock = store.lock_index().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "store::tests::image_index_lock_serializes_processes"])
            .env(CHILD_ROOT, &root).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(child.try_wait().unwrap().is_none(), "child acquired a held index lock");
        assert!(!root.join("child-acquired").exists());
        drop(lock);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("child did not acquire released index lock");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(root.join("child-acquired").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_image_writers_preserve_each_image() {
        let root = std::env::temp_dir().join(format!("asagiri-store-concurrent-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        Store::open(&root).unwrap();
        let barrier = std::sync::Barrier::new(16);
        std::thread::scope(|scope| {
            let mut writers = Vec::new();
            for writer in 0..16 {
                let root = &root;
                let barrier = &barrier;
                writers.push(scope.spawn(move || {
                    let store = Store::open(root).unwrap();
                    barrier.wait();
                    for version in 0..8 {
                        store.put_image(Image {
                            name: format!("writer-{writer}:{version}"),
                            digest: format!("sha256:{writer:064x}"),
                            index_digest: None,
                            local_tag: false,
                            config_digest: format!("sha256:{:064x}", writer * 8 + version),
                            layers: Vec::new(), size: 0, platform: "linux/amd64".into(),
                            created: String::new(), pulled_at: 0, config: RunConfig::default(),
                        })?;
                    }
                    Ok::<_, io::Error>(())
                }));
            }
            let results: Vec<_> = writers.into_iter().map(|writer| writer.join().unwrap()).collect();
            assert!(results.iter().all(Result::is_ok), "index writes failed: {results:?}");
        });
        let images = Store::open(&root).unwrap().images().unwrap();
        assert_eq!(images.len(), 128, "concurrent commits lost image entries");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_paths_instead_of_digests() {
        let root = std::env::temp_dir().join(format!("asagiri-store-path-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let outside = root.with_extension("outside");
        fs::write(&outside, b"do not touch").unwrap();
        let path = outside.to_string_lossy().into_owned();
        assert!(store.blob_path(&path).is_err());
        assert!(store.blob_path("sha256:../outside").is_err());
        assert!(!store.has_blob(&path, 12));
        assert!(store.blob_writer(&path).is_err());
        assert!(store.read_config(&path).is_err());

        let image = Image {
            name: "malicious:latest".into(),
            digest: "sha256:manifest".into(),
            index_digest: None,
            local_tag: false,
            config_digest: path.clone(),
            layers: vec![Layer { digest: path, size: 12, media_type: "application/vnd.oci.image.layer.v1.tar".into() }],
            size: 12,
            platform: "linux/amd64".into(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig::default(),
        };
        store.put_image(image).unwrap();
        store.remove_image("malicious:latest").unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"do not touch");
        fs::remove_dir_all(&root).unwrap();
        fs::remove_file(&outside).unwrap();
    }

    #[test]
    fn finds_a_pull_by_index_digest_by_either_digest() {
        let root = std::env::temp_dir().join(format!("asagiri-store-index-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let (index, manifest) = (format!("sha256:{}", "1".repeat(64)), format!("sha256:{}", "2".repeat(64)));
        let image = Image {
            name: format!("kindest/node@{index}"),
            digest: manifest.clone(),
            index_digest: Some(index.clone()),
            local_tag: false,
            config_digest: format!("sha256:{}", "3".repeat(64)),
            layers: Vec::new(),
            size: 0,
            platform: "linux/arm64".into(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig::default(),
        };
        assert_eq!(image.repo(), "kindest/node");
        assert_eq!(image.repo_digest(), Some(format!("kindest/node@{index}")));
        store.put_image(image).unwrap();
        for name in [format!("kindest/node@{index}"), format!("kindest/node:v1.37.0@{index}"), format!("kindest/node@{manifest}"), manifest.clone()] {
            assert!(store.find(&name).unwrap().is_some(), "{name}");
        }
        assert!(store.find(&format!("other/node@{index}")).unwrap().is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_push_leaves_the_registry_manifest_in_repo_digests() {
        let root = std::env::temp_dir().join(format!("asagiri-store-push-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let d = |c: &str| format!("sha256:{}", c.repeat(64));
        let image = |name: &str, config: &str, index: Option<String>| Image {
            name: name.into(),
            digest: d(config),
            index_digest: index,
            local_tag: true,
            config_digest: d(config),
            layers: Vec::new(),
            size: 0,
            platform: "linux/arm64".into(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig::default(),
        };
        // Built here: no registry has it yet, so no RepoDigest, as with Docker.
        store.put_image(image("localhost:5000/built:1", "1", None)).unwrap();
        // The same image's other tags: one in its repository, one in another registry.
        store.put_image(image("localhost:5000/built:latest", "1", None)).unwrap();
        store.put_image(image("localhost:5001/built:1", "1", None)).unwrap();
        // Pulled: alpine's index. Tagged in its repository it keeps that; in
        // another it has none there, rather than a digest that one lacks.
        let pulled = Image { digest: d("8"), local_tag: false, ..image("alpine:latest", "2", Some(d("9"))) };
        assert_eq!(pulled.tagged("alpine:old".into()).repo_digest(), Some(format!("alpine@{}", d("9"))));
        store.put_image(pulled.tagged("localhost:5000/tagged:1".into())).unwrap();
        store.put_image(image("localhost:5000/other:1", "3", None)).unwrap();
        let repo_digest = |n: &str| store.find(n).unwrap().unwrap().repo_digest();
        for n in ["localhost:5000/built:1", "localhost:5000/tagged:1", "localhost:5000/other:1"] {
            assert_eq!(repo_digest(n), None, "{n}");
        }
        store.set_pushed("localhost:5000/built:1", &d("1"), &d("a")).unwrap();
        store.set_pushed("localhost:5000/tagged:1", &d("2"), &d("b")).unwrap();
        // The name now another image: that one keeps its own.
        store.set_pushed("localhost:5000/other:1", &d("4"), &d("c")).unwrap();
        assert_eq!(repo_digest("localhost:5000/built:1"), Some(format!("localhost:5000/built@{}", d("a"))));
        // Pushing one tag gives the repository's other tags of it the digest; elsewhere it has none.
        assert_eq!(repo_digest("localhost:5000/built:latest"), Some(format!("localhost:5000/built@{}", d("a"))));
        assert!(!store.find("localhost:5000/built:1").unwrap().unwrap().local_tag);
        assert!(store.find("localhost:5000/built:latest").unwrap().unwrap().local_tag);
        assert_eq!(repo_digest("localhost:5001/built:1"), None);
        assert_eq!(repo_digest("localhost:5000/tagged:1"), Some(format!("localhost:5000/tagged@{}", d("b"))));
        assert_eq!(repo_digest("localhost:5000/other:1"), None);
        // And a pull by the pushed digest finds the image here.
        assert!(store.find(&format!("localhost:5000/built@{}", d("a"))).unwrap().is_some());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn repo_strips_a_tag_but_not_a_port() {
        let named = |name: &str| Image {
            name: name.into(),
            digest: String::new(),
            index_digest: None,
            local_tag: false,
            config_digest: String::new(),
            layers: Vec::new(),
            size: 0,
            platform: String::new(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig::default(),
        };
        assert_eq!(named("alpine:latest").repo(), "alpine");
        assert_eq!(named("localhost:5000/app:1").repo(), "localhost:5000/app");
        assert_eq!(named("localhost:5000/app").repo(), "localhost:5000/app");
        assert_eq!(named(&format!("kindest/node@sha256:{}", "a".repeat(64))).repo(), "kindest/node");
    }

    #[test]
    fn labels_survive_a_save_and_a_load() {
        let root =
            std::env::temp_dir().join(format!("asagiri-store-labels-test-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let image = Image {
            name: "labelled:1".into(),
            digest: format!("sha256:{}", "1".repeat(64)),
            index_digest: None,
            local_tag: false,
            config_digest: format!("sha256:{}", "2".repeat(64)),
            layers: Vec::new(),
            size: 0,
            platform: "linux/arm64".into(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig {
                labels: Some([("com.example.label".to_owned(), "1".to_owned())]
                    .into_iter()
                    .collect()),
                ..Default::default()
            },
        };
        store.put_image(image).unwrap();
        let labels = store.find("labelled:1").unwrap().unwrap().config.labels.unwrap();
        assert_eq!(
            labels.get("com.example.label").map(String::as_str),
            Some("1")
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn old_indexes_read_labels_from_the_config_without_rewriting_the_index() {
        let root = std::env::temp_dir().join(format!("asagiri-old-labels-{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        let store = Store::open(&root).unwrap();
        let stored = serde_json::json!({"config": {"Labels": {"vendor.feature": "enabled", "empty": ""}}});
        let mut blob = store.ingest().unwrap();
        blob.write_all(&serde_json::to_vec(&stored).unwrap()).unwrap();
        let (digest, _) = blob.finish().unwrap();
        let mut image = crate::archive::from_config("legacy:latest".into(), &digest, &stored, Vec::new());
        image.config.labels = None;
        store.put_image(image.clone()).unwrap();
        let before = fs::read(root.join("images.json")).unwrap();
        let reopened = Store::open(&root).unwrap();
        let image = reopened.find("legacy:latest").unwrap().unwrap();
        assert!(image.config.labels.is_none());
        assert_eq!(reopened.image_labels(&image).unwrap(), stored["config"]["Labels"]);
        assert_eq!(fs::read(root.join("images.json")).unwrap(), before);
        // A known empty map never consults a missing blob or invents labels.
        let mut known = image;
        known.config.labels = Some(Default::default());
        fs::remove_file(reopened.blob_path(&digest).unwrap()).unwrap();
        assert!(reopened.image_labels(&known).unwrap().is_null());
        // Cached nonempty labels also remain usable without rereading a blob.
        known.config.labels = Some([("vendor.feature".into(), "cached".into())].into_iter().collect());
        assert_eq!(reopened.image_labels(&known).unwrap(), serde_json::json!({"vendor.feature": "cached"}));
        known.config.labels = None;
        assert!(reopened.image_labels(&known).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
