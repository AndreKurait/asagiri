//! The host name and persistent identity reported by the Docker API.

use std::io::{self, Write};
use std::path::Path;

pub fn hostname() -> String {
    let mut name = [0u8; 256];
    // SAFETY: a writable buffer of the stated length.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } == 0 {
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        let name = String::from_utf8_lossy(&name[..end]).into_owned();
        if !name.is_empty() {
            return name;
        }
    }
    crate::share::host_name()
}

fn read_id(path: &Path) -> io::Result<String> {
    let value = std::fs::read_to_string(path)?;
    let id = value.trim();
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-:".contains(&b)) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid daemon ID"));
    }
    Ok(id.to_owned())
}

/// Publish a complete ID once; simultaneous initializers read the winner.
/// Never silently replace an existing identity after an I/O or parse error.
pub fn load_id(root: &Path) -> io::Result<String> {
    let path = root.join("engine-id");
    match read_id(&path) {
        Ok(id) => return Ok(id),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut hex = super::engine::random_hex(16).into_bytes();
    hex[12] = b'4';
    let variant = (hex[16] as char).to_digit(16).unwrap() as usize & 3;
    hex[16] = b"89ab"[variant];
    let hex = String::from_utf8(hex).expect("random_hex is ASCII");
    let id = format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]);
    let temp = root.join(format!(".engine-id-{}", super::engine::random_hex(16)));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
        writeln!(file, "{id}")?;
        file.sync_all()?;
        match std::fs::hard_link(&temp, &path) {
            Ok(()) => Ok(id),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => read_id(&path),
            Err(e) => Err(e),
        }
    })();
    let _ = std::fs::remove_file(temp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_identity_survives_reloads_and_concurrent_initializers() {
        let root = std::env::temp_dir().join(format!("asagiri-identity-{}", super::super::engine::random_hex(16)));
        std::fs::create_dir(&root).unwrap();
        let ids = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8).map(|_| scope.spawn(|| load_id(&root).unwrap())).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        });
        assert!(ids.iter().all(|id| id == &ids[0]));
        assert_eq!(ids[0].len(), 36);
        assert_eq!(load_id(&root).unwrap(), ids[0]);
        let other = root.join("other");
        std::fs::create_dir(&other).unwrap();
        assert_ne!(load_id(&other).unwrap(), ids[0]);
        std::fs::write(root.join("engine-id"), "").unwrap();
        assert_eq!(load_id(&root).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(root.join("engine-id")).unwrap(), b"");
        std::fs::remove_dir_all(root).unwrap();
    }
}
