//! Registry mirrors: Settings → Registry mirrors
//! (saved with the other registry settings in `~/.asagiri/daemon.json`).
//! The old `registry-mirrors.json` is read on first startup. Docker Hub images are pulled through
//! these first, in the order they are listed, then from Docker Hub itself.
//! As Docker's `registry-mirrors`, they apply to `docker.io` only: other
//! registries are pulled from directly, and pushes never go through them.
//!
//! The list is the daemon's, not the window's, because pulls happen here;
//! `asagiri registry-mirror` edits it through the same HTTP API.

use asagiri_image::Mirror;
use serde_json::{Value, json};

/// Enough to fail over a few times, few enough that a pull does not spend
/// its time asking one dead address after another.
#[cfg(test)]
const MAX: usize = 16;

/// The validated configured mirrors, in the order a pull tries them.
pub fn list() -> Vec<Mirror> {
    super::registry_config::snapshot().mirrors
}

/// What Settings shows: the saved addresses, normalized, and what they
/// apply to.
pub fn report() -> Value {
    json!({"mirrors": list().iter().map(Mirror::as_url).collect::<Vec<_>>(), "applies_to": "docker.io"})
}

/// Saves the list Settings sent, each address checked by [`Mirror::parse`].
/// Returns what was saved, normalized.
pub fn set(body: &Value) -> Result<Value, String> {
    let given = body["mirrors"].as_array().ok_or_else(|| "mirrors must be a list of addresses".to_owned())?;
    super::registry_config::set(&json!({"registry-mirrors": given}))?;
    Ok(report())
}

/// Checks and normalizes the addresses: each a mirror, in order, no
/// duplicates.
#[cfg(test)]
fn normalize(given: &[Value]) -> Result<Vec<Mirror>, String> {
    if given.len() > MAX {
        return Err(format!("at most {MAX} mirrors"));
    }
    let mut mirrors: Vec<Mirror> = Vec::new();
    for value in given {
        let text = value.as_str().ok_or_else(|| "a mirror must be an address string".to_owned())?;
        let mirror = Mirror::parse(text)?;
        if !mirrors.contains(&mirror) {
            mirrors.push(mirror);
        }
    }
    Ok(mirrors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_is_normalized_and_deduplicated() {
        let body = json!({"mirrors": ["mirror.example.com", "https://mirror.example.com/", " https://mirror.example.net/docker.io ", "https://mirror.example.com/v2"]});
        let urls: Vec<String> = normalize(body["mirrors"].as_array().unwrap()).unwrap().iter().map(Mirror::as_url).collect();
        assert_eq!(urls, ["https://mirror.example.com", "https://mirror.example.net/docker.io"]);
    }

    #[test]
    fn what_is_not_a_mirror_is_refused() {
        assert!(Mirror::parse("http://mirror.example.com").is_err());
        assert!(normalize(&[json!("http://mirror.example.com")]).is_err());
        assert!(normalize(&[json!(42)]).is_err());
        assert!(normalize(&[]).unwrap().is_empty());
    }
}
