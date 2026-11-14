//! `asagiri config`: read and update the running engine's daemon.json.

use std::io::{self, Read};
use serde_json::Value;

const USAGE: &str = "usage: asagiri config [show] | set JSON | set - (read JSON from stdin)";
const MAX_BYTES: usize = 64 << 10;

pub fn run(args: &[String]) -> io::Result<()> {
    let patch = match args {
        [] => None,
        [show] if show == "show" => None,
        [set, text] if set == "set" => {
            let bytes = if text == "-" {
                let mut bytes = Vec::new();
                io::stdin().take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
                bytes
            } else { text.as_bytes().to_vec() };
            if bytes.len() > MAX_BYTES {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "daemon config exceeds 64 KiB"));
            }
            Some(serde_json::from_slice::<Value>(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?)
        }
        _ => return Err(io::Error::other(USAGE)),
    };
    if !crate::client::alive() {
        return Err(io::Error::other("asagiri daemon is not running: open Asagiri or run `asagiri daemon`"));
    }
    if let Some(patch) = patch {
        let response = crate::client::request("POST", "/asagiri/daemon-config", Some(&patch))?;
        if !response.ok() { return Err(io::Error::other(response.error())); }
    }
    let value = crate::client::get("/asagiri/daemon-config")?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
