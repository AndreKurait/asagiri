//! Docker networks and address assignment (IPAM).
//!
//! `bridge` (the default, `kiri0`, 172.17.0.0/16), `host` and `none` always
//! exist. User-defined bridge networks get their own bridge and a /16 from
//! 172.18.0.0 up, as Docker picks them, and are saved to `networks.json`.
//! `IPAM.Config` (`--subnet`, `--gateway`, `--ip-range`, `--aux-address`)
//! picks them instead, and containers may then ask for fixed addresses.
//!
//! `--ipv6` (Compose `enable_ipv6`) adds an IPv6 subnet beside the IPv4 one:
//! an IPv6 `IPAM.Config` entry, or a /64 from Kiri's own ULA pool (a /48
//! under fd00::/8 per engine), as Docker's default address pools hand out.
//! The default `bridge` stays IPv4-only, as Docker's does.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Network {
    pub id: String,
    pub name: String,
    /// `bridge`, `macvlan`, `ipvlan`, `host` or `null`.
    pub driver: String,
    /// The device in the guest its containers are plugged into: a bridge,
    /// or the link a macvlan or ipvlan network's interfaces are made on
    /// (empty for host and none).
    pub bridge: String,
    pub subnet: [u8; 4],
    pub prefix: u8,
    /// All zeros on a macvlan or ipvlan network that was given none: it has
    /// no gateway of its own, and its first address goes to a container.
    pub gateway: [u8; 4],
    pub created_ns: u64,
    #[serde(default)]
    pub labels: serde_json::Value,
    #[serde(default)]
    pub internal: bool,
    /// The subnet came from `IPAM.Config`: only then may a container ask
    /// for an address of its own, as with Docker.
    #[serde(default)]
    pub user_subnet: bool,
    /// `--ip-range`: automatic addresses come from here only.
    #[serde(default)]
    pub ip_range: Option<([u8; 4], u8)>,
    /// `--aux-address name=ip`: never handed out.
    #[serde(default)]
    pub aux: BTreeMap<String, [u8; 4]>,
    /// `--opt` and `--ipam-opt`, kept for inspect.
    #[serde(default)]
    pub options: serde_json::Value,
    #[serde(default)]
    pub ipam_options: serde_json::Value,
    #[serde(default)]
    pub attachable: bool,
    /// An IPv6-only network (`--ipv4=false --ipv6`). It still has an IPv4
    /// subnet, inside Kiri only: each container's address there is the
    /// name the guest keeps it by (DNS, MAC), but nothing is given it, the
    /// bridge or the container, and inspect shows none, as with Docker.
    #[serde(default)]
    pub no_ipv4: bool,
    /// The IPv6 side of an `--ipv6` network.
    #[serde(default)]
    pub v6: Option<V6>,
    /// Host parts handed out (runtime only).
    #[serde(skip)]
    used: BTreeSet<u32>,
}

/// An IPv6 network's subnet and addresses, the IPv4 fields' counterparts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V6 {
    pub subnet: [u8; 16],
    pub prefix: u8,
    pub gateway: [u8; 16],
    /// From `IPAM.Config`, not the pool. (A container may ask for an IPv6
    /// address of its own, `--ip6`, either way, as with Docker 29.)
    #[serde(default)]
    pub user_subnet: bool,
    #[serde(default)]
    pub ip_range: Option<([u8; 16], u8)>,
    #[serde(default)]
    pub aux: BTreeMap<String, [u8; 16]>,
    /// Host parts handed out (runtime only).
    #[serde(skip)]
    used: BTreeSet<u128>,
}

pub fn ip6_str(a: [u8; 16]) -> String {
    std::net::Ipv6Addr::from(a).to_string()
}

pub fn parse_ip6(s: &str) -> Option<[u8; 16]> {
    s.parse::<std::net::Ipv6Addr>().ok().map(|a| a.octets())
}

fn mask6(prefix: u8) -> u128 {
    if prefix == 0 { 0 } else { u128::MAX << (128 - prefix as u32) }
}

pub fn in_subnet6(ip: [u8; 16], subnet: [u8; 16], prefix: u8) -> bool {
    u128::from_be_bytes(ip) & mask6(prefix) == u128::from_be_bytes(subnet)
}

pub fn overlaps6(a: ([u8; 16], u8), b: ([u8; 16], u8)) -> bool {
    in_subnet6(a.0, b.0, b.1) || in_subnet6(b.0, a.0, a.1)
}

impl V6 {
    /// Docker's `"fd00:1::/64"`.
    pub fn subnet_str(&self) -> String {
        format!("{}/{}", ip6_str(self.subnet), self.prefix)
    }

    pub fn ip_range_str(&self) -> Option<String> {
        self.ip_range.map(|(a, p)| format!("{}/{p}", ip6_str(a)))
    }

    fn reserved(&self, ip: [u8; 16]) -> bool {
        ip == self.gateway || self.aux.values().any(|a| *a == ip)
    }

    /// Host part of `ip` in the subnet.
    fn host(&self, ip: [u8; 16]) -> u128 {
        u128::from_be_bytes(ip).wrapping_sub(u128::from_be_bytes(self.subnet))
    }

    /// The highest host part: IPv6 has no broadcast address, so the last one
    /// is a host too, as Docker hands it out.
    fn last(&self) -> u128 {
        !mask6(self.prefix)
    }

    /// The next free address from ::1 up (or the range), past the gateway,
    /// the auxiliary addresses and `avoid`.
    fn allocate_avoiding(&mut self, avoid: &[[u8; 16]]) -> Option<[u8; 16]> {
        let base = u128::from_be_bytes(self.subnet);
        let (first, last) = match self.ip_range {
            Some((r, p)) => {
                let start = u128::from_be_bytes(r) - base;
                (start.max(1), (start + !mask6(p)).min(self.last()))
            }
            None => (1, self.last()),
        };
        // Taken addresses are few: the search ends within as many steps.
        let mut h = first;
        loop {
            if h > last {
                return None;
            }
            let ip = (base + h).to_be_bytes();
            if !self.used.contains(&h) && !self.reserved(ip) && !avoid.contains(&ip) {
                self.used.insert(h);
                return Some(ip);
            }
            h = h.checked_add(1)?;
        }
    }

    fn release(&mut self, ip: [u8; 16]) {
        let h = self.host(ip);
        self.used.remove(&h);
    }
}

impl Network {
    pub fn builtin(&self) -> bool {
        matches!(self.name.as_str(), "bridge" | "host" | "none")
    }

    /// The MTU its options ask for, if they do; see [`mtu_option`].
    /// A macvlan or ipvlan network has the MTU of the link it is made on.
    pub fn mtu(&self) -> Option<u32> {
        mtu_option(&self.options, self.v6.is_some()).ok().flatten().filter(|_| !self.own_link())
    }

    /// Whether its containers may reach each other over it; see [`icc_option`].
    /// A stored value that is not a boolean means on (see `Networks::load`).
    pub fn icc(&self) -> bool {
        self.own_link() || icc_option(&self.options).unwrap_or(true)
    }

    /// A macvlan network's `-o macvlan_mode` (`bridge` when not given);
    /// `None` on any other network.
    pub fn macvlan_mode(&self) -> Option<&str> {
        (self.driver == "macvlan").then(|| self.options.get("macvlan_mode").and_then(|m| m.as_str()).unwrap_or("bridge"))
    }

    /// An ipvlan network's `-o ipvlan_mode` and `-o ipvlan_flag` (`l2` and
    /// `bridge` when not given); `None` on any other network.
    pub fn ipvlan(&self) -> Option<(&str, &str)> {
        let option = |k: &str, default: &'static str| self.options.get(k).and_then(|m| m.as_str()).unwrap_or(default);
        (self.driver == "ipvlan").then(|| (option("ipvlan_mode", "l2"), option("ipvlan_flag", "bridge")))
    }

    /// Docker's refusal of a MAC address on an ipvlan network: a link there
    /// has its parent's MAC and takes no other.
    pub fn check_mac(&self, mac: &str) -> Result<(), String> {
        if self.driver == "ipvlan" && !mac.is_empty() {
            return Err("ipvlan interfaces do not support custom mac address assignment".into());
        }
        Ok(())
    }

    /// A macvlan or ipvlan network: its containers have links of their own
    /// on a link of the network's, and reach each other and nothing else.
    /// No gateway, no default route, no published ports.
    pub fn own_link(&self) -> bool {
        matches!(self.driver.as_str(), "macvlan" | "ipvlan")
    }

