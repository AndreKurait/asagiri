//! Validated daemon.json registry settings. The old mirror file is used only
//! when daemon.json omits registry-mirrors. API writes preserve every other
//! setting and atomically replace one canonical file before applying it.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

use asagiri_image::{Mirror, RegistrySettings};
use serde_json::{Value, json};

const MAX_BYTES: u64 = 64 << 10;
const KEYS: [&str; 4] = ["registry-mirrors", "insecure-registries", "max-concurrent-downloads", "max-concurrent-uploads"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub settings: RegistrySettings,
    pub mirrors: Vec<Mirror>,
}

fn strings(value: &Value, key: &str, max: usize) -> Result<Vec<String>, String> {
    let list = value.as_array().ok_or_else(|| format!("{key} must be a list of strings"))?;
    if list.len() > max { return Err(format!("at most {max} {key}")); }
    list.iter().map(|v| v.as_str().map(str::to_owned).ok_or_else(|| format!("{key} must contain only strings"))).collect()
}

impl Config {
    fn parse(value: &Value, legacy: &[Mirror]) -> Result<Self, String> {
        let map = value.as_object().ok_or("daemon config must be an object")?;
        if let Some(key) = map.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return Err(format!("unsupported daemon setting: {key}"));
        }
        let count = |key: &str, default: usize| -> Result<usize, String> {
            match map.get(key) {
                None => Ok(default),
                Some(v) => v.as_u64().filter(|v| (1..=16).contains(v)).map(|v| v as usize)
                    .ok_or_else(|| format!("{key} must be an integer from 1 to 16")),
            }
        };
        let insecure = map.get("insecure-registries").map(|v| strings(v, "insecure-registries", 64)).transpose()?.unwrap_or_default();
        let settings = RegistrySettings::new(&insecure, count("max-concurrent-downloads", 4)?, count("max-concurrent-uploads", 5)?)?;
        let mirrors = match map.get("registry-mirrors") {
            None => legacy.to_vec(),
            Some(value) => {
                let mut mirrors = Vec::new();
                for text in strings(value, "registry-mirrors", 16)? {
                    let mirror = Mirror::parse_with(&text, &settings)?;
                    if !mirrors.contains(&mirror) { mirrors.push(mirror); }
                }
                mirrors
            }
        };
        Ok(Self { settings, mirrors })
    }

    pub fn report(&self) -> Value {
        json!({
            "registry-mirrors": self.mirrors.iter().map(Mirror::as_url).collect::<Vec<_>>(),
            "insecure-registries": self.settings.insecure_registries(),
            "max-concurrent-downloads": self.settings.max_downloads(),
            "max-concurrent-uploads": self.settings.max_uploads(),
        })
    }
}

fn read_json(path: &Path) -> io::Result<Option<Value>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES { return Err(io::Error::new(io::ErrorKind::InvalidData, "daemon config exceeds 64 KiB")); }
    serde_json::from_slice(&bytes).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

struct Saved {
    path: PathBuf,
    config: Config,
}

impl Saved {
    fn load(dir: &Path) -> io::Result<Self> {
        let path = dir.join("daemon.json");
        let value = read_json(&path)?.unwrap_or_else(|| json!({}));
        let legacy = if value.get("registry-mirrors").is_none() {
            // Keep the old reader's tolerance for stale entries. An explicitly
            // configured daemon.json is strict and never silently defaults.
            read_json(&dir.join("registry-mirrors.json")).ok().flatten().unwrap_or(Value::Null)["mirrors"]
                .as_array().into_iter().flatten().filter_map(Value::as_str).filter_map(|s| Mirror::parse(s).ok()).take(16).collect()
        } else { Vec::new() };
        let config = Config::parse(&value, &legacy).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))?;
        Ok(Self { path, config })
    }

    fn update(&mut self, patch: &Value) -> Result<Value, String> {
        let patch = patch.as_object().ok_or("daemon config must be an object")?;
        let mut value = self.config.report();
        for (key, value_patch) in patch { value[key] = value_patch.clone(); }
        let config = Config::parse(&value, &[])?;
        if config != self.config {
            persist(&self.path, &config.report()).map_err(|e| format!("saving daemon.json: {e}"))?;
            self.config = config;
        }
        Ok(self.config.report())
    }
}

