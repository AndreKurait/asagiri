//! Select one user WSL distribution for path conversion and both build scripts.

use std::io;
use std::path::Path;
use std::process::{Command, Output};

fn decode(bytes: &[u8]) -> io::Result<String> {
    let utf16 = if let Some(rest) = bytes.strip_prefix(b"\xff\xfe") {
        Some((rest, true))
    } else if let Some(rest) = bytes.strip_prefix(b"\xfe\xff") {
        Some((rest, false))
    } else if bytes.contains(&0) {
        Some((bytes, true))
    } else {
        None
    };
    if let Some((bytes, little)) = utf16 {
        if !bytes.len().is_multiple_of(2) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated UTF-16 WSL output"));
        }
        let units: Vec<_> = bytes.as_chunks::<2>().0.iter().map(|&b| {
            if little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) }
        }).collect();
        String::from_utf16(&units).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    } else {
        String::from_utf8(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes).to_vec())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

fn decode_list(bytes: &[u8]) -> io::Result<Vec<String>> {
    Ok(decode(bytes)?.lines().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect())
}

fn choose(distros: &[String], explicit: Option<&str>) -> Option<String> {
    explicit.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).or_else(|| {
        distros.iter().find(|distro| {
            let lower = distro.to_ascii_lowercase();
            !lower.starts_with("docker-desktop") && lower != "asagiri"
                && !lower.starts_with("asagiri-") && lower != "kiri" && !lower.starts_with("kiri-")
        }).cloned()
    })
}

fn output_bytes(output: Output, operation: &str) -> io::Result<Vec<u8>> {
    if !output.status.success() {
        let reason = decode(&output.stderr).unwrap_or_else(|_| String::from_utf8_lossy(&output.stderr).into_owned());
        return Err(io::Error::other(format!("{operation} failed ({}): {}", output.status, reason.trim())));
    }
    Ok(output.stdout)
}

pub fn distro() -> io::Result<String> {
    let explicit = std::env::var("ASAGIRI_WSL_DISTRO").ok();
    if let Some(distro) = choose(&[], explicit.as_deref()) { return Ok(distro); }
    let output = Command::new("wsl.exe").env("WSL_UTF8", "1").args(["-l", "-q"]).output()?;
    let distros = decode_list(&output_bytes(output, "wsl.exe -l -q")?)?;
    choose(&distros, None).ok_or_else(|| io::Error::other(
        "no Linux user distro found; install one with `wsl --install Ubuntu` or set ASAGIRI_WSL_DISTRO",
    ))
}

pub fn command(distro: &str, program: &str) -> Command {
    let mut command = Command::new("wsl.exe");
    command.env("WSL_UTF8", "1").args(["-d", distro, "--exec", program]);
    command
}

fn path_command(distro: &str, path: &Path) -> Command {
    let mut command = command(distro, "wslpath");
    command.arg("-a").arg(path.to_string_lossy().replace('\\', "/"));
    command
}

pub fn path(distro: &str, path: &Path) -> io::Result<String> {
    let output = path_command(distro, path).output()?;
    let text = decode(&output_bytes(output, "wslpath")?)?;
    let path = text.trim();
    if !path.starts_with('/') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "wslpath did not return an absolute Linux path"));
    }
    Ok(path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtime_default_does_not_become_the_build_host() {
        let distros = decode_list(b"docker-desktop\r\ndocker-desktop-data\r\nAsagiri\r\nasagiri-dev\r\nkiri\r\nkiri-builds\r\nUbuntu-24.04\r\n").unwrap();
        assert_eq!(choose(&distros, None).as_deref(), Some("Ubuntu-24.04"));
    }

    #[test]
    fn an_explicit_distro_wins_and_blank_uses_discovery() {
        let distros = vec!["Ubuntu".into()];
        assert_eq!(choose(&distros, Some("  Fedora  ")).as_deref(), Some("Fedora"));
        assert_eq!(choose(&[], Some("my build distro")).as_deref(), Some("my build distro"));
        assert_eq!(choose(&distros, Some(" \t ")).as_deref(), Some("Ubuntu"));
    }

    #[test]
    fn only_runtime_distros_and_empty_lists_offer_no_build_host() {
        assert_eq!(choose(&decode_list(b"DOCKER-DESKTOP\r\nasagiri\r\nkiri\r\n").unwrap(), None), None);
        assert_eq!(choose(&decode_list(b" \r\n\r\n").unwrap(), None), None);
    }

    #[test]
    fn utf8_and_legacy_utf16_keep_unicode_distro_names() {
        let text = "docker-desktop\r\nUbuntu-日本語\r\n";
        let expected = vec!["docker-desktop".to_owned(), "Ubuntu-日本語".to_owned()];
        assert_eq!(decode_list(text.as_bytes()).unwrap(), expected);
        assert_eq!(decode_list([b"\xef\xbb\xbf".as_slice(), text.as_bytes()].concat().as_slice()).unwrap(), expected);
        let little: Vec<_> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_list(&little).unwrap(), expected);
        assert_eq!(decode_list(&[b"\xff\xfe".as_slice(), &little].concat()).unwrap(), expected);
        let big: Vec<_> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(decode_list(&[b"\xfe\xff".as_slice(), &big].concat()).unwrap(), expected);
    }

    #[test]
    fn malformed_output_is_not_a_silently_truncated_distro() {
        for bytes in [b"\xff\xfeU".as_slice(), b"\xff", b"\xff\xfe\x00\xd8"] {
            assert_eq!(decode_list(bytes).unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn path_conversion_and_build_programs_use_the_same_selected_distro() {
        let distro = "Ubuntu build";
        let path = path_command(distro, Path::new(r"C:\work\repo with spaces"));
        assert_eq!(path.get_args().collect::<Vec<_>>(),
            ["-d", distro, "--exec", "wslpath", "-a", "C:/work/repo with spaces"]);
        for program in ["bash", "sh"] {
            let command = command(distro, program);
            assert_eq!(command.get_args().collect::<Vec<_>>(), ["-d", distro, "--exec", program]);
        }
    }
}