    /// Docker's `"172.17.0.0/16"`.
    pub fn subnet_str(&self) -> String {
        format!("{}/{}", ip_str(self.subnet), self.prefix)
    }

    /// Docker's `"172.17.0.0/16"`.
    pub fn ip_range_str(&self) -> Option<String> {
        self.ip_range.map(|(a, p)| format!("{}/{p}", ip_str(a)))
    }

    /// Whether `ip` is in the subnet.
    pub fn contains(&self, ip: [u8; 4]) -> bool {
        in_subnet(ip, self.subnet, self.prefix)
    }

    /// Addresses no container may have: the gateway and the auxiliary ones.
    fn reserved(&self, ip: [u8; 4]) -> bool {
        ip == self.gateway || self.aux.values().any(|a| *a == ip)
    }

    /// The next free address, skipping the network, broadcast, gateway and
    /// auxiliary addresses and `avoid`; only from `--ip-range` when one is set.
    fn allocate_avoiding(&mut self, avoid: &[[u8; 4]]) -> Option<[u8; 4]> {
        let base = u32::from_be_bytes(self.subnet);
        let size = 1u32 << (32 - self.prefix as u32);
        let (first, last) = match self.ip_range {
            Some((r, p)) => {
                let start = u32::from_be_bytes(r) - base;
                (start.max(1), (start + (1u32 << (32 - p as u32)) - 1).min(size - 2))
            }
            None => (1, size - 2),
        };
        let host = (first..=last).find(|h| !self.used.contains(h) && !self.reserved((base + h).to_be_bytes()) && !avoid.contains(&(base + h).to_be_bytes()))?;
        self.used.insert(host);
        Some((base + host).to_be_bytes())
    }

    /// Whether a container may ask for `ip` here (`--ip`, Compose
    /// `ipv4_address`), in Docker's words when not. Whether it is free is
    /// only known when the container starts.
    pub fn check_static(&self, ip: [u8; 4]) -> Result<(), String> {
        if self.bridge.is_empty() {
            return Err(format!("user specified IP address is supported only on bridge networks, not {}", self.name));
        }
        if self.builtin() {
            return Err("user specified IP address is supported on user defined networks only".into());
        }
        if !self.user_subnet {
            return Err("user specified IP address is supported only when connecting to networks with user configured subnets".into());
        }
        let size = 1u32 << (32 - self.prefix as u32);
        let host = u32::from_be_bytes(ip).wrapping_sub(u32::from_be_bytes(self.subnet));
        if !self.contains(ip) || host == 0 || host == size - 1 {
            return Err(format!("no configured subnet or ip-range contain the IP address {}", ip_str(ip)));
        }
        Ok(())
    }

    /// Whether a container may ask for `ip` here (`--ip6`, Compose
    /// `ipv6_address`), in Docker's words when not.
    pub fn check_static6(&self, ip: [u8; 16]) -> Result<(), String> {
        if self.bridge.is_empty() {
            return Err(format!("user specified IP address is supported only on bridge networks, not {}", self.name));
        }
        if self.builtin() {
            return Err("user specified IP address is supported on user defined networks only".into());
        }
        let Some(v6) = &self.v6 else {
            return Err(format!("invalid endpoint settings: an IPv6 address was given, but network {} does not have IPv6 enabled", self.name));
        };
        // Docker 29 takes one on an automatically allocated IPv6 subnet too
        // (moby#51569), unlike IPv4.
        if !in_subnet6(ip, v6.subnet, v6.prefix) || v6.host(ip) == 0 {
            return Err(format!("no configured subnet or ip-range contain the IP address {}", ip6_str(ip)));
        }
        Ok(())
    }

    fn reserve6(&mut self, ip: [u8; 16]) -> Result<(), String> {
        self.check_static6(ip)?;
        let v6 = self.v6.as_mut().expect("checked");
        let h = v6.host(ip);
        if v6.reserved(ip) || !v6.used.insert(h) {
            return Err(format!("Address already in use: {}", ip6_str(ip)));
        }
        Ok(())
    }

    /// Takes `ip` for a container that asked for it.
    fn reserve(&mut self, ip: [u8; 4]) -> Result<(), String> {
        self.check_static(ip)?;
        let host = u32::from_be_bytes(ip) - u32::from_be_bytes(self.subnet);
        if self.reserved(ip) || !self.used.insert(host) {
            return Err(format!("Address already in use: {}", ip_str(ip)));
        }
        Ok(())
    }

    fn release(&mut self, ip: [u8; 4]) {
        let host = u32::from_be_bytes(ip).wrapping_sub(u32::from_be_bytes(self.subnet));
        self.used.remove(&host);
    }
}

#[derive(Debug)]
pub struct Networks {
    list: Vec<Network>,
    path: PathBuf,
    /// This engine's ULA /48 (fd + 40 bits), where `--ipv6` networks without
    /// a subnet get theirs: made from the engine's directory, so it stays
    /// the same across restarts and differs between engines.
    ula: [u8; 6],
}

/// The /48 for an engine at `root`: RFC 4193's fd00::/8 and a 40-bit global
/// ID, here from a hash of the path in place of a random one.
fn ula_prefix(root: &std::path::Path) -> [u8; 6] {
    use sha2::Digest;
    let h = sha2::Sha256::digest(root.to_string_lossy().as_bytes());
    [0xfd, h[0], h[1], h[2], h[3], h[4]]
}

/// The /64 an `--ipv6` network without a subnet gets: subnet ID from a hash
/// of the name, so a network made again gets the same one, moved on past
/// any taken one (by Kiri, or routed on a Linux host).
fn free_subnet6(ula: [u8; 6], name: &str, taken: &[([u8; 16], u8)]) -> Option<([u8; 16], u8)> {
    use sha2::Digest;
    let h = sha2::Sha256::digest(name.as_bytes());
    let start = u16::from_be_bytes([h[0], h[1]]);
    let routes = host_routes6();
    (0..=u16::MAX).map(|i| start.wrapping_add(i)).find_map(|id| {
        let mut s = [0u8; 16];
        s[..6].copy_from_slice(&ula);
        s[6..8].copy_from_slice(&id.to_be_bytes());
        let cand = (s, 64u8);
        (!taken.iter().chain(&routes).any(|t| overlaps6(*t, cand))).then_some(cand)
    })
}

