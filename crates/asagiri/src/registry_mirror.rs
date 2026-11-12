//! `asagiri registry-mirror`: Settings → Registry mirrors from the terminal.
//! Docker Hub images are pulled through the configured mirrors, in order,
//! then from Docker Hub itself; other registries are pulled directly.
//!
//! The setting is the daemon's, because pulls happen there, so these commands
//! go through its API as `asagiri machine` does. On Windows the whole command
//! runs inside Asagiri's WSL distro (crates/asagiri/src/wsl.rs), with the daemon's
//! own HOME, and edits the same file.

use std::io;

use serde_json::json;

use asagiri_image::Mirror;

use crate::client;

const USAGE: &str = "usage: asagiri registry-mirror list | add ADDRESS | remove ADDRESS";

pub fn run(args: &[String]) -> io::Result<()> {
    if !client::alive() {
        return Err(io::Error::other("asagiri daemon is not running: open Asagiri or run `asagiri daemon`"));
    }
    match args.first().map(String::as_str) {
        Some("list") | None => list(),
        Some("add") => add(args.get(1)),
        Some("remove" | "rm") => remove(args.get(1)),
        _ => Err(io::Error::other(USAGE)),
    }
}

fn list() -> io::Result<()> {
    let mirrors = current()?;
    if mirrors.is_empty() {
        println!("no registry mirrors: Docker Hub images come from Docker Hub itself");
        return Ok(());
    }
    println!("registry mirrors of Docker Hub, in the order a pull tries them:");
    for mirror in mirrors {
        println!("  {mirror}");
    }
    Ok(())
}

fn add(address: Option<&String>) -> io::Result<()> {
    let mirror = parse(address)?;
    let mut mirrors = current()?;
    if !mirrors.contains(&mirror) {
        mirrors.push(mirror.clone());
    }
    save(&mirrors)?;
    println!("{mirror} is a mirror of Docker Hub now; the next pull tries it");
    Ok(())
}

fn remove(address: Option<&String>) -> io::Result<()> {
    let mirror = parse(address)?;
    let mut mirrors = current()?;
    let before = mirrors.len();
    mirrors.retain(|m| m != &mirror);
    if mirrors.len() == before {
        return Err(io::Error::other(format!("{mirror} is not a configured mirror")));
    }
    save(&mirrors)?;
    println!("{mirror} is no longer a mirror of Docker Hub");
    Ok(())
}

/// The address as the daemon stores it (normalized), so `add` and `remove`
/// name the same mirror however it was typed.
fn parse(address: Option<&String>) -> io::Result<String> {
    let address = address.ok_or_else(|| io::Error::other(USAGE))?;
    Mirror::parse(address).map(|m| m.as_url()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

fn current() -> io::Result<Vec<String>> {
    let v = client::get("/asagiri/registry-mirrors")?;
    Ok(v["mirrors"].as_array().into_iter().flatten().filter_map(|m| m.as_str()).map(str::to_owned).collect())
}

fn save(mirrors: &[String]) -> io::Result<()> {
    let r = client::request("POST", "/asagiri/registry-mirrors", Some(&json!({"mirrors": mirrors})))?;
    if r.ok() { Ok(()) } else { Err(io::Error::other(r.error())) }
}
