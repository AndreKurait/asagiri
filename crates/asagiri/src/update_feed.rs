//! Release feeds are opt-in. Source builds never contact another project's
//! update service, including when the window checks in the background.

use std::io;

pub fn configured() -> bool {
    std::env::var("ASAGIRI_UPDATE_URL").is_ok_and(|v| !v.trim().is_empty())
}

pub fn url() -> io::Result<String> {
    parse(std::env::var("ASAGIRI_UPDATE_URL").ok())
}

fn parse(value: Option<String>) -> io::Result<String> {
    let url = value.filter(|v| !v.trim().is_empty()).ok_or_else(|| {
        io::Error::new(io::ErrorKind::Unsupported,
            "Asagiri has no default update feed; build from source or set ASAGIRI_UPDATE_URL to a trusted Asagiri release feed")
    })?;
    let url = url.trim();
    if !url.starts_with("https://") && !(cfg!(debug_assertions) && url.starts_with("file:///")) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "ASAGIRI_UPDATE_URL must use HTTPS"));
    }
    Ok(url.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_or_empty_feed_is_disabled() {
        for value in [None, Some(String::new()), Some("  ".into())] {
            assert_eq!(parse(value).unwrap_err().kind(), io::ErrorKind::Unsupported);
        }
    }

    #[test]
    fn a_feed_requires_an_explicit_secure_address() {
        assert_eq!(parse(Some(" https://updates.example.org/asagiri.json ".into())).unwrap(),
            "https://updates.example.org/asagiri.json");
        assert!(parse(Some("http://updates.example.org/asagiri.json".into())).is_err());
    }
}