pub fn ip_str(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

fn builtin(name: &str, driver: &str, bridge: &str, subnet: [u8; 4], gateway: [u8; 4]) -> Network {
    Network {
        // Stable ids, so clients that remember them keep working.
        id: format!("{:0<64}", name.bytes().map(|b| format!("{b:02x}")).collect::<String>()),
        name: name.into(),
        driver: driver.into(),
        bridge: bridge.into(),
        subnet,
        prefix: if bridge.is_empty() { 0 } else { 16 },
        gateway,
        created_ns: 0,
        labels: serde_json::json!({}),
        internal: false,
        user_subnet: false,
        ip_range: None,
        aux: BTreeMap::new(),
        options: serde_json::json!({}),
        ipam_options: serde_json::Value::Null,
        attachable: false,
        no_ipv4: false,
        v6: None,
        used: BTreeSet::new(),
    }
}

/// What `POST /networks/create` asks for beyond the name.
#[derive(Debug, Default)]
pub struct Settings {
    pub labels: serde_json::Value,
    /// `Driver: macvlan`: containers share a link of the network's own and
    /// reach each other, and nothing else, over it.
    pub macvlan: bool,
    /// `Driver: ipvlan`: the same, with ipvlan links.
    pub ipvlan: bool,
    pub internal: bool,
    pub attachable: bool,
    pub options: serde_json::Value,
    pub ipam_options: serde_json::Value,
    pub ipam: Option<Ipam>,
    /// `EnableIPv6`.
    pub ipv6: bool,
    /// `EnableIPv4: false`: IPv6 only (with `ipv6`).
    pub no_ipv4: bool,
    /// An IPv6 `IPAM.Config` entry; without one an IPv6 network gets a /64
    /// from the pool.
    pub ipam6: Option<Ipam6>,
}

/// The option Docker's bridge driver reads the MTU from.
pub const MTU_OPTION: &str = "com.docker.network.driver.mtu";

/// `--opt com.docker.network.driver.mtu=1400`, checked as Docker does when
/// it creates the bridge: a number (Go's `Atoi`), 0 for the default, else 68
/// to 65535, the range a bridge takes. IPv6 needs 1280, so Docker fails to
/// set up a smaller IPv6 bridge.
pub fn mtu_option(options: &serde_json::Value, ipv6: bool) -> Result<Option<u32>, String> {
    use std::num::IntErrorKind::{NegOverflow, PosOverflow};
    let Some(raw) = options.get(MTU_OPTION) else { return Ok(None) };
    let raw = raw.as_str().unwrap_or_default();
    let mtu: i64 = raw.parse().map_err(|e: std::num::ParseIntError| {
        // `Atoi`'s two errors: a number too large for an int, or no number.
        let why = if matches!(e.kind(), PosOverflow | NegOverflow) { "value out of range" } else { "invalid syntax" };
        format!("failed to parse {MTU_OPTION} value: {raw} (strconv.Atoi: parsing {raw:?}: {why})")
    })?;
    match mtu {
        0 => Ok(None),
        m if m < 0 => Err(format!("invalid MTU number: {m}")),
        68..=65535 if !ipv6 || mtu >= 1280 => Ok(Some(mtu as u32)),
        68..=65535 => Err(format!("invalid MTU number: {mtu} (an IPv6 network needs at least 1280)")),
        _ => Err(format!("invalid MTU number: {mtu} (a bridge takes 68 to 65535)")),
    }
}

/// The option Docker's bridge driver reads inter-container connectivity from.
pub const ICC_OPTION: &str = "com.docker.network.bridge.enable_icc";

/// `--opt com.docker.network.bridge.enable_icc=false`, read as Docker does,
/// with Go's `ParseBool`: on unless the option says otherwise.
pub fn icc_option(options: &serde_json::Value) -> Result<bool, String> {
    let Some(raw) = options.get(ICC_OPTION) else { return Ok(true) };
    match raw.as_str().unwrap_or_default() {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Ok(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Ok(false),
        raw => Err(format!("failed to parse {ICC_OPTION} value: {raw} (strconv.ParseBool: parsing {raw:?}: invalid syntax)")),
    }
}

/// One of a network's `--opt`s; empty when it was not given. Docker takes
/// no option that is not a string.
fn option<'a>(options: &'a serde_json::Value, k: &str) -> Result<&'a str, String> {
    match options.get(k) {
        None => Ok(""),
        Some(v) => v.as_str().ok_or_else(|| format!("invalid network option {k}: {v} is not a string")),
    }
}

/// `parent` is refused: it would put the containers on a network of the
/// machine's, where Kiri's own interface there is all that can be reached.
fn no_parent(options: &serde_json::Value, driver: &str) -> Result<(), String> {
    match option(options, "parent")? {
        "" => Ok(()),
        p => Err(format!("Kiri's {driver} networks have no parent interface (-o parent={p}): they connect containers to each other only")),
    }
}

/// A macvlan network's `--opt`s, checked: the mode in Docker's words.
fn check_macvlan(options: &serde_json::Value) -> Result<(), String> {
    match option(options, "macvlan_mode")? {
        "" | "bridge" | "private" | "vepa" | "passthru" => {}
        m => return Err(format!("requested macvlan mode '{m}' is not valid, 'bridge' mode is the macvlan driver default")),
    }
    no_parent(options, "macvlan")
}

/// An ipvlan network's `--opt`s, checked: the mode and the flag in Docker's
/// words.
fn check_ipvlan(options: &serde_json::Value) -> Result<(), String> {
    match option(options, "ipvlan_mode")? {
        "" | "l2" | "l3" | "l3s" => {}
        m => return Err(format!("requested ipvlan mode '{m}' is not valid, 'l2' mode is the ipvlan driver default")),
    }
    match option(options, "ipvlan_flag")? {
        "" | "bridge" | "private" | "vepa" => {}
        f => return Err(format!("requested ipvlan flag '{f}' is not valid, 'bridge' is the ipvlan driver default")),
    }
    no_parent(options, "ipvlan")
}

/// One IPv6 `IPAM.Config` entry, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ipam6 {
    pub subnet: [u8; 16],
    pub prefix: u8,
    pub gateway: Option<[u8; 16]>,
    pub ip_range: Option<([u8; 16], u8)>,
    pub aux: BTreeMap<String, [u8; 16]>,
}

/// One IPv4 `IPAM.Config` entry, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ipam {
    pub subnet: [u8; 4],
    pub prefix: u8,
    pub gateway: Option<[u8; 4]>,
    pub ip_range: Option<([u8; 4], u8)>,
    pub aux: BTreeMap<String, [u8; 4]>,
}

pub fn in_subnet(ip: [u8; 4], subnet: [u8; 4], prefix: u8) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) };
    u32::from_be_bytes(ip) & mask == u32::from_be_bytes(subnet)
}

/// Whether two subnets share any address.
pub fn overlaps(a: ([u8; 4], u8), b: ([u8; 4], u8)) -> bool {
    in_subnet(a.0, b.0, b.1) || in_subnet(b.0, a.0, a.1)
}

pub fn parse_ip(s: &str) -> Option<[u8; 4]> {
    s.parse::<std::net::Ipv4Addr>().ok().map(|a| a.octets())
}

/// `02:42:ac:11:00:02`.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = s.split(':');
    for b in &mut mac {
        let p = parts.next().filter(|p| p.len() == 2)?;
        *b = u8::from_str_radix(p, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// `10.5.0.0/16`; `what` names it in errors, as Docker does.
fn parse_cidr(s: &str, what: &str) -> Result<([u8; 4], u8), String> {
    let bad = || format!("invalid {what} {s}: invalid CIDR block notation");
    let (ip, bits) = s.split_once('/').ok_or_else(bad)?;
    let ip = parse_ip(ip).ok_or_else(bad)?;
    let bits: u8 = bits.parse().ok().filter(|b| *b <= 32).ok_or_else(bad)?;
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits as u32) };
    let net = (u32::from_be_bytes(ip) & mask).to_be_bytes();
    if net != ip {
        return Err(format!("invalid {what} {s}: it should be {}/{bits}", ip_str(net)));
    }
    Ok((ip, bits))
}

/// `fd00:1::/64`; `what` names it in errors, as Docker does.
fn parse_cidr6(s: &str, what: &str) -> Result<([u8; 16], u8), String> {
    let bad = || format!("invalid {what} {s}: invalid CIDR block notation");
    let (ip, bits) = s.split_once('/').ok_or_else(bad)?;
    let ip = parse_ip6(ip).ok_or_else(bad)?;
    let bits: u8 = bits.parse().ok().filter(|b| *b <= 128).ok_or_else(bad)?;
    let net = (u128::from_be_bytes(ip) & mask6(bits)).to_be_bytes();
    if net != ip {
        return Err(format!("invalid {what} {s}: it should be {}/{bits}", ip6_str(net)));
    }
    Ok((ip, bits))
}

/// One IPv6 `IPAM.Config` entry.
fn parse_ipam6(c: &serde_json::Value, subnet: &str) -> Result<Ipam6, String> {
    let field = |k: &str| c[k].as_str().unwrap_or("").to_owned();
    let (net, prefix) = parse_cidr6(subnet, "subnet")?;
    // A network needs room for the gateway and at least one container.
    if !(1..=126).contains(&prefix) {
        return Err(format!("invalid subnet {subnet}: Kiri supports IPv6 prefixes from /1 to /126"));
    }
    let inside = |ip: [u8; 16]| in_subnet6(ip, net, prefix);
    let ip_range = match field("IPRange") {
        r if r.is_empty() => None,
        r => {
            let (rn, rp) = parse_cidr6(&r, "ip-range")?;
            if rp < prefix || !inside(rn) {
                return Err(format!("invalid ip-range {r}: parent subnet {subnet} does not contain ip-range"));
            }
            Some((rn, rp))
        }
    };
    let address = |s: &str, what: &str| -> Result<[u8; 16], String> {
        let ip = parse_ip6(s).ok_or_else(|| format!("invalid {what} {s}: invalid address"))?;
        if !inside(ip) {
            return Err(format!("invalid {what} {s}: parent subnet {subnet} does not contain ip address {s}"));
        }
        Ok(ip)
    };
    let gateway = match field("Gateway") {
        g if g.is_empty() => None,
        g => Some(address(&g, "gateway")?),
    };
    let mut aux = BTreeMap::new();
    if let Some(map) = c["AuxiliaryAddresses"].as_object() {
        for (name, ip) in map {
            aux.insert(name.clone(), address(ip.as_str().unwrap_or(""), &format!("auxiliary address {name}"))?);
        }
    }
    Ok(Ipam6 { subnet: net, prefix, gateway, ip_range, aux })
}

