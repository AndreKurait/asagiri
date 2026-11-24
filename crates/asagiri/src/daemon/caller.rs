//! Classify engine API connections by the listener that accepted them.

#[cfg(any(target_os = "linux", test))]
use std::path::{Path, PathBuf};

/// Who is at the other end of an API connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caller {
    /// This machine's programs, on the daemon's own socket.
    Host,
    /// A container, through the VM's relay or its own socket.
    Container,
}

/// The socket containers that mount the engine's are given, beside the
/// daemon's own: `docker.sock` → `docker.container.sock`.
#[cfg(any(target_os = "linux", test))]
pub fn container_socket(own: &Path) -> PathBuf {
    let stem = own.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "docker".into());
    own.with_file_name(format!("{stem}.container.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_container_socket_sits_beside_the_daemons() {
        assert_eq!(container_socket(Path::new("/home/me/.asagiri/run/docker.sock")), Path::new("/home/me/.asagiri/run/docker.container.sock"));
        assert_eq!(container_socket(Path::new("/run/asagiri/engine")), Path::new("/run/asagiri/engine.container.sock"));
    }
}
