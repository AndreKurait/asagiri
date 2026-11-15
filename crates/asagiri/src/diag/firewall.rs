//! Explicit, read-only Linux host firewall diagnostics. A DROP policy is
//! evidence of a boundary to inspect, not proof that a packet is dropped:
//! an earlier rule may already accept it. Never run this on container start.

use std::fmt::Write as _;
use std::io::{self, Read};
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const LIMIT: usize = 1 << 20;
const TIMEOUT: Duration = Duration::from_secs(2);

pub fn run(args: &[String]) -> io::Result<()> {
    if !args.is_empty() {
        return Err(io::Error::other("usage: asagiri diag firewall"));
    }
    // These fixed, read-only commands neither contact nor start an engine.
    // Missing tools and insufficient permissions remain visible as unknowns.
    let (text, complete) = inspect(|program, args| {
        let mut command = Command::new(program);
        command.args(args).env("LC_ALL", "C");
        capture(&mut command, TIMEOUT, LIMIT)
    });
    print!("{text}");
    if complete { Ok(()) } else {
        Err(io::Error::other("firewall inspection is incomplete; see the unavailable checks above"))
    }
}

/// Drain both pipes without waiting indefinitely for a hung command or an
/// inherited pipe. A private process group lets timeout cleanup reap helpers.
fn capture(command: &mut Command, timeout: Duration, limit: usize) -> io::Result<String> {
    // An inherited ignored SIGCHLD or an automatic/custom reaper can release
    // a child's PID before our group cleanup. Diagnose that before spawning.
    // SAFETY: zeroed sigaction storage is valid for this read-only query.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: read the process disposition without changing it.
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if action.sa_sigaction != libc::SIG_DFL || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(io::Error::new(io::ErrorKind::Unsupported,
            "SIGCHLD must retain child exit status for safe diagnostic cleanup"));
    }
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped())
        .stderr(Stdio::piped()).process_group(0).spawn()?;
    let result = (|| {
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            // SAFETY: each descriptor is a live, owned child pipe.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let deadline = Instant::now() + timeout;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (mut out_done, mut err_done) = (false, false);
        loop {
            out_done |= drain(&mut stdout, &mut out, limit)?;
            err_done |= drain(&mut stderr, &mut err, limit)?;
            if out_done && err_done && exited(child.id())? {
                return Ok((out, err));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "exceeded the diagnostic time limit"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    // Keep the leader unreaped until after cleanup, even if it exited first:
    // its PID cannot be reused for an unrelated process group in between.
    // ECHILD means an external reaper took ownership despite our preflight.
    // Never signal a numeric group whose leader PID we no longer own.
    if !matches!(exited(child.id()), Err(ref error) if error.raw_os_error() == Some(libc::ECHILD)) {
        // SAFETY: this is the group created for our unreaped child, not ours.
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    }
    let status = child.wait();
    let (out, err) = result?;
    let status = status?;
    if status.success() {
        String::from_utf8(out).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    } else {
        Err(io::Error::other(format!("{status}: {}", String::from_utf8_lossy(&err).trim())))
    }
}

fn exited(pid: u32) -> io::Result<bool> {
    // SAFETY: a zeroed siginfo_t is valid; waitid writes it for this child.
    // WNOWAIT observes the status without releasing ownership of its PID.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let status = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info,
        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
    if status < 0 { Err(io::Error::last_os_error()) } else { Ok(info.si_signo == libc::SIGCHLD) }
}

fn drain(reader: &mut impl Read, bytes: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) if n <= limit.saturating_sub(bytes.len()) => bytes.extend_from_slice(&buffer[..n]),
            Ok(_) => return Err(io::Error::new(io::ErrorKind::FileTooLarge, "diagnostic output exceeds 1 MiB")),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn json(text: io::Result<String>) -> io::Result<Value> {
    serde_json::from_str(&text?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn inspect(mut query: impl FnMut(&str, &[&str]) -> io::Result<String>) -> (String, bool) {
    let addresses = json(query("ip", &["-j", "-details", "-4", "address", "show"]));
    let routes = json(query("ip", &["-j", "-4", "route", "show", "default"]));
    let nft = json(query("nft", &["-j", "list", "ruleset"]));
    let version = query("iptables", &["--version"]);
    let filter = query("iptables", &["-w", "1", "-S"]);
    let addresses_readable = addresses.as_ref().is_ok_and(Value::is_array);
    let mut text = String::from("Linux bridge forwarding inspection (read-only)\n");
    let mut complete = true;
    for (name, error) in [
        ("bridge addresses (iproute2)", addresses.as_ref().err()),
        ("default routes (iproute2)", routes.as_ref().err()),
        ("nftables rules (root or CAP_NET_ADMIN)", nft.as_ref().err()),
        ("iptables backend", version.as_ref().err()),
        ("iptables filter rules (root or CAP_NET_ADMIN)", filter.as_ref().err()),
    ] {
        if let Some(error) = error {
            complete = false;
            let _ = writeln!(text, "UNKNOWN {name}: {}", error.to_string().escape_default());
        }
    }
    if let Ok(version) = version {
        let _ = writeln!(text, "iptables frontend: {}", version.trim().escape_default());
    }
    let mut bridges = Vec::new();
    if let Ok(addresses) = addresses {
        match addresses.as_array() {
            Some(items) => for item in items {
                let Some(name) = item["ifname"].as_str() else { continue };
                if !asagiri_bridge(name) || item["linkinfo"]["info_kind"] != "bridge" { continue; }
                let mut subnets = Vec::new();
                if let Some(addresses) = item["addr_info"].as_array() {
                    for address in addresses {
                        if address["family"] != "inet" { continue; }
                        let Some(ip) = address["local"].as_str().and_then(|s| s.parse::<Ipv4Addr>().ok()) else { continue };
                        let Some(prefix) = address["prefixlen"].as_u64().filter(|p| *p <= 32) else { continue };
                        let mask = u32::MAX.checked_shl(32 - prefix as u32).unwrap_or(0);
                        subnets.push(format!("{}/{prefix} (gateway {ip})", Ipv4Addr::from(u32::from(ip) & mask)));
                    }
                }
                let _ = writeln!(text, "Bridge {name}: {}", if subnets.is_empty() { "no IPv4 address observed".into() } else { subnets.join(", ") });
                bridges.push(name.to_owned());
            },
            None => { complete = false; text.push_str("UNKNOWN bridge addresses: expected an iproute2 array\n"); }
        }
    }
    if bridges.is_empty() && addresses_readable {
        text.push_str("No active Asagiri-named bridge was identified. Start a bridged container and inspect again.\n");
    }
    let mut uplinks = Vec::new();
    if let Ok(routes) = routes {
        match routes.as_array() {
            Some(items) => for route in items {
                if let Some(dev) = route["dev"].as_str().filter(|name| interface(name)) {
                    if !uplinks.iter().any(|old| old == dev) { uplinks.push(dev.to_owned()); }
                }
            },
            None => { complete = false; text.push_str("UNKNOWN default routes: expected an iproute2 array\n"); }
        }
    }
    let _ = writeln!(text, "Main-table default uplinks: {}", if uplinks.is_empty() { "none identified".into() } else { uplinks.join(", ") });
    if let Ok(nft) = nft {
        if let Some(items) = nft["nftables"].as_array() {
            let table = items.iter().any(|item| item["table"]["family"] == "ip" && item["table"]["name"] == "asagiri");
            let masquerade = items.iter().any(|item| item["rule"]["family"] == "ip"
                && item["rule"]["table"] == "asagiri" && has_key(&item["rule"]["expr"], "masquerade"));
            let _ = writeln!(text, "Asagiri IPv4 nft table: {}; masquerade expression: {}",
                if table { "present" } else { "absent" }, if masquerade { "present" } else { "absent" });
            if !table || !masquerade {
                text.push_str("Inspect the Asagiri daemon startup log before adding NAT; missing engine rules need their own diagnosis.\n");
            }
            for item in items {
                let chain = &item["chain"];
                if chain["hook"] == "forward" && chain["policy"] == "drop" {
                    let _ = writeln!(text, "DROP forward policy: family={} table={} chain={} priority={}",
                        chain["family"], chain["table"], chain["name"], chain["prio"]);
                }
            }
        } else {
            complete = false;
            text.push_str("UNKNOWN nftables rules: expected a ruleset array\n");
        }
    }
    if let Ok(filter) = filter {
        let lines: Vec<_> = filter.lines().map(str::trim).collect();
        if let Some(policy) = lines.iter().find(|line| line.starts_with("-P FORWARD ")) {
            let _ = writeln!(text, "iptables forwarding policy: {}", policy.escape_default());
        } else {
            complete = false;
            text.push_str("UNKNOWN iptables forwarding policy: no FORWARD policy was reported\n");
        }
        let ufw = lines.contains(&"-N ufw-before-forward") && lines.contains(&"-A FORWARD -j ufw-before-forward");
        let docker = lines.contains(&"-N DOCKER-USER") && lines.contains(&"-A FORWARD -j DOCKER-USER");
        let _ = writeln!(text, "Unconditional FORWARD jump: UFW before-forward={ufw}, DOCKER-USER={docker}");
        if lines.contains(&"-P FORWARD DROP") && !bridges.is_empty() {
            text.push_str("DROP requires matching allowances, which may already exist. This inspection does not simulate rule traversal or prove a packet is blocked.\n");
            if let [uplink] = uplinks.as_slice() {
                text.push_str("Review the intended egress and existing rule order before applying these examples:\n");
                for bridge in &bridges {
                    if ufw {
                        let _ = writeln!(text, "  sudo ufw route allow in on {bridge} out on {uplink}");
                    } else {
                        let chain = if docker { "DOCKER-USER" } else { "FORWARD" };
                        let _ = writeln!(text, "  sudo iptables -I {chain} 1 -i {bridge} -o {uplink} -j ACCEPT");
                        let _ = writeln!(text, "  sudo iptables -I {chain} 1 -i {uplink} -o {bridge} -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT");
                    }
                }
                if ufw { text.push_str("UFW normally allows established replies; review customized before-forward rules too.\n"); }
            } else {
                text.push_str("No single default uplink: choose the intended route before writing a bridge allowance.\n");
            }
        }
    }
    text.push_str("An accept in Asagiri's nft table cannot override a later host firewall drop. Review every forward-hook chain, including native nftables, explicit DROP/REJECT rules, and any separate iptables-legacy rules.\n");
    text.push_str("This IPv4 snapshot does not evaluate IPv6, policy routing, VPNs, rule counters or packet reachability. Reinspect after network recreation or firewall reload; persist allowances through the owning firewall manager.\n");
    text.push_str("Keep the global policy, unrelated bridges and unsolicited inbound traffic restricted. Guide: docs/linux-firewall.md\n");
    (text, complete)
}

fn asagiri_bridge(name: &str) -> bool {
    name.strip_prefix("asagiri").is_some_and(|suffix| !suffix.is_empty()
        && suffix.bytes().all(|b| b.is_ascii_digit())) && name.len() < 16
}

fn interface(name: &str) -> bool {
    !name.is_empty() && name.len() < 16 && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn has_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(object) => object.contains_key(key) || object.values().any(|value| has_key(value, key)),
        Value::Array(array) => array.iter().any(|value| has_key(value, key)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(filter: &str, routes: Value) -> (String, bool) {
        let addresses = json!([
            {"ifname":"asagiri0","linkinfo":{"info_kind":"bridge"},"addr_info":[{"family":"inet","local":"172.20.0.1","prefixlen":16}]},
            {"ifname":"asagiri2","linkinfo":{"info_kind":"bridge"},"addr_info":[{"family":"inet","local":"172.23.4.1","prefixlen":24}]},
            {"ifname":"docker0","linkinfo":{"info_kind":"bridge"},"addr_info":[{"family":"inet","local":"172.17.0.1","prefixlen":16}]},
            {"ifname":"asagiri9","linkinfo":{"info_kind":"veth"},"addr_info":[]}
        ]);
        let nft = json!({"nftables":[
            {"table":{"family":"ip","name":"asagiri"}},
            {"rule":{"family":"ip","table":"asagiri","expr":[{"masquerade":null}]}},
            {"chain":{"family":"ip","table":"filter","name":"FORWARD","hook":"forward","prio":0,"policy":"drop"}},
            {"chain":{"family":"inet","table":"host","name":"route","hook":"forward","prio":10,"policy":"drop"}}
        ]});
        inspect(|program, args| Ok(match (program, args) {
            ("ip", ["-j", "-details", "-4", "address", "show"]) => addresses.to_string(),
            ("ip", ["-j", "-4", "route", "show", "default"]) => routes.to_string(),
            ("nft", ["-j", "list", "ruleset"]) => nft.to_string(),
            ("iptables", ["--version"]) => "iptables v1.8.11 (nf_tables)".into(),
            ("iptables", ["-w", "1", "-S"]) => filter.into(),
            other => panic!("unexpected command: {other:?}"),
        }))
    }

    #[test]
    fn custom_bridges_get_exact_subnets_and_docker_user_examples() {
        let (text, complete) = fixture("-P FORWARD DROP\n-N DOCKER-USER\n-A FORWARD -j DOCKER-USER\n-A DOCKER-USER -j RETURN", json!([{"dev":"eth0"}]));
        assert!(complete);
        assert!(text.contains("Bridge asagiri2: 172.23.4.0/24 (gateway 172.23.4.1)"));
        assert!(text.contains("-I DOCKER-USER 1 -i asagiri0 -o eth0 -j ACCEPT"));
        assert!(text.contains("-I DOCKER-USER 1 -i eth0 -o asagiri2 -m conntrack --ctstate ESTABLISHED,RELATED"));
        assert!(!text.contains("-i docker0"));
        assert!(!text.contains("Bridge asagiri9"));
        assert!(text.contains("table=\"host\" chain=\"route\""));
        assert!(text.contains("masquerade expression: present"));
    }

    #[test]
    fn drop_with_an_accept_is_not_reported_as_proven_blocking() {
        let (text, _) = fixture("-P FORWARD DROP\n-A FORWARD -i asagiri0 -j ACCEPT", json!([{"dev":"eth0"}]));
        assert!(text.contains("allowances, which may already exist"));
        assert!(text.contains("does not simulate rule traversal or prove a packet is blocked"));
    }

    #[test]
    fn an_unreachable_docker_user_chain_does_not_get_examples() {
        let (text, _) = fixture("-P FORWARD DROP\n-N DOCKER-USER\n-A FORWARD -i docker0 -j DOCKER-USER", json!([{"dev":"eth0"}]));
        assert!(text.contains("-I FORWARD 1 -i asagiri2"));
        assert!(!text.contains("-I DOCKER-USER"));
    }

    #[test]
    fn ufw_manager_rules_take_precedence_over_direct_iptables_examples() {
        let (text, _) = fixture("-P FORWARD DROP\n-N ufw-before-forward\n-A FORWARD -j ufw-before-forward\n-N DOCKER-USER\n-A FORWARD -j DOCKER-USER", json!([{"dev":"ens5"}]));
        assert!(text.contains("sudo ufw route allow in on asagiri2 out on ens5"));
        assert!(!text.contains("sudo iptables -I"));
    }

    #[test]
    fn ambiguous_or_unsafe_routes_do_not_generate_commands() {
        for routes in [json!([]), json!([{"dev":"eth0"},{"dev":"wg0"}]), json!([{"dev":"eth0;id"}]), json!([{"dev":"-bad"}])] {
            let (text, _) = fixture("-P FORWARD DROP", routes);
            assert!(!text.contains("sudo iptables -I"));
            assert!(text.contains("No single default uplink"));
        }
        assert!(!asagiri_bridge("asagiri+"));
        assert!(!asagiri_bridge("asagiri2;id"));
    }

    #[test]
    fn accept_policy_does_not_suggest_opening_more_forwarding() {
        let (text, _) = fixture("-P FORWARD ACCEPT", json!([{"dev":"eth0"}]));
        assert!(!text.contains("sudo iptables -I"));
        assert!(text.contains("explicit DROP/REJECT rules"));
    }

    #[test]
    fn unavailable_and_malformed_inputs_are_unknown_not_clean() {
        let (text, complete) = inspect(|_, _| Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied")));
        assert!(!complete);
        assert_eq!(text.matches("UNKNOWN").count(), 5);
        assert!(!text.contains("masquerade expression: absent"));
        assert!(!text.contains("Start a bridged container"));
        let (text, complete) = inspect(|_, _| Ok("{}".into()));
        assert!(!complete);
        assert!(text.contains("UNKNOWN nftables"));
    }

    #[test]
    fn capture_drains_both_pipes_and_reports_failure() {
        if std::env::var_os("ASAGIRI_TEST_IGNORED_SIGCHLD").is_some() {
            let error = capture(&mut Command::new("/asagiri-diagnostic-must-not-spawn"), TIMEOUT, 100).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(error.to_string().contains("SIGCHLD"));
            return;
        }
        let text = capture(Command::new("sh").args(["-c", "printf output; printf error >&2"]), TIMEOUT, 100).unwrap();
        assert_eq!(text, "output");
        let error = capture(Command::new("sh").args(["-c", "printf denied >&2; exit 4"]), TIMEOUT, 100).unwrap_err();
        assert!(error.to_string().contains("denied"));
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let deadline = Instant::now() + TIMEOUT;
        while !exited(child.id()).unwrap() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // Observing completion twice must not reap the PID between cleanup
        // and wait. A reaping wait would return ECHILD on the second call.
        let first = exited(child.id()).unwrap();
        let second = exited(child.id()).unwrap();
        assert!(child.wait().unwrap().success());
        assert!(first && second);
    }

    #[test]
    fn capture_limits_output_and_kills_timed_out_helpers() {
        let error = capture(Command::new("sh").args(["-c", "printf 123456"]), TIMEOUT, 5).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir().join(format!("asagiri-firewall-helper-{}-{}",
            std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let mut killed = Vec::new();
        for (index, ending) in ["wait", "exit 0"].iter().enumerate() {
            let path = dir.join(index.to_string());
            let record = if cfg!(target_os = "linux") {
                "cat /proc/\"$helper\"/stat"
            } else {
                "ps -p \"$helper\" -o pid=,lstart=,stat="
            };
            // A one-second helper also lets a deliberately broken leader-only
            // cleanup control finish naturally, without signaling a reused PID.
            let script = format!("sleep 1 & helper=$!; {record} > \"$1\"; {ending}");
            let mut command = Command::new("sh");
            command.args(["-c", &script, "sh"]).arg(&path);
            let start = Instant::now();
            let error = capture(&mut command, Duration::from_millis(200), 100).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(start.elapsed() < Duration::from_secs(2));
            let (pid, identity, _) = helper_stat(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let alive = || helper(pid).is_some_and(|(_, current, live)| current == identity && live);
            killed.push(!alive());
            // Cleanup is checked before waiting. A leaked test helper exits
            // by itself; the negative control must not leak it after failing.
            let deadline = Instant::now() + Duration::from_secs(2);
            while alive() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(10)); }
            assert!(!alive(), "test helper did not finish");
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(killed, [true, true], "timeout cleanup left a live descendant");
    }

    fn helper_stat(text: &str) -> Option<(u32, String, bool)> {
        let pid = text.split_whitespace().next()?.parse().ok()?;
        if cfg!(target_os = "linux") {
            let fields: Vec<_> = text.rsplit_once(')')?.1.split_whitespace().collect();
            Some((pid, fields.get(19)?.to_string(), *fields.first()? != "Z" && *fields.first()? != "X"))
        } else {
            let fields: Vec<_> = text.split_whitespace().collect();
            let state = fields.last()?;
            Some((pid, fields.get(1..fields.len().checked_sub(1)?)?.join(" "), !state.starts_with('Z') && !state.starts_with('X')))
        }
    }

    fn helper(pid: u32) -> Option<(u32, String, bool)> {
        if cfg!(target_os = "linux") {
            helper_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
        } else {
            let output = Command::new("ps").args(["-p", &pid.to_string(), "-o", "pid=,lstart=,stat="]).output().ok()?;
            helper_stat(std::str::from_utf8(&output.stdout).ok()?)
        }
    }
}