/// The `IPAM` object of a network create request: the default driver, at
/// most one IPv4 and one IPv6 pool.
pub fn parse_ipam(v: &serde_json::Value) -> Result<Option<Ipam>, String> {
    parse_ipam_both(v).map(|(v4, _)| v4)
}

/// `parse_ipam`, with the IPv6 entry too.
pub fn parse_ipam_both(v: &serde_json::Value) -> Result<(Option<Ipam>, Option<Ipam6>), String> {
    match v["Driver"].as_str().unwrap_or("") {
        "" | "default" => {}
        d => return Err(format!("Kiri supports only the default IPAM driver, not {d}")),
    }
    let configs = v["Config"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let mut out: Option<Ipam> = None;
    let mut out6: Option<Ipam6> = None;
    for c in configs {
        let field = |k: &str| c[k].as_str().unwrap_or("").to_owned();
        let subnet = field("Subnet");
        if subnet.contains(':') {
            if out6.is_some() {
                return Err("Kiri supports one IPv6 subnet per network".into());
            }
            out6 = Some(parse_ipam6(c, &subnet)?);
            continue;
        }
        if subnet.is_empty() {
            if field("Gateway").is_empty() && field("IPRange").is_empty() {
                continue;
            }
            return Err("invalid network config: an IP range or gateway needs a subnet".into());
        }
        if out.is_some() {
            return Err("Kiri supports one IPv4 subnet per network".into());
        }
        let (net, prefix) = parse_cidr(&subnet, "subnet")?;
        if prefix > 30 || prefix < 8 {
            return Err(format!("invalid subnet {subnet}: Kiri supports prefixes from /8 to /30"));
        }
        let inside = |ip: [u8; 4]| in_subnet(ip, net, prefix);
        let ip_range = match field("IPRange") {
            r if r.is_empty() => None,
            r => {
                let (rn, rp) = parse_cidr(&r, "ip-range")?;
                if rp < prefix || !inside(rn) {
                    return Err(format!("invalid ip-range {r}: parent subnet {subnet} does not contain ip-range"));
                }
                Some((rn, rp))
            }
        };
        let address = |s: &str, what: &str| -> Result<[u8; 4], String> {
            let ip = parse_ip(s).ok_or_else(|| format!("invalid {what} {s}: invalid address"))?;
            if !inside(ip) {
                return Err(format!("invalid {what} {s}: parent subnet {subnet} does not contain ip address {s}"));
            }
            Ok(ip)
        };
        let gateway = match field("Gateway") {
            g if g.is_empty() => None,
            g => Some(address(&g, "gateway")?),
        };
        let mut aux = BTreeMap::new();
        if let Some(map) = c["AuxiliaryAddresses"].as_object() {
            for (name, ip) in map {
                aux.insert(name.clone(), address(ip.as_str().unwrap_or(""), &format!("auxiliary address {name}"))?);
            }
        }
        out = Some(Ipam { subnet: net, prefix, gateway, ip_range, aux });
    }
    Ok((out, out6))
}

impl Networks {
    pub fn load(root: &std::path::Path) -> io::Result<Networks> {
        let path = root.join("networks.json");
        let saved: Vec<Network> = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice::<Vec<Network>>(&b).unwrap_or_default().into_iter().filter(|n| !n.builtin()).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        // An older Kiri stored any value for this option and ignored it: one
        // that is not a boolean still leaves the containers connected.
        for n in &saved {
            if let Err(e) = icc_option(&n.options) {
                crate::diag::warn!("enable_icc is not a boolean: the network's containers stay connected", network = n.name.clone(), error = e);
            }
        }
        // 172.17.0.0/16 as Docker uses, unless the host already routes it
        // (Kiri on a Linux host inside another engine's container) or a
        // saved network has it.
        let taken: Vec<([u8; 4], u8)> = saved.iter().map(|n| (n.subnet, n.prefix)).collect();
        let (subnet, prefix) = free_subnet(&taken, true).unwrap_or(([172, 17, 0, 0], 16));
        let mut bridge = builtin("bridge", "bridge", "kiri0", subnet, [subnet[0], subnet[1], subnet[2], 1]);
        bridge.prefix = prefix;
        // `KIRI_BRIDGE_IPV6=1`, as Docker's daemon.json `"ipv6": true`: the
        // default bridge gets an IPv6 /64 too. Off by default, as Docker's.
        let ula = ula_prefix(root);
        if std::env::var("KIRI_BRIDGE_IPV6").is_ok_and(|v| v == "1" || v == "true") {
            let taken6: Vec<([u8; 16], u8)> = saved.iter().filter_map(|n| n.v6.as_ref()).map(|v| (v.subnet, v.prefix)).collect();
            if let Some((s6, p6)) = free_subnet6(ula, "bridge", &taken6) {
                let gateway = (u128::from_be_bytes(s6) + 1).to_be_bytes();
                bridge.v6 = Some(V6 { subnet: s6, prefix: p6, gateway, user_subnet: false, ip_range: None, aux: BTreeMap::new(), used: BTreeSet::new() });
            }
        }
        let mut list = vec![bridge, builtin("host", "host", "", [0; 4], [0; 4]), builtin("none", "null", "", [0; 4], [0; 4])];
        list.extend(saved);
        Ok(Networks { list, path, ula })
    }

    fn save(&self) {
        let user: Vec<&Network> = self.list.iter().filter(|n| !n.builtin()).collect();
        if let Ok(json) = serde_json::to_vec(&user) {
            let tmp = self.path.with_extension("json.tmp");
            if let Err(e) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &self.path)) {
                crate::diag::error!("saving networks failed", error = e);
            }
        }
    }

    pub fn all(&self) -> &[Network] {
        &self.list
    }

    /// By name, full id or id prefix. `default` is Docker's alias for `bridge`.
    pub fn find(&self, key: &str) -> Option<&Network> {
        let key = if key == "default" || key.is_empty() { "bridge" } else { key };
        self.list
            .iter()
            .find(|n| n.name == key || n.id == key)
            .or_else(|| self.list.iter().find(|n| key.len() >= 6 && n.id.starts_with(key)))
    }

    fn find_mut(&mut self, name: &str) -> Option<&mut Network> {
        self.list.iter_mut().find(|n| n.name == name)
    }

    pub fn create(&mut self, name: &str, s: Settings) -> Result<Network, String> {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) {
            return Err(format!("invalid network name {name:?}"));
        }
        if self.find(name).is_some_and(|n| n.name == name) {
            return Err(format!("network with name {name} already exists"));
        }
        if s.no_ipv4 && !s.ipv6 {
            return Err("invalid network config: IPv4 or IPv6 must be enabled".into());
        }
        if s.no_ipv4 && s.ipam.is_some() {
            return Err("invalid network config: an IPv4 subnet was given, but IPv4 is disabled (--ipv4=false)".into());
        }
        if s.macvlan {
            check_macvlan(&s.options)?;
        } else {
            mtu_option(&s.options, s.ipv6)?;
            icc_option(&s.options)?;
        }
        if s.ipvlan {
            check_ipvlan(&s.options)?;
        }
        let own_link = s.macvlan || s.ipvlan;
        let taken: Vec<([u8; 4], u8)> = self.list.iter().filter(|n| !n.bridge.is_empty()).map(|n| (n.subnet, n.prefix)).collect();
        let (subnet, gateway) = match &s.ipam {
            Some(ipam) => {
                if taken.iter().any(|t| overlaps(*t, (ipam.subnet, ipam.prefix))) {
                    return Err("invalid pool request: Pool overlaps with other one on this address space".into());
                }
                // Docker's default gateway: the subnet's first address. A
                // macvlan or ipvlan network has one only when it is asked for.
                let first = if own_link { [0; 4] } else { (u32::from_be_bytes(ipam.subnet) + 1).to_be_bytes() };
                ((ipam.subnet, ipam.prefix), ipam.gateway.unwrap_or(first))
            }
            None => {
                let subnet = free_subnet(&taken, false).ok_or("no free subnet left")?;
                let mut gateway = subnet.0;
                gateway[3] = 1;
                (subnet, if own_link { [0; 4] } else { gateway })
            }
        };
        let v6 = if s.ipv6 {
            let taken6: Vec<([u8; 16], u8)> = self.list.iter().filter_map(|n| n.v6.as_ref()).map(|v| (v.subnet, v.prefix)).collect();
            let (subnet, prefix, user) = match &s.ipam6 {
                Some(i) => {
                    if taken6.iter().any(|t| overlaps6(*t, (i.subnet, i.prefix))) {
                        return Err("invalid pool request: Pool overlaps with other one on this address space".into());
                    }
                    (i.subnet, i.prefix, true)
                }
                None => {
                    let (s, p) = free_subnet6(self.ula, name, &taken6).ok_or("no free IPv6 subnet left")?;
                    (s, p, false)
                }
            };
            let first = if own_link { [0; 16] } else { (u128::from_be_bytes(subnet) + 1).to_be_bytes() };
            Some(V6 {
                subnet,
                prefix,
                gateway: s.ipam6.as_ref().and_then(|i| i.gateway).unwrap_or(first),
                user_subnet: user,
                ip_range: s.ipam6.as_ref().and_then(|i| i.ip_range),
                aux: s.ipam6.as_ref().map(|i| i.aux.clone()).unwrap_or_default(),
                used: BTreeSet::new(),
            })
        } else {
            if s.ipam6.is_some() {
                return Err("invalid network config: an IPv6 subnet needs IPv6 enabled (--ipv6, Compose enable_ipv6: true)".into());
            }
            None
        };
        // A macvlan or ipvlan network's link is named apart from the
        // bridges, so a name is never taken up again as another kind.
        let (driver, device) = match (s.macvlan, s.ipvlan) {
            (true, _) => ("macvlan", "kirim"),
            (false, true) => ("ipvlan", "kirii"),
            (false, false) => ("bridge", "kiri"),
        };
        let index = (1..).find(|i| !self.list.iter().any(|n| n.bridge == format!("{device}{i}"))).expect("unbounded");
        let n = Network {
            id: super::engine::random_hex(32),
            name: name.into(),
            driver: driver.into(),
            bridge: format!("{device}{index}"),
            subnet: subnet.0,
            prefix: subnet.1,
            gateway,
            created_ns: now_ns(),
            labels: s.labels,
            internal: s.internal,
            user_subnet: s.ipam.is_some(),
            ip_range: s.ipam.as_ref().and_then(|i| i.ip_range),
            aux: s.ipam.map(|i| i.aux).unwrap_or_default(),
            options: if s.options.is_object() { s.options } else { serde_json::json!({}) },
            ipam_options: s.ipam_options,
            attachable: s.attachable,
            no_ipv4: s.no_ipv4,
            v6,
            used: BTreeSet::new(),
        };
        self.list.push(n.clone());
        self.save();
        Ok(n)
    }

    pub fn remove(&mut self, key: &str) -> Result<(), String> {
        let name = self.find(key).map(|n| n.name.clone()).ok_or_else(|| format!("network {key} not found"))?;
        let n = self.find_mut(&name).expect("found");
        if n.builtin() {
            return Err(format!("{name} is a pre-defined network and cannot be removed"));
        }
        if !n.used.is_empty() || n.v6.as_ref().is_some_and(|v| !v.used.is_empty()) {
            return Err(format!("error while removing network: network {name} has active endpoints"));
        }
        self.list.retain(|n| n.name != name);
        self.save();
        Ok(())
    }

    /// A free address that none of `avoid` (fixed addresses other
    /// containers will ask for) is, so those still get theirs.
    pub fn allocate_avoiding(&mut self, name: &str, avoid: &[[u8; 4]]) -> Option<[u8; 4]> {
        self.find_mut(name).and_then(|n| n.allocate_avoiding(avoid))
    }

    /// Takes a fixed address on a network, or says why it cannot.
    pub fn reserve(&mut self, name: &str, ip: [u8; 4]) -> Result<(), String> {
        self.find_mut(name).ok_or_else(|| format!("network {name} not found"))?.reserve(ip)
    }

    pub fn release(&mut self, name: &str, ip: [u8; 4]) {
        if let Some(n) = self.find_mut(name) {
            n.release(ip);
        }
    }

    /// An IPv6 address on an `--ipv6` network: the fixed one asked for, or
    /// the next free one past `avoid`. None for a network without IPv6.
    pub fn allocate6(&mut self, name: &str, want: Option<[u8; 16]>, avoid: &[[u8; 16]]) -> Result<Option<[u8; 16]>, String> {
        let n = self.find_mut(name).ok_or_else(|| format!("network {name} not found"))?;
        if n.v6.is_none() {
            return match want {
                Some(ip) => n.check_static6(ip).map(|()| None),
                None => Ok(None),
            };
        }
        match want {
            Some(ip) => n.reserve6(ip).map(|()| Some(ip)),
            None => n.v6.as_mut().expect("checked").allocate_avoiding(avoid).map(Some).ok_or_else(|| format!("no free IPv6 address left on network {name}")),
        }
    }

    pub fn release6(&mut self, name: &str, ip: [u8; 16]) {
        if let Some(v6) = self.find_mut(name).and_then(|n| n.v6.as_mut()) {
            v6.release(ip);
        }
    }

    pub fn in_use(&self, name: &str) -> bool {
        self.list.iter().any(|n| n.name == name && !n.used.is_empty())
    }
}