fn persist(path: &Path, value: &Value) -> io::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or_else(|| io::Error::other("config has no parent"))?;
    let temporary = parent.join(format!(".daemon.json.{}.{}.tmp", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed)));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&temporary); }
    result
}

static SAVED: OnceLock<Mutex<Saved>> = OnceLock::new();

pub fn initialize() -> io::Result<()> {
    let saved = Saved::load(&super::state_dir()?)?;
    SAVED.set(Mutex::new(saved)).map_err(|_| io::Error::other("registry config already initialized"))
}

pub fn snapshot() -> Config {
    SAVED.get().map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).config.clone()).unwrap_or_default()
}

pub fn image_settings() -> (RegistrySettings, Vec<Mirror>) {
    let config = snapshot();
    (config.settings, config.mirrors)
}

pub fn set(patch: &Value) -> Result<Value, String> {
    SAVED.get().ok_or("registry settings have not been initialized")?.lock().unwrap_or_else(|e| e.into_inner()).update(patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("asagiri-config-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn legacy_mirrors_migrate_without_losing_other_settings() {
        let dir = dir();
        std::fs::write(dir.join("registry-mirrors.json"), r#"{"mirrors":["https://mirror.example"]}"#).unwrap();
        std::fs::write(dir.join("daemon.json"), r#"{"max-concurrent-downloads":2}"#).unwrap();
        let mut saved = Saved::load(&dir).unwrap();
        assert_eq!(saved.config.mirrors[0].host, "mirror.example");
        saved.update(&json!({"max-concurrent-uploads": 1})).unwrap();
        let report = Saved::load(&dir).unwrap().config.report();
        assert_eq!(report["registry-mirrors"], json!(["https://mirror.example"]));
        assert_eq!(report["max-concurrent-downloads"], 2);
        assert_eq!(report["max-concurrent-uploads"], 1);
        saved.update(&json!({"registry-mirrors": []})).unwrap();
        assert!(Saved::load(&dir).unwrap().config.mirrors.is_empty());
        assert_eq!(saved.config.settings.max_downloads(), 2);
        assert!(dir.join("registry-mirrors.json").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_and_failed_updates_preserve_the_saved_and_active_config() {
        let dir = dir();
        let mut saved = Saved::load(&dir).unwrap();
        saved.update(&json!({"max-concurrent-downloads": 2})).unwrap();
        let original = std::fs::read(&saved.path).unwrap();
        for patch in [json!({"max-concurrent-downloads":0}), json!({"max-concurrent-uploads":1.5}),
            json!({"insecure-registries":[false]}), json!({"registry-mirrors":["http://registry.example"]}),
            json!({"insecure-registries":["https://registry.example"]}), json!({"unknown":true}), json!([])] {
            assert!(saved.update(&patch).is_err(), "{patch}");
            assert_eq!(std::fs::read(&saved.path).unwrap(), original);
            assert_eq!(saved.config.settings.max_downloads(), 2);
        }
        let missing = dir.join("missing").join("daemon.json");
        saved.path = missing;
        assert!(saved.update(&json!({"max-concurrent-downloads":3})).is_err());
        assert_eq!(saved.config.settings.max_downloads(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn strict_startup_and_scoped_http_mirror_validation() {
        let dir = dir();
        std::fs::write(dir.join("daemon.json"), b"{").unwrap();
        assert!(Saved::load(&dir).is_err());
        std::fs::write(dir.join("daemon.json"), vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(Saved::load(&dir).is_err());
        let config = Config::parse(&json!({"insecure-registries":["registry.example:5000"], "registry-mirrors":["http://registry.example:5000"]}), &[]).unwrap();
        assert_eq!(config.mirrors[0].as_url(), "http://registry.example:5000");
        assert!(Config::parse(&json!({"insecure-registries":["registry.example:5001"], "registry-mirrors":["http://registry.example:5000"]}), &[]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
