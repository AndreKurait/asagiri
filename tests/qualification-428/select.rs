#[path = "../../crates/asagiri-win/src/docker_context.rs"]
mod docker_context;
use std::{io, process::Command};
fn main() -> io::Result<()> {
    let name = std::env::args().nth(1).unwrap();
    docker_context::select(&name, "asagiri", "npipe:////./pipe/asagiri", |args| {
        let out = Command::new("docker.exe").args(args).env_remove("DOCKER_HOST").env_remove("DOCKER_CONTEXT").output()?;
        if !out.status.success() { return Err(io::Error::other(String::from_utf8_lossy(&out.stderr).into_owned())); }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    })
}