/// The first free subnet: 172.17.0.0/16 (only for the default bridge), then
/// 172.18.0.0/16 … 172.31.0.0/16, then 10.89.N.0/24, skipping ones Kiri
/// already uses and ones the host routes.
fn free_subnet(taken: &[([u8; 4], u8)], default_bridge: bool) -> Option<([u8; 4], u8)> {
    let routes = host_routes();
    let first = if default_bridge { 17 } else { 18 };
    (first..=31u8)
        .map(|b| ([172, b, 0, 0], 16u8))
        .chain((0..=255u8).map(|c| ([10, 89, c, 0], 24u8)))
        .find(|(s, p)| {
            let (net, mask) = (u32::from_be_bytes(*s), u32::MAX << (32 - p));
            !taken.iter().any(|t| overlaps(*t, (*s, *p))) && !routes.iter().any(|(rn, rm)| (net & rm) == *rn || (rn & mask) == net)
        })
}

/// The host's own IPv6 routes, outside Kiri's bridges, the default route
/// and link-local and multicast ones. Only a Linux host shares its routing
/// table with Kiri.
#[cfg(target_os = "linux")]
fn host_routes6() -> Vec<([u8; 16], u8)> {
    let text = std::fs::read_to_string("/proc/net/ipv6_route").unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            // dest prefix src srcprefix next-hop metric ref use flags iface, in hex
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 10 || f[9].starts_with("kiri") || f[9] == "lo" {
                return None;
            }
            let dest = u128::from_str_radix(f[0], 16).ok()?.to_be_bytes();
            let prefix = u8::from_str_radix(f[1], 16).ok()?;
            let special = dest[0] == 0xff || (dest[0] == 0xfe && dest[1] & 0xc0 == 0x80);
            (prefix != 0 && !special).then_some((dest, prefix))
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn host_routes6() -> Vec<([u8; 16], u8)> {
    Vec::new()
}

/// The host's own routes as (network, mask), outside Kiri's bridges and the
/// default route. Only a Linux host shares its routing table with Kiri.
#[cfg(target_os = "linux")]
fn host_routes() -> Vec<(u32, u32)> {
    let text = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 8 || f[0].starts_with("kiri") {
                return None;
            }
            // Little-endian hex, as the kernel prints them.
            let dest = u32::from_str_radix(f[1], 16).ok()?.swap_bytes();
            let mask = u32::from_str_radix(f[7], 16).ok()?.swap_bytes();
            (mask != 0).then_some((dest & mask, mask))
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn host_routes() -> Vec<(u32, u32)> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(ipam: Option<Ipam>) -> Network {
        let mut n = builtin("x", "bridge", "kiri9", [10, 5, 0, 0], [10, 5, 0, 1]);
        n.name = "app".into();
        n.prefix = 24;
        if let Some(i) = ipam {
            n.user_subnet = true;
            n.ip_range = i.ip_range;
            n.aux = i.aux;
            if let Some(g) = i.gateway {
                n.gateway = g;
            }
        }
        n
    }

    #[test]
    fn ipam_config_is_read_and_checked() {
        let v = serde_json::json!({"Driver": "default", "Config": [{"Subnet": "10.5.0.0/24", "Gateway": "10.5.0.254", "IPRange": "10.5.0.128/25", "AuxiliaryAddresses": {"router": "10.5.0.200"}}]});
        let i = parse_ipam(&v).unwrap().unwrap();
        assert_eq!((i.subnet, i.prefix, i.gateway, i.ip_range), ([10, 5, 0, 0], 24, Some([10, 5, 0, 254]), Some(([10, 5, 0, 128], 25))));
        assert_eq!(i.aux.get("router"), Some(&[10, 5, 0, 200]));
        // Compose without an ipam block, and the CLI without --subnet.
        assert_eq!(parse_ipam(&serde_json::json!({"Driver": "default", "Config": []})).unwrap(), None);
        assert_eq!(parse_ipam(&serde_json::Value::Null).unwrap(), None);

        let bad = |cfg: serde_json::Value| parse_ipam(&serde_json::json!({"Config": [cfg]})).unwrap_err();
        assert_eq!(bad(serde_json::json!({"Subnet": "10.5.0.1/24"})), "invalid subnet 10.5.0.1/24: it should be 10.5.0.0/24");
        assert!(bad(serde_json::json!({"Subnet": "nope"})).contains("invalid CIDR block notation"));
        assert!(bad(serde_json::json!({"Subnet": "10.5.0.0/24", "Gateway": "10.6.0.1"})).contains("does not contain"));
        assert!(bad(serde_json::json!({"Subnet": "10.5.0.0/24", "IPRange": "10.5.0.0/16"})).contains("ip-range"));
        assert!(bad(serde_json::json!({"Subnet": "10.5.0.0/24", "AuxiliaryAddresses": {"a": "10.9.0.1"}})).contains("auxiliary address a"));
        assert!(parse_ipam(&serde_json::json!({"Driver": "other"})).is_err());
    }

    #[test]
    fn ipv6_ipam_config_is_read_and_checked() {
        let v = serde_json::json!({"Config": [
            {"Subnet": "10.5.0.0/24"},
            {"Subnet": "fd00:5::/64", "Gateway": "fd00:5::fe", "IPRange": "fd00:5::100/120", "AuxiliaryAddresses": {"r": "fd00:5::200"}},
        ]});
        let (v4, v6) = parse_ipam_both(&v).unwrap();
        assert_eq!(v4.unwrap().subnet, [10, 5, 0, 0]);
        let v6 = v6.unwrap();
        assert_eq!((ip6_str(v6.subnet), v6.prefix), ("fd00:5::".to_owned(), 64));
        assert_eq!(v6.gateway.map(ip6_str).as_deref(), Some("fd00:5::fe"));
        assert_eq!(v6.ip_range.map(|(a, p)| (ip6_str(a), p)), Some(("fd00:5::100".to_owned(), 120)));
        assert_eq!(v6.aux.get("r").copied().map(ip6_str).as_deref(), Some("fd00:5::200"));
        // An IPv6 entry alone is fine (the IPv4 subnet comes from the pool).
        let (v4, v6) = parse_ipam_both(&serde_json::json!({"Config": [{"Subnet": "fc00:f853:ccd:e793::/64"}]})).unwrap();
        assert!(v4.is_none() && v6.is_some());

        let bad = |cfg: serde_json::Value| parse_ipam_both(&serde_json::json!({"Config": [cfg]})).unwrap_err();
        assert_eq!(bad(serde_json::json!({"Subnet": "fd00:5::1/64"})), "invalid subnet fd00:5::1/64: it should be fd00:5::/64");
        assert!(bad(serde_json::json!({"Subnet": "fd00:5::/64", "Gateway": "fd00:6::1"})).contains("does not contain"));
        assert!(bad(serde_json::json!({"Subnet": "fd00:5::/64", "Gateway": "10.0.0.1"})).contains("invalid address"));
        assert!(bad(serde_json::json!({"Subnet": "fd00:5::/127"})).contains("prefixes"));
        assert!(bad(serde_json::json!({"Subnet": "fd00:5::/64", "IPRange": "fd00:5::/48"})).contains("ip-range"));
        let two = serde_json::json!({"Config": [{"Subnet": "fd00:5::/64"}, {"Subnet": "fd00:6::/64"}]});
        assert!(parse_ipam_both(&two).unwrap_err().contains("one IPv6 subnet"));
    }

    fn nets() -> (Networks, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("kiri-net-test-{}-{}", std::process::id(), now_ns()));
        std::fs::create_dir_all(&dir).unwrap();
        (Networks::load(&dir).unwrap(), dir)
    }

    #[test]
    fn a_macvlan_network_has_its_own_link_and_a_gateway_only_when_given_one() {
        let (mut n, dir) = nets();
        let macvlan = |options: serde_json::Value, ipam: Option<Ipam>| Settings { macvlan: true, options, ipam, ..Default::default() };
        let a = n.create("a", macvlan(serde_json::json!({}), None)).unwrap();
        assert_eq!((a.driver.as_str(), a.bridge.as_str(), a.gateway, a.macvlan_mode()), ("macvlan", "kirim1", [0; 4], Some("bridge")));
        // The subnet's first address is a container's, as with Docker.
        assert_eq!(n.allocate_avoiding("a", &[]), Some((u32::from_be_bytes(a.subnet) + 1).to_be_bytes()));
        // A bridge network takes its name from the bridges' own numbers.
        let b = n.create("b", Settings::default()).unwrap();
        assert_eq!((b.driver.as_str(), b.bridge.as_str(), b.macvlan_mode()), ("bridge", "kiri1", None));
        // A gateway that was asked for is kept, and kept from containers.
        let ipam = Ipam { subnet: [10, 5, 0, 0], prefix: 24, gateway: Some([10, 5, 0, 1]), ip_range: None, aux: BTreeMap::new() };
        let c = n.create("c", macvlan(serde_json::json!({"macvlan_mode": "private"}), Some(ipam))).unwrap();
        assert_eq!((c.bridge.as_str(), c.gateway, c.macvlan_mode()), ("kirim2", [10, 5, 0, 1], Some("private")));
        assert_eq!(n.allocate_avoiding("c", &[]), Some([10, 5, 0, 2]));
        // IPv6 the same way.
        let six = n.create("six", Settings { macvlan: true, ..v6(None) }).unwrap();
        let v = six.v6.unwrap();
        assert_eq!(v.gateway, [0; 16]);
        assert_eq!(n.allocate6("six", None, &[]), Ok(Some((u128::from_be_bytes(v.subnet) + 1).to_be_bytes())));
        // The mode is checked in Docker's words; a parent is Kiri's to refuse.
        assert_eq!(
            n.create("d", macvlan(serde_json::json!({"macvlan_mode": "bogus"}), None)).unwrap_err(),
            "requested macvlan mode 'bogus' is not valid, 'bridge' mode is the macvlan driver default"
        );
        assert!(n.create("e", macvlan(serde_json::json!({"parent": "eth0"}), None)).unwrap_err().contains("no parent interface"));
        assert!(n.create("e", macvlan(serde_json::json!({"parent": 5}), None)).unwrap_err().contains("not a string"));
        assert!(n.create("e", macvlan(serde_json::json!({"macvlan_mode": 7}), None)).unwrap_err().contains("not a string"));
        // On a bridge network they are options like any other.
        assert!(n.create("f", Settings { options: serde_json::json!({"macvlan_mode": "bogus", "parent": "eth0"}), ..Default::default() }).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_ipvlan_network_has_its_own_link_its_mode_and_its_flag() {
        let (mut n, dir) = nets();
        let ipvlan = |options: serde_json::Value| Settings { ipvlan: true, options, ..Default::default() };
        let a = n.create("a", ipvlan(serde_json::json!({}))).unwrap();
        assert_eq!((a.driver.as_str(), a.bridge.as_str(), a.gateway, a.ipvlan(), a.macvlan_mode()), ("ipvlan", "kirii1", [0; 4], Some(("l2", "bridge")), None));
        assert!(a.own_link());
        // The subnet's first address is a container's, as with Docker.
        assert_eq!(n.allocate_avoiding("a", &[]), Some((u32::from_be_bytes(a.subnet) + 1).to_be_bytes()));
        // Bridge and macvlan networks take their names from their own numbers.
        let b = n.create("b", Settings::default()).unwrap();
        assert_eq!((b.bridge.as_str(), b.ipvlan(), b.own_link()), ("kiri1", None, false));
        let m = n.create("m", Settings { macvlan: true, ..Default::default() }).unwrap();
        assert_eq!((m.bridge.as_str(), m.ipvlan(), m.own_link()), ("kirim1", None, true));
        let c = n.create("c", ipvlan(serde_json::json!({"ipvlan_mode": "l3", "ipvlan_flag": "private"}))).unwrap();
        assert_eq!((c.bridge.as_str(), c.ipvlan()), ("kirii2", Some(("l3", "private"))));
        // A MAC address is refused on ipvlan alone.
        assert_eq!(c.check_mac("02:42:0a:05:00:09").unwrap_err(), "ipvlan interfaces do not support custom mac address assignment");
        assert!(c.check_mac("").is_ok() && b.check_mac("02:42:0a:05:00:09").is_ok() && m.check_mac("02:42:0a:05:00:09").is_ok());
        // IPv6 the same way.
        let six = n.create("six", Settings { ipvlan: true, ..v6(None) }).unwrap();
        assert_eq!(six.v6.unwrap().gateway, [0; 16]);
        // The mode and the flag are checked in Docker's words; a parent is Kiri's to refuse.
        assert_eq!(
            n.create("d", ipvlan(serde_json::json!({"ipvlan_mode": "bogus"}))).unwrap_err(),
            "requested ipvlan mode 'bogus' is not valid, 'l2' mode is the ipvlan driver default"
        );
        assert_eq!(
            n.create("d", ipvlan(serde_json::json!({"ipvlan_flag": "bogus"}))).unwrap_err(),
            "requested ipvlan flag 'bogus' is not valid, 'bridge' is the ipvlan driver default"
        );
        assert!(n.create("e", ipvlan(serde_json::json!({"parent": "eth0"}))).unwrap_err().contains("Kiri's ipvlan networks have no parent interface"));
        assert!(n.create("e", ipvlan(serde_json::json!({"ipvlan_mode": 7}))).unwrap_err().contains("not a string"));
        assert!(n.create("e", ipvlan(serde_json::json!({"ipvlan_flag": false}))).unwrap_err().contains("not a string"));
        // A macvlan network's options are not an ipvlan network's to check, nor the other way.
        assert!(n.create("f", ipvlan(serde_json::json!({"macvlan_mode": "bogus"}))).is_ok());
        assert!(n.create("g", Settings { macvlan: true, options: serde_json::json!({"ipvlan_mode": "bogus"}), ..Default::default() }).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ipv6_only_networks_need_ipv6_and_no_ipv4_subnet() {
        let dir = std::env::temp_dir().join(format!("kiri-net6only-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut n = Networks::load(&dir).unwrap();
        let only = n.create("only6", Settings { no_ipv4: true, ..v6(None) }).unwrap();
        assert!(only.no_ipv4 && only.v6.is_some());
        assert!(n.create("neither", Settings { no_ipv4: true, ..Default::default() }).unwrap_err().contains("IPv4 or IPv6 must be enabled"));
        let ipam = parse_ipam(&serde_json::json!({"Config": [{"Subnet": "10.77.0.0/24"}]})).unwrap();
        assert!(n.create("both", Settings { no_ipv4: true, ipam, ..v6(None) }).unwrap_err().contains("IPv4 is disabled"));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn v6(ipam6: Option<Ipam6>) -> Settings {
        Settings { labels: serde_json::json!({}), ipv6: true, ipam6, ..Default::default() }
    }

    #[test]
    fn ipv6_networks_get_a_ula_subnet_and_addresses() {
        let (mut n, dir) = nets();
        // The default bridge is IPv4-only, as Docker's.
        assert!(n.find("bridge").unwrap().v6.is_none());
        let a = n.create("a", v6(None)).unwrap();
        let b = n.create("b", v6(None)).unwrap();
        let (va, vb) = (a.v6.clone().unwrap(), b.v6.clone().unwrap());
        assert_eq!((va.subnet[0], va.prefix), (0xfd, 64));
        assert_eq!(va.subnet[..6], vb.subnet[..6], "one /48 per engine");
        assert!(!overlaps6((va.subnet, 64), (vb.subnet, 64)));
        assert_eq!(va.gateway, (u128::from_be_bytes(va.subnet) + 1).to_be_bytes());
        assert!(!va.user_subnet);
        // The same name gets the same subnet again.
        n.remove("a").unwrap();
        assert_eq!(n.create("a", v6(None)).unwrap().v6.unwrap().subnet, va.subnet);
        // Addresses: ::2 up, past the gateway; released ones come back.
        let first = n.allocate6("a", None, &[]).unwrap().unwrap();
        assert_eq!(ip6_str(first), ip6_str((u128::from_be_bytes(va.subnet) + 2).to_be_bytes()));
        assert!(n.remove("a").unwrap_err().contains("active endpoints"));
        n.release6("a", first);
        n.remove("a").unwrap();
        // IPv4-only networks hand out none, and refuse a fixed one.
        n.create("plain", Settings::default()).unwrap();
        assert_eq!(n.allocate6("plain", None, &[]), Ok(None));
        assert!(n.allocate6("plain", parse_ip6("fd00::5"), &[]).unwrap_err().contains("does not have IPv6 enabled"));
        // Persisted.
        let again = Networks::load(&dir).unwrap();
        assert_eq!(again.find("b").unwrap().v6.as_ref().unwrap().subnet, vb.subnet);
        assert!(again.find("plain").unwrap().v6.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ipv6_subnets_and_fixed_addresses_follow_dockers_rules() {
        let (mut n, dir) = nets();
        let ipam = |s: &str| parse_ipam_both(&serde_json::json!({"Config": [{"Subnet": s}]})).unwrap().1;
        let k = n.create("kind", v6(ipam("fc00:f853:ccd:e793::/64"))).unwrap();
        let kv = k.v6.unwrap();
        assert_eq!(kv.subnet_str(), "fc00:f853:ccd:e793::/64");
        assert_eq!(ip6_str(kv.gateway), "fc00:f853:ccd:e793::1");
        assert!(n.create("again", v6(ipam("fc00:f853:ccd:e793::/80"))).unwrap_err().contains("Pool overlaps"));
        // A subnet without --ipv6 is refused, not dropped.
        let no = Settings { ipam6: ipam("fd00:9::/64"), ..Default::default() };
        assert!(n.create("no", no).unwrap_err().contains("IPv6 enabled"));
        // Fixed addresses: in the subnet, free, not the gateway.
        let want = parse_ip6("fc00:f853:ccd:e793::10");
        assert_eq!(n.allocate6("kind", want, &[]), Ok(want));
        assert!(n.allocate6("kind", want, &[]).unwrap_err().starts_with("Address already in use"));
        assert!(n.allocate6("kind", parse_ip6("fc00:f853:ccd:e793::1"), &[]).unwrap_err().starts_with("Address already in use"));
        assert!(n.allocate6("kind", parse_ip6("fd00:1::5"), &[]).unwrap_err().contains("no configured subnet"));
        // Automatic ones pass over addresses other containers asked for.
        let skip = parse_ip6("fc00:f853:ccd:e793::2").unwrap();
        assert_eq!(n.allocate6("kind", None, &[skip]).unwrap().map(ip6_str).as_deref(), Some("fc00:f853:ccd:e793::3"));
        // A pool subnet takes fixed addresses too, as Docker 29 does (moby#51569).
        n.create("pool", v6(None)).unwrap();
        let pv = n.find("pool").unwrap().v6.clone().unwrap();
        let inside = (u128::from_be_bytes(pv.subnet) + 9).to_be_bytes();
        assert_eq!(n.allocate6("pool", Some(inside), &[]), Ok(Some(inside)));
        assert!(n.allocate6("pool", parse_ip6("fd00:dead::9"), &[]).unwrap_err().contains("no configured subnet"));
        // A range: only from there.
        let r = parse_ipam_both(&serde_json::json!({"Config": [{"Subnet": "fd00:7::/64", "IPRange": "fd00:7::40/126"}]})).unwrap().1;
        n.create("ranged", v6(r)).unwrap();
        let got: Vec<String> = (0..4).filter_map(|_| n.allocate6("ranged", None, &[]).unwrap()).map(ip6_str).collect();
        assert_eq!(got, ["fd00:7::40", "fd00:7::41", "fd00:7::42", "fd00:7::43"]);
        assert!(n.allocate6("ranged", None, &[]).unwrap_err().contains("no free IPv6 address"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn overlapping_subnets_are_found() {
        assert!(overlaps(([172, 18, 0, 0], 16), ([172, 18, 5, 0], 24)));
        assert!(overlaps(([172, 18, 5, 0], 24), ([172, 18, 0, 0], 16)));
        assert!(!overlaps(([172, 18, 0, 0], 16), ([172, 19, 0, 0], 16)));
        // A user's 172.18.0.0/24 keeps the automatic choice off 172.18.0.0/16.
        let free = free_subnet(&[([172, 17, 0, 0], 16), ([172, 18, 0, 0], 24)], false).unwrap();
        assert!(!overlaps(free, ([172, 18, 0, 0], 24)) && !overlaps(free, ([172, 17, 0, 0], 16)));
    }

    #[test]
    fn addresses_come_from_the_range_and_skip_reserved_ones() {
        let mut aux = BTreeMap::new();
        aux.insert("r".to_owned(), [10, 5, 0, 129]);
        let mut n = net(Some(Ipam { subnet: [10, 5, 0, 0], prefix: 24, gateway: Some([10, 5, 0, 128]), ip_range: Some(([10, 5, 0, 128], 30)), aux }));
        // .128 is the gateway, .129 auxiliary: .130 and .131 are left.
        assert_eq!(n.allocate_avoiding(&[]), Some([10, 5, 0, 130]));
        assert_eq!(n.allocate_avoiding(&[]), Some([10, 5, 0, 131]));
        assert_eq!(n.allocate_avoiding(&[]), None);
        n.release([10, 5, 0, 130]);
        assert_eq!(n.allocate_avoiding(&[[10, 5, 0, 130]]), None);
        // Without a range: from .1 up, past the gateway.
        let mut d = net(None);
        assert_eq!(d.allocate_avoiding(&[]), Some([10, 5, 0, 2]));
        d.release([10, 5, 0, 2]);
        let mut g = net(Some(Ipam { subnet: [10, 5, 0, 0], prefix: 24, gateway: Some([10, 5, 0, 254]), ip_range: None, aux: BTreeMap::new() }));
        assert_eq!(g.allocate_avoiding(&[]), Some([10, 5, 0, 1]));
    }

    #[test]
    fn static_addresses_follow_dockers_rules() {
        let mut n = net(Some(Ipam { subnet: [10, 5, 0, 0], prefix: 24, gateway: None, ip_range: Some(([10, 5, 0, 128], 25)), aux: BTreeMap::new() }));
        // Outside the range but inside the subnet is fine, as with Docker.
        assert_eq!(n.reserve([10, 5, 0, 10]), Ok(()));
        assert!(n.reserve([10, 5, 0, 10]).unwrap_err().starts_with("Address already in use"));
        assert!(n.reserve([10, 5, 0, 1]).unwrap_err().starts_with("Address already in use"));
        assert!(n.check_static([10, 6, 0, 10]).unwrap_err().contains("no configured subnet"));
        assert!(n.check_static([10, 5, 0, 255]).is_err());
        n.release([10, 5, 0, 10]);
        assert_eq!(n.reserve([10, 5, 0, 10]), Ok(()));
        assert!(net(None).check_static([10, 5, 0, 10]).unwrap_err().contains("user configured subnets"));
        let mut b = net(None);
        b.name = "bridge".into();
        assert!(b.check_static([10, 5, 0, 10]).unwrap_err().contains("user defined networks only"));
    }

    #[test]
    fn the_mtu_option_is_checked_as_docker_does() {
        let mtu = |v: &str, ipv6| mtu_option(&serde_json::json!({ MTU_OPTION: v }), ipv6);
        assert_eq!(mtu_option(&serde_json::json!({}), false), Ok(None));
        assert_eq!(mtu_option(&serde_json::Value::Null, false), Ok(None));
        assert_eq!(mtu("1400", false), Ok(Some(1400)));
        assert_eq!(mtu("+1400", false), Ok(Some(1400)));
        // 0 leaves the default, and the limits are the bridge's own.
        assert_eq!(mtu("0", true), Ok(None));
        assert_eq!(mtu("68", false), Ok(Some(68)));
        assert_eq!(mtu("65535", false), Ok(Some(65535)));
        assert!(mtu("67", false).is_err());
        assert!(mtu("65536", false).is_err());
        assert_eq!(mtu("-5", false).unwrap_err(), "invalid MTU number: -5");
        assert_eq!(mtu("abc", false).unwrap_err(), "failed to parse com.docker.network.driver.mtu value: abc (strconv.Atoi: parsing \"abc\": invalid syntax)");
        assert_eq!(mtu("99999999999999999999", false).unwrap_err(), "failed to parse com.docker.network.driver.mtu value: 99999999999999999999 (strconv.Atoi: parsing \"99999999999999999999\": value out of range)");
        assert!(mtu("-99999999999999999999", false).unwrap_err().ends_with("value out of range)"));
        assert!(mtu("9223372036854775807", false).unwrap_err().starts_with("invalid MTU number"));
        assert!(mtu(" 1400", false).is_err());
        assert!(mtu("", false).is_err());
        // IPv6 needs 1280.
        assert_eq!(mtu("1280", true), Ok(Some(1280)));
        assert!(mtu("1279", true).is_err());
        assert_eq!(mtu("1279", false), Ok(Some(1279)));
        // A network refuses a bad one and keeps a good one for the agent.
        let (mut nets, dir) = nets();
        let options = |v: &str| Settings { options: serde_json::json!({ MTU_OPTION: v }), ..Settings::default() };
        assert!(nets.create("bad", options("abc")).unwrap_err().contains(MTU_OPTION));
        assert!(nets.find("bad").is_none());
        assert_eq!(nets.create("small", options("1400")).unwrap().mtu(), Some(1400));
        assert_eq!(nets.create("plain", Settings::default()).unwrap().mtu(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_icc_option_is_read_as_docker_does() {
        let icc = |v: &str| icc_option(&serde_json::json!({ ICC_OPTION: v }));
        assert_eq!(icc_option(&serde_json::json!({})), Ok(true));
        assert_eq!(icc_option(&serde_json::Value::Null), Ok(true));
        for on in ["1", "t", "T", "true", "TRUE", "True"] {
            assert_eq!(icc(on), Ok(true), "{on}");
        }
        for off in ["0", "f", "F", "false", "FALSE", "False"] {
            assert_eq!(icc(off), Ok(false), "{off}");
        }
        assert_eq!(icc("maybe").unwrap_err(), "failed to parse com.docker.network.bridge.enable_icc value: maybe (strconv.ParseBool: parsing \"maybe\": invalid syntax)");
        for bad in ["", "no", "tRUE", " true"] {
            assert!(icc(bad).is_err(), "{bad:?}");
        }
        // A network refuses a bad one and remembers a good one for the agent.
        let (mut nets, dir) = nets();
        let options = |v: &str| Settings { options: serde_json::json!({ ICC_OPTION: v }), ..Settings::default() };
        assert!(nets.create("bad", options("maybe")).unwrap_err().contains(ICC_OPTION));
        assert!(nets.find("bad").is_none());
        assert!(!nets.create("apart", options("false")).unwrap().icc());
        assert!(nets.create("together", options("true")).unwrap().icc());
        assert!(nets.create("plain", Settings::default()).unwrap().icc());
        // What an older Kiri stored is read the same way; a value it took
        // that is not a boolean leaves the network as it was then: on.
        assert!(!Networks::load(&dir).unwrap().find("apart").unwrap().icc());
        let file = dir.join("networks.json");
        let stored = std::fs::read_to_string(&file).unwrap();
        let old = stored.replace(&format!("{ICC_OPTION:?}:\"false\""), &format!("{ICC_OPTION:?}:\"off\""));
        assert_ne!(old, stored);
        std::fs::write(&file, old).unwrap();
        assert!(Networks::load(&dir).unwrap().find("apart").unwrap().icc());
        let _ = std::fs::remove_dir_all(dir);
    }
}
