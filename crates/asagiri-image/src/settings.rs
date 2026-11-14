//! One pull/push's registry policy. Explicit authorities never match another
//! port or a hostname suffix. CIDRs apply to literal IP image references;
//! hostname references need an explicit authority, without a DNS trust lookup.

use std::net::IpAddr;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Rule {
    Authority(String),
    Network(IpAddr, u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrySettings {
    insecure: Vec<String>,
    rules: Vec<Rule>,
    downloads: usize,
    uploads: usize,
}

impl Default for RegistrySettings {
    fn default() -> Self {
        Self { insecure: Vec::new(), rules: Vec::new(), downloads: 4, uploads: 5 }
    }
}

/// An authority, never a URL, userinfo, path, query or fragment.
pub(crate) fn authority(input: &str) -> Result<String, String> {
    let text = input.trim().to_ascii_lowercase();
    if text.is_empty() || text.len() > 260 || text.bytes().any(|b| b.is_ascii_whitespace() || b"/@?#\\%".contains(&b)) {
        return Err(format!("expected a registry host or host:port: {input}"));
    }
    let parsed: ureq::http::uri::Authority = text.parse().map_err(|_| format!("invalid registry authority: {input}"))?;
    let host = parsed.host().trim_start_matches('[').trim_end_matches(']');
    if host.parse::<IpAddr>().is_err() && (host.len() > 253 || host.split('.').any(|label| {
        label.is_empty() || label.len() > 63 || label.starts_with('-') || label.ends_with('-')
            || !label.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    })) {
        return Err(format!("invalid registry host: {input}"));
    }
    let suffix = if text.starts_with('[') { text.split_once(']').map(|(_, s)| s).unwrap_or("invalid") } else { &text[host.len()..] };
    if !suffix.is_empty() && (!suffix.starts_with(':') || suffix[1..].parse::<u16>().ok().filter(|p| *p > 0).is_none()) {
        return Err(format!("registry port must be 1 to 65535: {input}"));
    }
    Ok(text)
}

fn network(ip: IpAddr, bits: u8) -> Option<IpAddr> {
    match ip {
        IpAddr::V4(ip) if bits <= 32 => Some(IpAddr::V4((u32::from(ip) & u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0)).into())),
        IpAddr::V6(ip) if bits <= 128 => Some(IpAddr::V6((u128::from(ip) & u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0)).into())),
        _ => None,
    }
}

impl RegistrySettings {
    pub fn new(insecure: &[String], downloads: usize, uploads: usize) -> Result<Self, String> {
        if !(1..=16).contains(&downloads) || !(1..=16).contains(&uploads) {
            return Err("max-concurrent-downloads and max-concurrent-uploads must be integers from 1 to 16".into());
        }
        if insecure.len() > 64 {
            return Err("at most 64 insecure-registries".into());
        }
        let mut out = Self { downloads, uploads, ..Self::default() };
        for item in insecure {
            let text = item.trim();
            let (normalized, rule) = if let Some((address, bits)) = text.split_once('/') {
                let ip: IpAddr = address.parse().map_err(|_| format!("invalid registry CIDR: {item}"))?;
                let bits = bits.parse::<u8>().map_err(|_| format!("invalid registry CIDR: {item}"))?;
                let ip = network(ip, bits).ok_or_else(|| format!("invalid registry CIDR: {item}"))?;
                (format!("{ip}/{bits}"), Rule::Network(ip, bits))
            } else {
                let host = authority(text)?;
                (host.clone(), Rule::Authority(host))
            };
            if !out.rules.contains(&rule) {
                out.insecure.push(normalized);
                out.rules.push(rule);
            }
        }
        Ok(out)
    }

    pub fn insecure_registries(&self) -> &[String] { &self.insecure }
    pub fn max_downloads(&self) -> usize { self.downloads }
    pub fn max_uploads(&self) -> usize { self.uploads }

    pub fn allows_insecure(&self, input: &str) -> bool {
        let Ok(host) = authority(input) else { return false };
        let ip = host.parse::<ureq::http::uri::Authority>().ok()
            .and_then(|a| a.host().trim_matches(['[', ']']).parse::<IpAddr>().ok());
        self.rules.iter().any(|rule| match rule {
            Rule::Authority(allowed) => *allowed == host,
            Rule::Network(allowed, bits) => ip.and_then(|ip| network(ip, *bits)) == Some(*allowed),
        })
    }

    /// BuildKit's config accepts individual registry authorities, not CIDRs.
    pub fn authorities(&self) -> impl Iterator<Item = &str> {
        self.rules.iter().filter_map(|r| match r { Rule::Authority(a) => Some(a.as_str()), _ => None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_authorities_are_exact_and_networks_only_match_literal_ips() {
        let settings = RegistrySettings::new(&[" Registry.Example:5000 ".into(), "10.9.1.8/16".into(), "fd12::/48".into()], 1, 16).unwrap();
        for yes in ["registry.example:5000", "REGISTRY.EXAMPLE:5000", "10.9.4.3:7000", "[fd12::3]:5000"] {
            assert!(settings.allows_insecure(yes), "{yes}");
        }
        for no in ["registry.example", "registry.example:5001", "registry.example.evil:5000", "10.10.1.2", "internal.example", "[fd13::3]:5000"] {
            assert!(!settings.allows_insecure(no), "{no}");
        }
        assert_eq!(settings.insecure_registries(), ["registry.example:5000", "10.9.0.0/16", "fd12::/48"]);
        assert!(!RegistrySettings::default().allows_insecure("registry.example:5000"));
    }

    #[test]
    fn malformed_policies_never_become_broad_trust_rules() {
        for value in ["https://registry.example", "user:pass@registry.example", "registry.example/path", "registry.example?x",
            "registry.example#x", "registry.example:0", "registry.example:65536", "registry.example:", "registry.example:abc",
            "registry..example", "registry.example\\other", "[::1", "::1", "10.0.0.1/33", "::/129", "10.0.0.1/16/path", ""] {
            assert!(RegistrySettings::new(&[value.into()], 4, 5).is_err(), "{value}");
        }
        for (down, up) in [(0, 5), (17, 5), (4, 0), (4, 17)] {
            assert!(RegistrySettings::new(&[], down, up).is_err());
        }
    }
}
