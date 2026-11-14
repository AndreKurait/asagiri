//! Browser permissions are independent of MCP, with a private password hash.

use std::io::{self, Read};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::integrations::{self, Permissions};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    pub enabled: bool,
    pub allow_container_changes: bool,
    pub password_hash: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserConfig").field("permissions", &self.permissions())
            .field("password_configured", &self.password_hash.is_some()).finish()
    }
}

impl Config {
    pub fn permissions(&self) -> Permissions {
        Permissions { enabled: self.enabled && self.password_hash.is_some(),
            allow_container_changes: self.enabled && self.allow_container_changes && self.password_hash.is_some() }
    }

    pub fn status(&self) -> Value {
        let permissions = self.permissions();
        json!({"enabled": permissions.enabled, "allowContainerChanges": permissions.allow_container_changes,
            "passwordConfigured": self.password_hash.is_some()})
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    directory: PathBuf,
}

impl Settings {
    pub fn current() -> io::Result<Self> { Ok(Self { directory: integrations::directory()? }) }

    #[cfg(test)]
    pub fn at(directory: PathBuf) -> Self { Self { directory } }

    pub fn load(&self) -> io::Result<Config> {
        let file = match integrations::open(&self.directory.join("web.json"), false) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 { return Err(io::Error::other("browser settings file is too large")); }
        let config: Config = serde_json::from_slice(&bytes)
            .map_err(|_| io::Error::other("invalid browser settings"))?;
        if config.password_hash.as_ref().is_some_and(|hash| hash.len() > 256 || !hash.is_ascii()) {
            return Err(io::Error::other("invalid browser password hash"));
        }
        Ok(config)
    }

    pub fn update(&self, change: impl FnOnce(&mut Config) -> io::Result<()>) -> io::Result<Config> {
        integrations::private_directory(&self.directory)?;
        let lock = integrations::open(&self.directory.join("web.lock"), true)?;
        lock.lock()?;
        let mut config = self.load()?;
        change(&mut config)?;
        if config.enabled && config.password_hash.is_none() {
            return Err(io::Error::other("set a browser password first: asagiri web password"));
        }
        if !config.enabled { config.allow_container_changes = false; }
        let mut bytes = serde_json::to_vec_pretty(&config)?;
        bytes.push(b'\n');
        integrations::private_write(&self.directory.join("web.json"), &bytes)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_permissions_require_a_password_and_do_not_enable_mcp() {
        let directory = std::env::temp_dir().join(format!("asagiri-web-settings-{}", std::process::id()));
        let settings = Settings::at(directory.clone());
        assert_eq!(settings.load().unwrap().permissions(), Permissions::default());
        assert!(settings.update(|c| { c.enabled = true; Ok(()) }).is_err());
        settings.update(|c| {
            c.password_hash = Some("test hash which must never be in status".into());
            c.enabled = true;
            c.allow_container_changes = true;
            Ok(())
        }).unwrap();
        assert!(settings.load().unwrap().permissions().allow_container_changes);
        assert!(!settings.load().unwrap().status().to_string().contains("test hash"));
        assert_eq!(integrations::Settings::at(directory.clone()).load().unwrap(), Permissions::default());
        settings.update(|c| { c.enabled = false; Ok(()) }).unwrap();
        let config = settings.load().unwrap();
        assert_eq!(config.permissions(), Permissions::default());
        assert!(config.password_hash.is_some());
        integrations::private_write(&directory.join("web.json"), b"{\"enabled\":true,\"unknown\":true}").unwrap();
        assert!(settings.load().is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
