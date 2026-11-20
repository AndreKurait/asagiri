//! Containers, execs and images as the daemon tracks them.
//!
//! Container records are saved to `containers.json` on every change (except
//! `--rm` containers, which are gone by the time anyone could ask), so `docker
//! ps -a` survives a daemon restart. Every state change notifies one
//! condition variable, which is what `wait` blocks on.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use asagiri_image::{Image, Store};
use asagiri_proto::{Stream, run_flags};

use super::network::Networks;
use super::vmlink::{Event, VmLink};

/// What a request that needs the VM gets while Asagiri is stopped.
pub const STOPPED: &str = "Asagiri is stopped: press Start in the Asagiri window to run containers again";

/// The file that keeps "stopped" across engine restarts.
fn stopped_marker() -> Option<std::path::PathBuf> {
    super::state_dir().ok().map(|d| d.join("stopped"))
}

/// The VM's memory ceiling: `ASAGIRI_MEMORY_MIB`, else Settings → Resources,
/// else half the Mac's RAM up to 8 GiB.
pub fn memory_ceiling() -> u64 {
    if let Some(mib) = std::env::var("ASAGIRI_MEMORY_MIB").ok().and_then(|v| v.parse::<u64>().ok()) {
        return mib << 20;
    }
    if let Some(mib) = resources()["memory_mib"].as_u64() {
        return (mib << 20).clamp(1 << 30, physical_memory());
    }
    (physical_memory() / 2).clamp(2 << 30, 8 << 30)
}

/// The VM's CPUs: `ASAGIRI_CPUS`, else Settings → Resources, else 2.
pub fn vm_cpus() -> usize {
    let max = max_cpus();
    std::env::var("ASAGIRI_CPUS").ok().and_then(|v| v.parse().ok()).or_else(|| resources()["cpus"].as_u64().map(|c| c as usize)).unwrap_or(2).clamp(1, max)
}

/// The Mac's CPUs, the most the VM can have.
pub fn max_cpus() -> usize {
    std::thread::available_parallelism().map_or(2, std::num::NonZero::get)
}

/// What Settings → Resources saved, read at each boot: `{"cpus", "memory_mib"}`.
pub fn resources_path() -> Option<std::path::PathBuf> {
    super::state_dir().ok().map(|d| d.join("vm.json"))
}

pub fn resources() -> serde_json::Value {
    resources_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(serde_json::Value::Null)
}

pub fn physical_memory_bytes() -> u64 {
    physical_memory()
}

#[cfg(target_os = "macos")]
fn physical_memory() -> u64 {
    let mut ram: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: sysctlbyname writes a u64 into `ram`.
    unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&mut ram as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) };
    ram
}

/// This process's resident memory, in bytes: on a Linux host the daemon runs
/// beside its containers, so it counts in what Asagiri holds.
#[cfg(not(target_os = "macos"))]
fn self_rss() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
    // SAFETY: sysconf takes no pointers.
    pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as u64
}

#[cfg(target_os = "macos")]
fn self_rss() -> u64 {
    0
}

#[cfg(not(target_os = "macos"))]
fn physical_memory() -> u64 {
    let info = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    info.lines().find_map(|l| l.strip_prefix("MemTotal:")).and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok()).map_or(4 << 30, |k| k << 10)
}

/// Docker's restart backoff: 100 ms doubling, capped at one minute.
const RESTART_BASE: Duration = Duration::from_millis(100);
const RESTART_CAP: Duration = Duration::from_secs(60);

fn restart_backoff(attempt: u32) -> Duration {
    RESTART_BASE.saturating_mul(1 << attempt.min(10)).min(RESTART_CAP)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Start {
    User,
    Restart,
    DaemonRestart,
    VmRecovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Created,
    Running,
    Exited,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Created => "created",
            Status::Running => "running",
            Status::Exited => "exited",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartPolicy {
    /// `no`, `always`, `unless-stopped` or `on-failure`.
    pub name: String,
    pub max_retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    /// Owned by the engine, not a user workload. Only internal creation can
    /// set this; Docker request labels and fields never grant this status.
    #[serde(default)]
    pub engine_service: bool,
    pub image: String,
    pub image_id: String,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub user: String,
    pub auto_remove: bool,
    pub network_mode: String,
    pub labels: serde_json::Value,
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub open_stdin: bool,
    /// Before this was kept, stdin was closed whenever an attached client's
    /// input ended: a record without it loads as set, and still does that.
    #[serde(default = "enabled")]
    pub stdin_once: bool,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Initial terminal size (rows, cols), from `HostConfig.ConsoleSize`.
    #[serde(default)]
    pub console_size: Option<(u16, u16)>,
    /// Extra names on its network (`--network-alias`, Compose service names).
    #[serde(default)]
    pub aliases: Vec<String>,
    /// `--add-host` lines for /etc/hosts, already as `ip name`.
    #[serde(default)]
    pub extra_hosts: Vec<String>,
    /// `-p` bindings.
    #[serde(default)]
    pub ports: Vec<super::ports::Binding>,
    /// TCP ports the image (or `--expose`) declares; picks the domain's port.
    #[serde(default)]
    pub exposed: Vec<u16>,
    /// All declared port/protocol pairs, retained for Docker inspection.
    /// Older records keep their TCP declarations in `exposed`.
    #[serde(default)]
    pub exposed_port_keys: Vec<String>,
    /// Bind mounts as the guest takes them: `source\ttarget\tflags`, where
    /// `source` is a path inside the VM.
    #[serde(default)]
    pub mounts: Vec<String>,
    /// For `inspect`: Docker's `Mounts` entries.
    #[serde(default)]
    pub mount_info: Vec<serde_json::Value>,
    /// Named volumes it uses (kept while it exists), and the anonymous ones
    /// created for it (deleted with it on `--rm` or `rm -v`).
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default)]
    pub anon_volumes: Vec<String>,
    /// `--privileged`.
    #[serde(default)]
    pub privileged: bool,
    /// `--memory`, `--cpus`, `--cpu-shares`, `--pids-limit`; `docker update` changes them.
    #[serde(default)]
    pub limits: Limits,
    /// `--cap-add` / `--cap-drop` applied to Docker's default set, one bit
    /// per capability; 0 means the default set.
    #[serde(default)]
    pub caps: u64,
    /// `--cap-add`/`--cap-drop` as given, for inspect.
    #[serde(default)]
    pub cap_add: Vec<String>,
    #[serde(default)]
    pub cap_drop: Vec<String>,
    /// `--security-opt` entries as given.
    #[serde(default)]
    pub security_opt: Vec<String>,
    /// `--read-only`.
    #[serde(default)]
    pub readonly_rootfs: bool,
    /// `--hostname`; empty means the short container id, as Docker does.
    #[serde(default)]
    pub hostname: String,
    /// `--stop-signal` (image STOPSIGNAL); SIGTERM if not set.
    #[serde(default)]
    pub stop_signal: Option<u32>,
    /// `--stop-timeout`, seconds (-1: wait for ever); the stop request's own wins.
    #[serde(default)]
    pub stop_timeout: Option<i64>,
    /// `Config.Healthcheck` merged over the image's, as inspect shows it.
    #[serde(default)]
    pub healthcheck: Option<super::health::Healthcheck>,
    /// `Config.Shell` (or the image's SHELL) for `CMD-SHELL` healthchecks.
    #[serde(default)]
    pub shell: Vec<String>,
    /// `--init` (`HostConfig.Init`): a small init as PID 1 that forwards
    /// signals and reaps zombies.
    #[serde(default)]
    pub init: Option<bool>,
    /// Networks beyond `network_mode` (`docker network connect`, Compose):
    /// name and aliases on it.
    #[serde(default)]
    pub extra_networks: Vec<ExtraNetwork>,
    /// `--log-driver` / `--log-opt`; containers from before it have the default.
    #[serde(default)]
    pub log_config: super::logfile::LogConfig,
    /// `--ip` / Compose `ipv4_address` on `network_mode`; empty for any.
    #[serde(default)]
    pub ipv4_address: String,
    /// `--ip6` / Compose `ipv6_address` on `network_mode`; empty for any.
    #[serde(default)]
    pub ipv6_address: String,
    /// `--mac-address` on `network_mode`; empty for the kernel's choice.
    #[serde(default)]
    pub mac_address: String,
    /// `--dns`, `--dns-search`, `--dns-option`.
    #[serde(default)]
    pub dns: Vec<String>,
    #[serde(default)]
    pub dns_search: Vec<String>,
    #[serde(default)]
    pub dns_options: Vec<String>,
    /// `--domainname`.
    #[serde(default)]
    pub domainname: String,
    /// `--link` / Compose `links`: /etc/hosts entries for these containers.
    #[serde(default)]
    pub links: Vec<Link>,
    /// HostConfig's Ulimits, Sysctls, Devices, DeviceCgroupRules and
    /// GroupAdd as given; see [`super::hostopts`].
    #[serde(default)]
    pub host_opts: serde_json::Value,
}

/// One `--link name:alias`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// The linked container's id, and its name when the link was made.
    pub id: String,
    pub name: String,
    pub alias: String,
}

/// cgroup limits as saved with a container; see [`asagiri_proto::Limits`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub memory_max: u64,
    pub cpu_quota: u64,
    pub cpu_period: u32,
    pub cpu_weight: u32,
    pub pids_max: u32,
    #[serde(default)]
    pub shm_size: u64,
    #[serde(default)]
    pub memory_swap: i64,
    #[serde(default)]
    pub memory_reservation: u64,
    #[serde(default)]
    pub cpuset_cpus: String,
    #[serde(default)]
    pub cpuset_mems: String,
    /// `NanoCpus` and `CpuShares` as given, so inspect returns what create
    /// was sent. Containers saved before these existed have the quota,
    /// period and weight they stood for instead.
    #[serde(default)]
    pub nano_cpus: u64,
    #[serde(default)]
    pub cpu_shares: u64,
}

impl Limits {
    pub fn validate(&self) -> Result<(), Error> {
        if self.memory_swap > 0 && (self.memory_max == 0 || (self.memory_swap as u64) < self.memory_max) {
            return Err(Error::BadRequest("MemorySwap requires Memory and must be at least Memory".into()));
        }
        Ok(())
    }

    pub fn wire(&self) -> asagiri_proto::Limits<'_> {
        // `--cpus 1.5` is 150 ms of CPU time every 100 ms; an explicit quota
        // replaces it, as in the runtime spec dockerd builds.
        let cpu_quota = if self.cpu_quota > 0 { self.cpu_quota } else { (u128::from(self.nano_cpus) * 100_000 / 1_000_000_000) as u64 };
        asagiri_proto::Limits {
            memory_max: self.memory_max,
            cpu_quota,
            cpu_period: self.cpu_period,
            cpu_weight: self.cpu_weight,
            pids_max: self.pids_max,
            shm_size: self.shm_size,
            memory_swap: self.memory_swap,
            memory_reservation: self.memory_reservation,
            cpuset_cpus: &self.cpuset_cpus,
            cpuset_mems: &self.cpuset_mems,
        }
    }
}

fn enabled() -> bool {
    true
}

impl Spec {
    /// The guest's `opts` lines for a start or an exec. `net_host`: it runs
    /// in the VM's (or host's) network namespace.
    pub fn guest_opts(&self, net_host: bool) -> Result<Vec<String>, Error> {
        let mut lines = super::hostopts::guest_lines(&self.host_opts, net_host).map_err(Error::BadRequest)?;
        lines.extend(seccomp_lines(&self.security_opt)?);
        Ok(lines)
    }

    /// Whether `--sysctl net.ipv6.conf.{all,default,<ifname>}.disable_ipv6=1`
    /// turns IPv6 off on `ifname`: then it gets no IPv6 address (and no AAAA
    /// record), as with Docker.
    pub fn ipv6_disabled(&self, ifname: &str) -> bool {
        super::hostopts::ipv6_disabled(&self.host_opts, ifname)
    }

    /// The security flags for the guest: seccomp, no-new-privileges, read-only root.
    pub fn security_flags(&self) -> u32 {
        let mut f = 0;
        if seccomp_option(&self.security_opt) == Some("unconfined") {
            f |= run_flags::NO_SECCOMP;
        }
        if self.security_opt.iter().any(|o| o == "no-new-privileges" || o == "no-new-privileges:true" || o == "no-new-privileges=true") {
            f |= run_flags::NO_NEW_PRIVS;
        }
        if self.readonly_rootfs {
            f |= run_flags::READONLY_ROOTFS;
        }
        f
    }
}

/// The names a container answers to on a user-defined network, as Docker
/// lists them in an endpoint's `DNSNames`: its name, the endpoint's aliases,
/// its short id and its hostname, each once.
pub fn endpoint_names(name: &str, aliases: &[String], id: &str, hostname: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in [name].into_iter().chain(aliases.iter().map(String::as_str)).chain([id.get(..12).unwrap_or(id), hostname]) {
        if !n.is_empty() && !out.iter().any(|o| o == n) {
            out.push(n.to_owned());
        }
    }
    out
}

/// A container on a network: (id, name, ip, IPv6 ip, the MAC address it
/// asked for, whether it is running).
pub type Member = (String, String, Option<[u8; 4]>, Option<[u8; 16]>, String, bool);

/// The seccomp security option in effect: the last one given, as with
/// Docker. `None` is the default profile (no option, or `builtin`).
fn seccomp_option(options: &[String]) -> Option<&str> {
    let value = options.iter().filter_map(|o| o.strip_prefix("seccomp=").or_else(|| o.strip_prefix("seccomp:"))).next_back();
    value.filter(|v| !v.is_empty() && *v != "builtin")
}

/// A custom profile as one guest-option line, its JSON re-encoded so that it
/// holds no newline. Checked when the container starts, like Docker: a
/// profile that does not parse fails the start rather than run unconfined.
/// The transport bounds the complete request, including its other fields.
pub(super) fn seccomp_lines(options: &[String]) -> Result<Vec<String>, Error> {
    let Some(profile) = seccomp_option(options).filter(|v| *v != "unconfined") else { return Ok(Vec::new()) };
    let invalid = |why: String| Error::Io(io::Error::other(format!("Decoding seccomp profile failed: {why}")));
    let mut value: serde_json::Value = serde_json::from_str(profile).map_err(|e| invalid(e.to_string()))?;
    if value.is_null() {
        // To Docker `null` is the empty profile.
        value = serde_json::json!({});
    }
    if !value.is_object() {
        return Err(invalid("the profile must be a JSON object".into()));
    }
    Ok(vec![format!("seccomp\t{value}")])
}

#[cfg(test)]
mod seccomp_options_tests {
    use super::*;

    fn lines(options: &[&str]) -> Result<Vec<String>, Error> {
        seccomp_lines(&options.iter().map(|o| o.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_last_seccomp_option_wins() {
        assert!(lines(&["seccomp=[]"]).is_err());
        assert!(lines(&["seccomp=invalid"]).is_err());
        assert!(lines(&["seccomp=invalid", "seccomp=unconfined"]).unwrap().is_empty());
        assert!(lines(&["seccomp=unconfined", "seccomp=invalid"]).is_err());
        assert!(lines(&["seccomp={}", "seccomp=builtin"]).unwrap().is_empty());
        assert!(lines(&["no-new-privileges", "label=disable"]).unwrap().is_empty());
        assert_eq!(lines(&["seccomp: {\n\"defaultAction\": \"SCMP_ACT_ALLOW\"\n}"]).unwrap(), [r#"seccomp	{"defaultAction":"SCMP_ACT_ALLOW"}"#]);
    }

    #[test]
    fn an_invalid_profile_fails_the_start_and_names_seccomp() {
        let Err(Error::Io(e)) = lines(&["seccomp={\"defaultAction\":"]) else { panic!("accepted") };
        assert!(e.to_string().starts_with("Decoding seccomp profile failed: "), "{e}");
    }

    #[test]
    fn a_profile_replaces_the_default_even_when_privileged() {
        let profile = r#"seccomp={"defaultAction":"SCMP_ACT_ERRNO"}"#.to_string();
        let mut spec: Spec = serde_json::from_value(serde_json::json!({
            "image": "", "image_id": "", "entrypoint": [], "cmd": [], "env": [], "cwd": "", "user": "",
            "auto_remove": false, "network_mode": "", "labels": {}, "security_opt": [&profile], "privileged": true,
        })).unwrap();
        let sent = |spec: &Spec| spec.guest_opts(false).unwrap().into_iter().filter(|l| l.starts_with("seccomp\t")).collect::<Vec<_>>();
        assert_eq!(spec.security_flags() & run_flags::NO_SECCOMP, 0);
        assert_eq!(sent(&spec), [r#"seccomp	{"defaultAction":"SCMP_ACT_ERRNO"}"#]);
        spec.security_opt = vec!["seccomp=unconfined".into(), profile.clone()];
        assert_eq!(spec.security_flags() & run_flags::NO_SECCOMP, 0);
        spec.security_opt = vec![profile, "seccomp=unconfined".into()];
        assert_ne!(spec.security_flags() & run_flags::NO_SECCOMP, 0);
        assert!(sent(&spec).is_empty());
        spec.security_opt = vec!["seccomp=null".into()];
        assert_eq!(sent(&spec), ["seccomp\t{}"]);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraNetwork {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// A fixed address on it (`IPAMConfig.IPv4Address`); empty for any.
    #[serde(default)]
    pub ipv4_address: String,
    /// A fixed IPv6 address on it (`IPAMConfig.IPv6Address`); empty for any.
    #[serde(default)]
    pub ipv6_address: String,
    #[serde(default)]
    pub mac_address: String,
}

#[derive(Debug)]
pub struct Container {
    /// Serializes start with stop/kill/remove through guest creation.
    lifecycle: Arc<Mutex<u64>>,
    pub id: String,
    pub name: String,
    pub spec: Spec,
    pub layers: Vec<String>,
    pub created: SystemTime,
    pub status: Status,
    pub started: Option<SystemTime>,
    pub finished: Option<SystemTime>,
    pub exit_code: i32,
    /// The last start failure, cleared once a later start succeeds.
    pub start_error: String,
    /// Its last run ended with the kernel killing it for memory.
    pub oom_killed: bool,
    /// Its last run ended because the VM or the engine stopped under it, not
    /// because the app exited: Docker's exit code 255, but nothing failed.
    pub vm_stopped: bool,
    /// Set by `stop`/`kill`: no automatic restart until started again.
    pub stopped_by_user: bool,
    pub restart_count: u32,
    /// Exited, with its restart policy about to start it again.
    restart_pending: bool,
    /// How many times it has exited; `wait?condition=next-exit` waits for this to grow.
    exits: u64,
    /// Terminal size asked for before the process existed.
    size: Option<(u16, u16)>,
    /// Its address while running on a bridge network.
    pub ip: Option<[u8; 4]>,
    /// Its first process while running, as the agent's kernel numbers it
    /// (the host's on a Linux host, the VM's otherwise); 0 when not running.
    pub pid: u32,
    /// Published ports while running.
    pub forwards: Option<super::ports::Forwards>,
    /// Frozen by `docker pause`.
    pub paused: bool,
    /// Addresses on `spec.extra_networks` while running, as (network, ip).
    pub extra_ips: Vec<(String, [u8; 4])>,
    /// The interface each of those networks is plugged in as while running,
    /// as (network, n) for `eth{n}`. A network that leaves frees its number;
    /// the ones that stay keep theirs.
    extra_ifs: Vec<(String, u32)>,
    /// IPv6 addresses while running: on `network_mode`, and on extra
    /// networks as (network, ip). Only networks with IPv6 give one.
    pub ip6: Option<[u8; 16]>,
    pub extra_ip6s: Vec<(String, [u8; 16])>,
    /// True while the guest has the process: from then on a signal has
    /// something to reach.
    in_guest: bool,
    /// True once the guest has the process and the input held here until
    /// then: `docker run -i` attaches and sends stdin before it starts the
    /// container.
    live: bool,
    early_input: Vec<u8>,
    early_close: bool,
    /// `docker logs`: its output on disk, and who follows it.
    log: Arc<Mutex<super::logfile::Log>>,
    attached: Vec<Sender<(Stream, Vec<u8>)>>,
}

impl Container {
    pub fn argv(&self) -> Vec<String> {
        self.spec.entrypoint.iter().chain(&self.spec.cmd).cloned().collect()
    }

    pub fn short_id(&self) -> &str {
        &self.id[..12]
    }
}

#[derive(Debug, Clone)]
pub struct ExecConfig {
    /// Initial terminal size (rows, cols).
    pub console_size: Option<(u16, u16)>,
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub user: String,
    pub tty: bool,
    pub stdin: bool,
    /// `exec --privileged`, or any exec in a privileged container.
    pub privileged: bool,
    /// Asagiri's own (the Kubernetes service watcher): no events.
    pub internal: bool,
}

#[derive(Debug)]
pub struct Exec {
    pub id: String,
    pub container_id: String,
    pub config: ExecConfig,
    pub running: bool,
    pub exit_code: Option<i32>,
    /// Its process while running; 0 otherwise.
    pub pid: u32,
    attached: Vec<Sender<(Stream, Vec<u8>)>>,
}

/// What `docker logs` reads: the stored part, then (with `follow`) what comes after it.
pub struct Logs {
    pub tty: bool,
    pub past: super::logfile::Snapshot,
    pub live: Option<Receiver<super::logfile::Record>>,
}

fn lock_log(l: &Mutex<super::logfile::Log>) -> MutexGuard<'_, super::logfile::Log> {
    l.lock().unwrap_or_else(|e| e.into_inner())
}

/// What `containers.json` holds for one container.
#[derive(Serialize, Deserialize)]
struct Saved {
    id: String,
    name: String,
    spec: Spec,
    layers: Vec<String>,
    created_ns: u64,
    status: Status,
    started_ns: Option<u64>,
    finished_ns: Option<u64>,
    exit_code: i32,
    #[serde(default)]
    start_error: String,
    #[serde(default)]
    oom_killed: bool,
    #[serde(default)]
    vm_stopped: bool,
    stopped_by_user: bool,
    restart_count: u32,
    #[serde(default)]
    health: Option<super::health::Health>,
}

/// A container's healthcheck while it has one: its `State.Health`, and
/// whether a monitor thread is probing. `epoch` changes whenever the monitor
/// should stop, so a thread that sees a different one quits.
#[derive(Debug, Default)]
struct HealthRun {
    health: super::health::Health,
    epoch: u64,
    monitoring: bool,
}

#[derive(Default)]
struct State {
    /// Pull/create work that needs the guest before any container is running.
    image_preparations: usize,
    vm_recovery_at: Option<std::time::Instant>,
    vm_recovery_running: bool,
    vm_recovery_generation: u64,
    containers: HashMap<String, Container>,
    execs: HashMap<String, Exec>,
    /// Exit codes of recently removed containers, for `wait?condition=removed`
    /// (that is how `docker run --rm` learns the exit status). Bounded.
    removed: VecDeque<(String, i32)>,
    /// `State.Health` of containers with a healthcheck, by id.
    health: HashMap<String, HealthRun>,
}

/// Set while the daemon shuts down: exits then are not restarts.
static SHUTTING_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `docker stop`'s default wait, when neither the request nor the container sets one.
pub const DEFAULT_STOP_TIMEOUT: i64 = 10;

/// A stop timeout in seconds as a wait; negative means as long as it takes.
pub fn stop_wait(secs: i64) -> Duration {
    if secs < 0 { Duration::from_secs(365 * 86_400) } else { Duration::from_secs(secs as u64) }
}

pub struct Engine {
    /// Stable for this data directory, including across daemon restarts.
    pub id: String,
    pub volumes: Mutex<super::volumes::Volumes>,
    /// The VM, while it is awake. It sleeps (powers off, freeing all its
    /// memory) when idle, and boots again on the next request.
    link: Mutex<Option<Arc<VmLink>>>,
    /// Serialises retirement and boot without blocking access to the link.
    waking: Mutex<()>,
    #[cfg(test)]
    boot_attempts: std::sync::atomic::AtomicUsize,
    /// Unix seconds of the last API request; drives the idle sleep.
    last_activity: std::sync::atomic::AtomicU64,
    /// Set while the VM is being put to sleep, so its stop is not an error.
    sleeping: std::sync::atomic::AtomicBool,
    /// Stopped from the window: the VM stays off, whatever asks for it
    /// (a build, a docker command, a share), until Start. Unlike the idle
    /// sleep, which the next request wakes. Kept across engine restarts by
    /// a file in the state dir (`stopped`).
    stopped: std::sync::atomic::AtomicBool,
    pub store: Store,
    pub networks: Mutex<Networks>,
    state: Mutex<State>,
    /// Serialises read/merge/apply/persist for resource updates.
    resource_updates: Mutex<()>,
    changed: Condvar,
    /// Independent layers may transfer together; duplicate digests wait.
    layers: super::layer_transfers::LayerTransfers,
    records: PathBuf,
    /// Serialises writes of `containers.json`.
    saving: Mutex<()>,
    me: OnceLock<Weak<Engine>>,
    /// Containers that come back on Start: the ones whose restart policy
    /// would have started them when the daemon came up, held because Asagiri
    /// was stopped (Start starts them) or because the VM did not boot
    /// (`retry_held` tries it again), and the ones Stop (or the VM's sleep on
    /// request) stopped, which `always` and `unless-stopped` bring back.
    held_restarts: Mutex<Vec<String>>,
    /// Taken by Stop (`sleep_now`) for its whole run and by `start_held`
    /// while it starts held containers. A Stop during a Start waits for the
    /// one container being started, then stops it with the rest, and the
    /// ones not started yet stay held; a Start during a Stop starts them
    /// once the Stop is done.
    power: Mutex<()>,
    /// Set while `sleep_now` stops containers: those exits are the engine's,
    /// not ends the restart policy answers (Start brings them back).
    parking: std::sync::atomic::AtomicBool,
    /// Memory ceiling and the balloon's current target, in bytes. The
    /// ceiling is read again at each boot, so Settings applies on restart.
    ceiling: std::sync::atomic::AtomicU64,
    /// CPUs the running VM has.
    cpus: std::sync::atomic::AtomicUsize,
    memory_target: Mutex<super::balloon::Balloon>,
    /// What the guest last used, in bytes (0 while asleep): the balloon's
    /// measure, without the pages the balloon itself holds.
    memory_used: std::sync::atomic::AtomicU64,
    /// How starved of memory the guest is: PSI "full avg10" in hundredths
    /// of a percent (0 while asleep), and processes killed for memory since it booted.
    memory_stall: std::sync::atomic::AtomicU64,
    oom_kills: std::sync::atomic::AtomicU64,
    /// How busy the VM was over the last couple of seconds, in hundredths of a
    /// percent of all its CPUs (0 while asleep).
    cpu_permyriad: std::sync::atomic::AtomicU32,
    /// `docker events`: recent ones for `--since`, and who is listening.
    events: Mutex<Events>,
    /// Kubernetes LoadBalancer services published on the Mac's localhost:
    /// (port, protocol) → its forward.
    k8s_lb: Mutex<HashMap<(u16, String), super::ports::Forwards>>,
    /// The cluster's pod and service ranges routed to the node, in which VM
    /// (its link's address) and through which node address.
    k8s_routes: Mutex<Option<(usize, [u8; 4])>>,
    /// Shares of Kubernetes services and pods: the Mac port → its forward
    /// to a cluster address. They go when the cluster does.
    k8s_fwd: Mutex<HashMap<u16, super::ports::Forwards>>,
}

/// Keep idle retirement out until layer installation has finished, including
/// failed and cancelled preparations. Explicit Stop still retires the guest.
struct ImagePreparation<'a>(&'a Engine);

impl Drop for ImagePreparation<'_> {
    fn drop(&mut self) {
        self.0.lock().image_preparations -= 1;
    }
}

/// k3s's pod and service ranges, which `asagiri k8s` leaves at their defaults.
pub const K8S_RANGES: [([u8; 4], u8); 2] = [([10, 42, 0, 0], 16), ([10, 43, 0, 0], 16)];

/// Whether an address is in the cluster's pod or service range.
pub fn in_k8s_ranges(ip: [u8; 4]) -> bool {
    K8S_RANGES.iter().any(|(net, bits)| {
        let mask = u32::MAX << (32 - *bits as u32);
        u32::from_be_bytes(ip) & mask == u32::from_be_bytes(*net)
    })
}

/// Events kept for `docker events --since`.
const RECENT_EVENTS: usize = 512;
/// Events queued for a slow listener before it starts missing some.
const EVENT_QUEUE: usize = 256;

#[derive(Default)]
struct Events {
    recent: VecDeque<serde_json::Value>,
    listeners: Vec<std::sync::mpsc::SyncSender<serde_json::Value>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Engine")
    }
}

#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Conflict(String),
    BadRequest(String),
    Unauthorized(String),
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&error_text(self))
    }
}

fn ns(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

fn recover_after_vm_loss(c: &Container) -> bool {
    c.vm_stopped && c.status == Status::Exited && !c.stopped_by_user && !c.spec.auto_remove
        && matches!(c.spec.restart.name.as_str(), "always" | "unless-stopped")
}

fn from_ns(n: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(n)
}

fn check_preparation(cancelled: &dyn Fn() -> bool) -> io::Result<()> {
    if cancelled() { Err(io::Error::new(io::ErrorKind::Interrupted, "image preparation cancelled")) } else { Ok(()) }
}

impl Engine {
    /// Boots the guest, loads saved containers and restarts the ones whose
    /// restart policy says so.
    pub fn boot(store: Store) -> io::Result<Arc<Engine>> {
        let records = store.root().join("containers.json");
        let id = super::identity::load_id(store.root())?;
        let ceiling = memory_ceiling();
        let networks = Mutex::new(Networks::load(store.root())?);
        let volumes = Mutex::new(super::volumes::Volumes::load(store.root())?);
        let engine = Arc::new(Engine {
            id,
            volumes,
            link: Mutex::new(None),
            waking: Mutex::new(()),
            #[cfg(test)]
            boot_attempts: std::sync::atomic::AtomicUsize::new(0),
            last_activity: std::sync::atomic::AtomicU64::new(now_secs()),
            sleeping: std::sync::atomic::AtomicBool::new(false),
            stopped: std::sync::atomic::AtomicBool::new(stopped_marker().is_some_and(|f| f.exists())),
            networks,
            store,
            state: Mutex::default(),
            resource_updates: Mutex::new(()),
            changed: Condvar::new(),
            layers: super::layer_transfers::LayerTransfers::default(),
            records,
            saving: Mutex::new(()),
            me: OnceLock::new(),
            held_restarts: Mutex::new(Vec::new()),
            power: Mutex::new(()),
            parking: std::sync::atomic::AtomicBool::new(false),
            ceiling: std::sync::atomic::AtomicU64::new(ceiling),
            cpus: std::sync::atomic::AtomicUsize::new(vm_cpus()),
            memory_target: Mutex::new(super::balloon::Balloon::new(ceiling, std::time::Instant::now())),
            memory_used: std::sync::atomic::AtomicU64::new(0),
            memory_stall: std::sync::atomic::AtomicU64::new(0),
            oom_kills: std::sync::atomic::AtomicU64::new(0),
            cpu_permyriad: std::sync::atomic::AtomicU32::new(0),
            events: Mutex::new(Events::default()),
            k8s_lb: Mutex::new(HashMap::new()),
            k8s_routes: Mutex::new(None),
            k8s_fwd: Mutex::new(HashMap::new()),
        });
        let _ = engine.me.set(Arc::downgrade(&engine));
        engine.load()?;
        engine.start_balloon();
        engine.start_cpu_meter();
        engine.start_sleeper();
        engine.start_clock();
        engine.start_k8s_lb();
        #[cfg(target_os = "macos")]
        super::watch::start(Arc::downgrade(&engine));
        Ok(engine)
    }

    /// The running VM, booting it first if it is asleep (about 0.1 s).
    pub fn vm(&self) -> io::Result<Arc<VmLink>> {
        self.vm_inner(false)
    }

    fn vm_inner(&self, recovery: bool) -> io::Result<Arc<VmLink>> {
        let waking = self.waking.lock().unwrap_or_else(|e| e.into_inner());
        let old = self.link.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(vm) = old.as_ref().filter(|vm| !vm.is_lost()) {
            return Ok(vm.clone());
        }
        if self.is_stopped() || SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(io::Error::other(STOPPED));
        }
        if recovery {
            let mut st = self.lock();
            if !st.containers.values().any(recover_after_vm_loss)
                || st.vm_recovery_at.is_some_and(|at| at > std::time::Instant::now()) {
                // A deferred boot is still pending recovery work.
                st.vm_recovery_generation = st.vm_recovery_generation.saturating_add(1);
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "VM recovery is backing off"));
            }
        }
        if let Some(vm) = old && let Err(e) = vm.retire() {
            let mut st = self.lock();
            let delay = st.containers.values().filter(|c| recover_after_vm_loss(c))
                .map(|c| restart_backoff(c.restart_count.saturating_sub(1))).max();
            st.vm_recovery_at = delay.map(|delay| std::time::Instant::now() + delay);
            drop(st);
            self.recover_vm();
            return Err(e);
        }
        // Retained old links are inert; successful retirement has released
        // their VM and completed cleanup before the replacement opens disk.
        *self.link.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if self.is_stopped() || SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(io::Error::other(STOPPED));
        }
        if recovery && !self.lock().containers.values().any(recover_after_vm_loss) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "VM recovery was canceled"));
        }
        let (ceiling, cpus) = (memory_ceiling(), vm_cpus());
        self.ceiling.store(ceiling, std::sync::atomic::Ordering::Relaxed);
        self.cpus.store(cpus, std::sync::atomic::Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        #[cfg(test)]
        self.boot_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let guest = crate::guest::Guest::boot(&crate::guest::BootOptions {
            data_disk: true,
            console_log: crate::diag::console::path().inspect_err(|e| {
                crate::diag::warn!("guest console log has no location", error = e);
            }).ok(),
            memory_mib: Some(ceiling >> 20),
            cpus: Some(cpus),
            ..Default::default()
        })
        .inspect_err(|e| crate::diag::error!("VM did not boot", error = e))?;
        let (vm, conn) = guest.into_parts();
        let me = self.me.get().cloned().unwrap_or_default();
        let vm = Arc::new(VmLink::new(vm, conn, move |ev| {
            if let Some(engine) = me.upgrade() {
                engine.on_event(ev);
            }
        })?);
        // host.docker.internal: the guest dials back for each connection.
        vm.listen(super::ports::HOST_BOUND_PORT, Box::new(super::ports::serve_host_bound))?;
        // SSH agent forwarding, for containers that mount /run/host-services/ssh-auth.sock.
        vm.listen(super::ports::SSH_AGENT_PORT, Box::new(super::ports::serve_ssh_agent))?;
        // The engine's API, for containers that mount /var/run/docker.sock
        // (a Linux host's containers mount the daemon's socket itself).
        if cfg!(target_os = "macos") {
            vm.listen(super::ports::ENGINE_API_PORT, Box::new(super::ports::serve_engine_api))?;
        }
        // Container lookups the VM's gateway resolver fails, asked of the
        // Mac's own (VM only: a Linux host's agent asks the host's itself).
        if cfg!(target_os = "macos") {
            vm.listen(asagiri_proto::DNS_RELAY_PORT, Box::new(super::dns::serve_relay))?;
        }
        // Everything else on the guest's disk belongs to containers that are gone.
        let keep: Vec<String> = self.lock().containers.keys().cloned().collect();
        vm.retain(&keep)?;
        // And every unpacked layer no image or container uses. Skipped if
        // the image list can't be read, rather than deleting everything.
        match self.store.images() {
            Ok(images) => {
                let mut layers: std::collections::BTreeSet<String> = images.iter().flat_map(|i| i.layers.iter().map(|l| l.digest.clone())).collect();
                layers.extend(self.lock().containers.values().flat_map(|c| c.layers.iter().cloned()));
                vm.retain_layers(&layers.into_iter().collect::<Vec<_>>())?;
            }
            Err(e) => crate::diag::warn!("kept every unpacked layer: images unreadable", error = e),
        }
        *self.memory_target.lock().unwrap_or_else(|e| e.into_inner()) =
            super::balloon::Balloon::new(ceiling, std::time::Instant::now());
        self.sleeping.store(false, std::sync::atomic::Ordering::SeqCst);
        *self.link.lock().unwrap_or_else(|e| e.into_inner()) = Some(vm.clone());
        crate::diag::info!("VM booted", ms = t0.elapsed().as_millis(), cpus = cpus, memory_mib = ceiling >> 20);
        drop(waking);
        self.recover_vm();
        Ok(vm)
    }

    /// The VM if it is awake; never boots it.
    pub(super) fn awake(&self) -> Option<Arc<VmLink>> {
        self.link.lock().unwrap_or_else(|e| e.into_inner()).as_ref().filter(|vm| !vm.is_disconnected()).cloned()
    }

    fn vm_restart_candidates(&self) -> Vec<String> {
        let st = self.lock();
        let mut ids: Vec<String> = st.containers.values().filter(|c| recover_after_vm_loss(c)).map(|c| c.id.clone()).collect();
        ids.sort_by_key(|id| st.containers.get(id).is_some_and(|c| c.spec.network_mode.starts_with("container:") || !c.spec.links.is_empty()));
        ids
    }

    fn recover_vm(&self) {
        let Some(me) = self.me.get().and_then(Weak::upgrade) else { return };
        let mut st = self.lock();
        if self.is_stopped() || SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst)
            || !st.containers.values().any(recover_after_vm_loss)
        {
            return;
        }
        st.vm_recovery_generation = st.vm_recovery_generation.saturating_add(1);
        if st.vm_recovery_running {
            return;
        }
        st.vm_recovery_running = true;
        drop(st);
        // vm() also serves start requests. Recover on another thread so
        // it cannot recursively start the caller's own container.
        let worker = std::thread::Builder::new().name("asagiri-vmre".into()).spawn(move || me.run_vm_recovery());
        if let Err(e) = worker {
            self.lock().vm_recovery_running = false;
            crate::diag::error!("VM recovery worker did not start", error = e);
        }
    }

    fn run_vm_recovery(&self) {
        loop {
            let mut st = self.lock();
            if self.is_stopped() || SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst)
                || !st.containers.values().any(recover_after_vm_loss)
            {
                st.vm_recovery_running = false;
                return;
            }
            let delay = st.vm_recovery_at.map_or(Duration::ZERO, |at| at.saturating_duration_since(std::time::Instant::now()));
            if !delay.is_zero() {
                drop(self.changed.wait_timeout(st, delay).unwrap_or_else(|e| e.into_inner()));
                continue;
            }
            let generation = st.vm_recovery_generation;
            drop(st);
            let mut failed = false;
            for id in self.vm_restart_candidates() {
                if self.lock().vm_recovery_generation != generation {
                    break;
                }
                if SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) || self.is_stopped() {
                    break;
                }
                if let Err(e) = self.start_inner(&id, Start::VmRecovery) {
                    failed = true;
                    if !matches!(&e, Error::Io(e) if e.kind() == io::ErrorKind::WouldBlock) {
                        crate::diag::warn!("container did not restart after VM stop", id = &id[..12], error = format!("{e:?}"));
                    }
                    if self.awake().is_none() {
                        break;
                    }
                }
            }
            let mut st = self.lock();
            // A boot failure waits for a later request. A new loss or a
            // successful request-driven boot must leave this worker a retry.
            if failed && st.vm_recovery_generation == generation {
                st.vm_recovery_running = false;
                return;
            }
        }
    }

    pub fn touch(&self) {
        self.last_activity.store(now_secs(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Powers the VM off after `ASAGIRI_IDLE_SECS` (default 60; 0 = never)
    /// with no running containers or execs and no API requests. A VM that
    /// boots in a tenth of a second can afford to sleep, and sleeping is the
    /// one thing that gives all its memory back to macOS: Apple's balloon
    /// device takes pages from the guest but does not release them.
    /// Runs a command in a running container and collects its output.
    pub fn exec_capture(&self, key: &str, argv: &[&str]) -> Result<(i32, String), Error> {
        let config = ExecConfig {
            console_size: None,
            argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            env: Vec::new(),
            cwd: "/".into(),
            user: String::new(),
            tty: false,
            stdin: false,
            privileged: false,
            internal: true,
        };
        let id = self.exec_create(key, config)?;
        let (_, rx) = self.exec_start(&id)?;
        let mut out = Vec::new();
        for (_, data) in rx {
            out.extend_from_slice(&data);
        }
        let code = self.with_exec(&id, |e| e.exit_code.unwrap_or(-1)).unwrap_or(-1);
        self.lock().execs.remove(&id);
        Ok((code, String::from_utf8_lossy(&out).into_owned()))
    }

    /// `asagiri k8s`: every LoadBalancer service gets its port on the Mac's
    /// localhost, as OrbStack and Docker Desktop do. k3s's service load
    /// balancer already serves the port on the node, so a forward to the
    /// node container is all it takes. Checked every few seconds while the
    /// cluster runs; forwards go when services or the cluster do.
    fn start_k8s_lb(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        // `asagiri k8s set --lan on` puts them on every interface; a change rebinds them.
        let mut bound_on = String::new();
        let _ = std::thread::Builder::new().name("asagiri-k8s-lb".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_secs(3));
            let Some(engine) = me.upgrade() else { return };
            let node = engine.lock().containers.values().find(|c| c.status == Status::Running && c.spec.labels.get("dev.asagiri.k8s").is_some()).map(|c| (c.id.clone(), c.ip));
            let Some((id, Some(ip))) = node else {
                engine.k8s_lb.lock().unwrap_or_else(|e| e.into_inner()).clear();
                engine.k8s_fwd.lock().unwrap_or_else(|e| e.into_inner()).clear();
                if let Some((_, via)) = engine.k8s_routes.lock().unwrap_or_else(|e| e.into_inner()).take()
                    && let Some(vm) = engine.awake()
                {
                    for (net, bits) in K8S_RANGES {
                        let _ = vm.route(net, bits, via, false);
                    }
                }
                continue;
            };
            engine.route_k8s(ip);
            let query = r#"jsonpath={range .items[?(@.spec.type=="LoadBalancer")]}{range .spec.ports[*]}{.port}/{.protocol} {end}{end}"#;
            let Ok((0, out)) = engine.exec_capture(&id, &["timeout", "-s", "KILL", "20", "kubectl", "get", "svc", "-A", "-o", query]) else { continue };
            let wanted: Vec<(u16, String)> = out
                .split_whitespace()
                .filter_map(|p| {
                    let (port, proto) = p.split_once('/')?;
                    Some((port.parse().ok()?, proto.to_ascii_lowercase()))
                })
                .collect();
            let Some(vm) = engine.awake() else { continue };
            let host_ip = if crate::k8s::config().lan { "0.0.0.0" } else { "127.0.0.1" };
            let mut lb = engine.k8s_lb.lock().unwrap_or_else(|e| e.into_inner());
            if vm.is_disconnected() {
                continue;
            }
            if bound_on != host_ip {
                lb.clear();
                bound_on = host_ip.to_owned();
            }
            lb.retain(|k, _| wanted.contains(k));
            for (port, proto) in wanted {
                if lb.contains_key(&(port, proto.clone())) {
                    continue;
                }
                let b = super::ports::Binding { container_port: port, proto: proto.clone(), host_ip: host_ip.into(), host_port: port };
                match super::ports::start(vm.clone(), &format!("lb-{port}-{proto}"), &[b], ip) {
                    Ok(fw) => {
                        crate::diag::info!("Kubernetes service published", at = format!("{host_ip}:{port}/{proto}"));
                        lb.insert((port, proto), fw);
                    }
                    Err(e) => crate::diag::warn!("Kubernetes service port not published", port = format!("{port}/{proto}"), error = e),
                }
            }
        });
    }

    /// Routes the cluster's pod and service ranges to the node, so the
    /// relay can reach a ClusterIP or a pod as it reaches a container: the
    /// node's kube-proxy takes it from there. Again after the VM wakes (a new
    /// VM has none) or the node's address changes.
    fn route_k8s(&self, node: [u8; 4]) {
        let Some(vm) = self.awake() else { return };
        let key = (Arc::as_ptr(&vm) as usize, node);
        let mut routes = self.k8s_routes.lock().unwrap_or_else(|e| e.into_inner());
        if *routes == Some(key) {
            return;
        }
        match K8S_RANGES.iter().try_for_each(|(net, bits)| vm.route(*net, *bits, node, true)) {
            Ok(()) => *routes = Some(key),
            // An older guest can't; shares of services just can't connect.
            Err(e) => {
                if routes.is_none() {
                    crate::diag::warn!("Kubernetes ranges not routed; sharing a service needs a newer guest", error = e);
                }
                *routes = Some((0, node));
            }
        }
    }

    /// Asagiri's own: a port on the Mac's loopback that reaches `ip:port` in
    /// the cluster (a service's ClusterIP or a pod), for sharing it.
    pub fn k8s_forward(&self, ip: [u8; 4], port: u16) -> Result<u16, Error> {
        if !in_k8s_ranges(ip) || port == 0 {
            return Err(Error::BadRequest("not an address in the Kubernetes cluster".into()));
        }
        let node = self.lock().containers.values().find(|c| c.status == Status::Running && c.spec.labels.get("dev.asagiri.k8s").is_some()).and_then(|c| c.ip);
        let Some(node) = node else { return Err(Error::Conflict("Kubernetes isn't running".into())) };
        self.route_k8s(node);
        let vm = self.vm()?;
        let b = super::ports::Binding { container_port: port, proto: "tcp".into(), host_ip: "127.0.0.1".into(), host_port: 0 };
        let fw = super::ports::start(vm.clone(), &format!("k8s-share-{port}"), &[b], ip)?;
        let mac = fw.bound.first().map(|(_, _, p)| *p).unwrap_or(0);
        crate::diag::info!("Kubernetes address forwarded for sharing", to = format!("{}:{port}", std::net::Ipv4Addr::from(ip)), at = format!("127.0.0.1:{mac}"));
        let mut forwards = self.k8s_fwd.lock().unwrap_or_else(|e| e.into_inner());
        if vm.is_disconnected() {
            drop(forwards);
            return Err(Error::Io(io::Error::other("the VM stopped")));
        }
        forwards.insert(mac, fw);
        Ok(mac)
    }

    /// Closes a forward `k8s_forward` opened.
    pub fn k8s_unforward(&self, mac: u16) {
        self.k8s_fwd.lock().unwrap_or_else(|e| e.into_inner()).remove(&mac);
    }

    fn start_sleeper(self: &Arc<Self>) {
        let idle: u64 = std::env::var("ASAGIRI_IDLE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
        if idle == 0 {
            return;
        }
        let builder_idle: u64 = std::env::var("ASAGIRI_BUILDER_IDLE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
        let me = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("asagiri-sleeper".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            let Some(engine) = me.upgrade() else { return };
            // Native BuildKit connections have no dial-stdio exec to count.
            // Keep admission excluded through pausing, stopping and VM sleep;
            // a new connection resumes or starts its backend after this guard.
            let Some(_builder_idle) = super::builder::idle_guard() else { continue };
            let quiet_for = now_secs().saturating_sub(engine.last_activity.load(std::sync::atomic::Ordering::Relaxed));
            let managed = super::builder::container_name();
            // BuildKit containers (buildx's docker-container driver) run until
            // paused, which keeps the VM's state without using CPU. Between
            // builds they are paused; the next build resumes them. The managed
            // backend has its own watch, using its settings and last build time.
            let idle_builders: Vec<String> = {
                let st = engine.lock();
                let building = st.execs.values().any(|e| e.running);
                let running: Vec<&Container> = st.containers.values().filter(|c| c.status == Status::Running).collect();
                if builder_idle != 0 && !building && quiet_for >= builder_idle && !running.is_empty() && running.iter().all(|c| is_builder(c)) {
                    running.iter().filter(|c| !c.paused && c.name != managed).map(|c| c.id.clone()).collect()
                } else {
                    Vec::new()
                }
            };
            for id in &idle_builders {
                if engine.pause_idle_builder(id).is_ok() {
                    crate::diag::info!("paused an idle BuildKit container; the next build resumes it", id = &id[..12]);
                }
            }
            // A paused BuildKit container is no reason to stay awake.
            let (busy, parked) = {
                let st = engine.lock();
                let parked: Vec<String> = st.containers.values().filter(|c| c.status == Status::Running && c.paused && is_builder(c)).map(|c| c.id.clone()).collect();
                let busy = st.image_preparations != 0 || st.containers.values().any(|c| c.status == Status::Running && !(c.paused && is_builder(c))) || st.execs.values().any(|e| e.running) || usb_attached();
                (busy, parked)
            };
            let quiet = quiet_for >= idle;
            if busy || !quiet {
                continue;
            }
            // Paused, they would keep the VM up for good: stopped before it
            // sleeps, as they were before pausing; the next build starts them.
            for id in &parked {
                if let Err(e) = engine.stop(id, Duration::from_secs(10)) {
                    crate::diag::warn!("paused BuildKit container did not stop", id = &id[..12], error = format!("{e:?}"));
                }
            }
            if engine.sleep_if_idle(true) {
                crate::diag::info!("VM asleep: idle; the next request wakes it", idle_s = idle);
            }
        });
    }

    /// Admit retirement under the same lock as preparation and boot. Keep it
    /// until retirement completes so a new preparation cannot use the old VM.
    fn when_idle(&self, idle_timer: bool, retire: impl FnOnce() -> bool) -> bool {
        let _waking = self.waking.lock().unwrap_or_else(|e| e.into_inner());
        let idle = {
            let st = self.lock();
            !st.containers.values().any(|c| c.status == Status::Running)
                && (!idle_timer || (st.image_preparations == 0 && !st.execs.values().any(|e| e.running)))
                && !usb_attached()
        };
        idle && retire()
    }

    /// Powers the VM off if nothing runs in it. True if it went to sleep.
    fn sleep_if_idle(&self, idle_timer: bool) -> bool {
        // Not `link.take().filter(..)`: that takes the VM first and drops it
        // when busy, which powers it off under whatever just started.
        self.when_idle(idle_timer, || {
            let Some(vm) = self.link.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return false };
            self.sleeping.store(true, std::sync::atomic::Ordering::SeqCst);
            self.memory_used.store(0, std::sync::atomic::Ordering::Relaxed);
            self.memory_stall.store(0, std::sync::atomic::Ordering::Relaxed);
            vm.shutdown();
            // Let the guest flush its disk and power off before the VM is dropped.
            if let Err(e) = vm.retire() {
                self.sleeping.store(false, std::sync::atomic::Ordering::SeqCst);
                crate::diag::warn!("the VM did not finish stopping", error = e);
                return false;
            }
            *self.link.lock().unwrap_or_else(|e| e.into_inner()) = None;
            true
        })
    }

    /// The window's Stop: Asagiri off until Start. Marked stopped first, so
    /// nothing that comes in meanwhile boots it again; then every container
    /// stops and the backend stops. On Windows this stops the engine process,
    /// not the surrounding WSL VM.
    pub fn stop_all(&self) -> Result<(), Error> {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(f) = stopped_marker() {
            let _ = std::fs::write(f, b"");
        }
        crate::diag::info!("Asagiri stopped: the engine stays off until Start");
        self.sleep_now()
    }

    /// Start: no longer stopped (the VM boots on the next request, or now
    /// for `/asagiri/wake`).
    pub fn unstop(&self) {
        if self.stopped.swap(false, std::sync::atomic::Ordering::SeqCst) {
            crate::diag::info!("Asagiri started");
        }
        if let Some(f) = stopped_marker() {
            let _ = std::fs::remove_file(f);
        }
        // What the daemon would have restarted when it came up, had Asagiri
        // not been stopped then. Off this thread: the caller boots the VM.
        if let Some(me) = self.me.get().and_then(Weak::upgrade) {
            std::thread::spawn(move || me.start_held());
        }
    }

    /// Starts the held containers (`held_restarts`), one at a time, each
    /// taken from the list as it starts: Start, or `retry_held` once the VM
    /// boots. A Stop meanwhile ends it: the rest stay held for the next
    /// Start, and the one being started is stopped with the others.
    fn start_held(&self) {
        let _power = self.power.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.is_stopped() || SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let id = {
                let mut held = self.held_restarts.lock().unwrap_or_else(|e| e.into_inner());
                if held.is_empty() {
                    return;
                }
                held.remove(0)
            };
            let Ok(lifecycle) = self.lifecycle(&id) else { continue };
            let generation = *lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = self.start_inner(&id, Start::DaemonRestart) {
                // Stopped before it reached the VM: still held, as if not taken.
                // A concurrent docker stop owns this same generation lock;
                // never put back the restart that it has just canceled.
                let current = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
                let rehold = *current == generation && self.is_stopped() && {
                    let mut st = self.lock();
                    let c = st.containers.get_mut(&id).filter(|c| c.status != Status::Running && comes_back(c));
                    c.map(|c| c.restart_pending = true).is_some()
                };
                if rehold {
                    self.held_restarts.lock().unwrap_or_else(|e| e.into_inner()).insert(0, id);
                    return;
                }
                drop(current);
                crate::diag::warn!("container did not restart", id = &id[..12], error = format!("{e:?}"));
            }
        }
    }

    /// The VM did not boot when the daemon came up, and containers wait for
    /// it. Tries again after 10 s, then twice as long each time, up to every
    /// 5 minutes: what launchd and systemd did by starting a daemon that
    /// exited, without the exits. Ends when the containers have started, when
    /// Start took them, or when Asagiri is stopped (then Start starts them).
    fn retry_held(&self) {
        let Some(me) = self.me.get().and_then(Weak::upgrade) else { return };
        std::thread::spawn(move || {
            let mut delay = Duration::from_secs(10);
            loop {
                std::thread::sleep(delay);
                let none = me.held_restarts.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
                if none || me.is_stopped() || SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                if me.vm().is_ok() {
                    return me.start_held();
                }
                delay = (delay * 2).min(Duration::from_secs(300));
            }
        });
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Stop every container, then let the VM sleep (it wakes on the next
    /// request). The window's Stop is stop_all; restarting the VM is this,
    /// then Start. Neither is a stop by the user, as stopping Docker's daemon
    /// isn't: the containers that `always` and `unless-stopped` keep running
    /// are held, and Start (`unstop`) brings them back; a `docker stop` of
    /// one meanwhile cancels that, as it cancels any pending restart.
    pub fn sleep_now(&self) -> Result<(), Error> {
        // After a Start's restarts that are under way: what they started is
        // stopped here too.
        let _power = self.power.lock().unwrap_or_else(|e| e.into_inner());
        let mut held = std::mem::take(&mut *self.held_restarts.lock().unwrap_or_else(|e| e.into_inner()));
        {
            let mut st = self.lock();
            for c in st.containers.values_mut() {
                if (c.status == Status::Running || c.restart_pending) && comes_back(c) {
                    c.restart_pending = true;
                    if !held.contains(&c.id) {
                        held.push(c.id.clone());
                    }
                }
            }
            held.sort_by_key(|id| st.containers.get(id).is_some_and(joins_others));
        }
        *self.held_restarts.lock().unwrap_or_else(|e| e.into_inner()) = held;
        let running = self.each(|c| (c.status == Status::Running).then(|| c.id.clone()));
        // All at once, as for a daemon shutdown, each with its own stop
        // signal and timeout: seven `sleep 1d`s take one timeout, not seven.
        if let Some(vm) = self.awake().filter(|_| !running.is_empty()) {
            self.parking.store(true, std::sync::atomic::Ordering::SeqCst);
            let left = self.stop_together(&vm, &running, Stopping::Engine);
            self.parking.store(false, std::sync::atomic::Ordering::SeqCst);
            if let Some(id) = left.first() {
                return Err(Error::Io(io::Error::other(format!("cannot stop container: {}: it did not exit after SIGKILL", &id[..12]))));
            }
        }
        // Passed-through USB devices go back to the Mac, as containers stop.
        detach_usb();
        if self.sleep_if_idle(false) {
            crate::diag::info!("VM stopped on request");
        }
        Ok(())
    }

    fn ceiling(&self) -> u64 {
        self.ceiling.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// CPUs the VM has (or will boot with).
    pub fn cpus(&self) -> usize {
        self.cpus.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// CPUs a container can be given: this machine's on Linux, where
    /// containers run on its kernel, and the VM's elsewhere.
    pub fn container_cpus(&self) -> usize {
        if cfg!(target_os = "linux") { max_cpus() } else { self.cpus() }
    }

    /// The Mac folders and files that running containers bind-mount, as
    /// the Mac keeps them (the guest sees them under /mnt/mac).
    #[cfg(target_os = "macos")]
    pub fn mac_bind_sources(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .lock()
            .containers
            .values()
            .filter(|c| c.status == Status::Running)
            .flat_map(|c| c.spec.mounts.iter())
            .filter_map(|m| m.split('\t').next()?.strip_prefix("/mnt/mac").filter(|p| p.starts_with('/')).map(str::to_owned))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The VM's link while it runs, without waking it.
    #[cfg(target_os = "macos")]
    pub fn awake_link(&self) -> Option<Arc<VmLink>> {
        self.awake()
    }

    /// Whether the VM is running right now.
    pub fn is_awake(&self) -> bool {
        self.awake().is_some()
    }

    pub(super) fn lock_resource_updates(&self) -> MutexGuard<'_, ()> {
        self.resource_updates.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Restores saved containers. Ones that were running when the daemon
    /// stopped are recorded as exited, as Docker does without live restore.
    fn load(&self) -> io::Result<()> {
        let saved: Vec<Saved> = match std::fs::read(&self.records) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cannot read container records {}: {e}; startup stopped before cleanup; the original index and container data are unchanged", self.records.display()),
            ))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(io::Error::new(e.kind(), format!(
                "cannot read container records {}: {e}; startup stopped before cleanup; container data are unchanged",
                self.records.display(),
            ))),
        };
        let now = SystemTime::now();
        let mut restart = Vec::new();
        {
            let mut st = self.lock();
            for s in saved {
                let was_running = s.status == Status::Running;
                let mut c = Container {
                    lifecycle: Arc::new(Mutex::new(0)),
                    id: s.id.clone(),
                    name: s.name,
                    layers: s.layers,
                    created: from_ns(s.created_ns),
                    status: if was_running { Status::Exited } else { s.status },
                    started: s.started_ns.map(from_ns),
                    finished: if was_running { Some(now) } else { s.finished_ns.map(from_ns) },
                    exit_code: if was_running { 255 } else { s.exit_code },
                    start_error: s.start_error,
                    oom_killed: !was_running && s.oom_killed,
                    vm_stopped: was_running || s.vm_stopped,
                    stopped_by_user: s.stopped_by_user,
                    restart_count: s.restart_count,
                    restart_pending: false,
                    exits: 0,
                    size: None,
                ip: None,
                pid: 0,
                forwards: None,
                in_guest: false,
                live: false,
                early_input: Vec::new(),
                early_close: false,
                paused: false,
                extra_ips: Vec::new(),
                extra_ifs: Vec::new(),
                ip6: None,
                extra_ip6s: Vec::new(),
                    log: Arc::new(Mutex::new(super::logfile::Log::new(super::logfile::dir(self.store.root(), &s.id), &s.spec.log_config))),
                    attached: Vec::new(),
                    spec: s.spec,
                };
                if let Some(mut health) = s.health {
                    // Its monitor stopped with the daemon; Docker reports that as unhealthy.
                    if was_running {
                        health.status = super::health::UNHEALTHY.into();
                    }
                    st.health.insert(c.id.clone(), HealthRun { health, epoch: 0, monitoring: false });
                }
                if comes_back(&c) {
                    // Pending until it starts, also while held: a stop in the meantime cancels it.
                    c.restart_pending = true;
                    restart.push(c.id.clone());
                }
                st.containers.insert(s.id, c);
            }
            // Logs of containers that are gone (`--rm` ones are never saved).
            if let Ok(dirs) = std::fs::read_dir(self.store.root().join("containers")) {
                for d in dirs.flatten() {
                    if !d.file_name().to_str().is_some_and(|n| st.containers.contains_key(n)) {
                        let _ = std::fs::remove_dir_all(d.path());
                    }
                }
            }
            // Containers that join another's network or link to others start
            // after those, as Docker orders them.
            restart.sort_by_key(|id| st.containers.get(id).is_some_and(joins_others));
            drop(st);
            // Boot now only when containers restart; otherwise the engine
            // starts asleep (Ready · Sleeping) and the first request boots
            // the VM in about 0.1 s. Every boot tells the guest which
            // stopped containers to keep (vm()).
            if self.is_stopped() {
                // Stopped (the window's Stop, kept across restarts): the
                // daemon still comes up, so Start has something to talk to
                // and launchd has nothing to start again and again.
                *self.held_restarts.lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut restart);
            } else if !restart.is_empty() && self.vm().is_err() {
                // vm() has said why. The daemon stays up, since exiting only
                // had launchd start one that failed the same way every 10 s;
                // the containers wait and the VM is tried again (retry_held).
                crate::diag::warn!("the VM did not boot: the containers that restart wait while it is tried again", containers = restart.len());
                *self.held_restarts.lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut restart);
                self.retry_held();
            }
        }
        for id in restart {
            if let Err(e) = self.start_inner(&id, Start::DaemonRestart) {
                crate::diag::warn!("container did not restart", id = &id[..12], error = format!("{e:?}"));
            }
        }
        Ok(())
    }

    /// Writes `containers.json`. `--rm` containers are left out.
    fn save(&self) {
        // Snapshot and replacement must follow the same writer order.
        let _one_writer = self.saving.lock().unwrap_or_else(|e| e.into_inner());
        let json = {
            let st = self.lock();
            let saved: Vec<Saved> = st
                .containers
                .values()
                .filter(|c| !c.spec.auto_remove)
                .map(|c| Saved {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    spec: c.spec.clone(),
                    layers: c.layers.clone(),
                    created_ns: ns(c.created),
                    status: c.status,
                    started_ns: c.started.map(ns),
                    finished_ns: c.finished.map(ns),
                    exit_code: c.exit_code,
                    start_error: c.start_error.clone(),
                    oom_killed: c.oom_killed,
                    vm_stopped: c.vm_stopped,
                    stopped_by_user: c.stopped_by_user,
                    restart_count: c.restart_count,
                    health: st.health.get(&c.id).map(|h| h.health.clone()),
                })
                .collect();
            serde_json::to_vec(&saved)
        };
        let Ok(json) = json else { return };
        let tmp = self.records.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &json).and_then(|()| std::fs::rename(&tmp, &self.records)) {
            crate::diag::error!("saving containers failed", error = e);
        }
    }

    fn on_event(&self, ev: Event<'_>) {
        match ev {
            Event::Output { id, stream, data } => {
                let mut st = self.lock();
                if let Some(c) = st.containers.get_mut(id) {
                    c.attached.retain(|tx| tx.send((stream, data.to_vec())).is_ok());
                    // Written outside the engine's lock; holding the log's
                    // keeps the order `docker logs -f` sees.
                    let log = c.log.clone();
                    let mut log = lock_log(&log);
                    drop(st);
                    log.append(stream, ns(SystemTime::now()), data);
                } else if let Some(e) = st.execs.get_mut(id) {
                    e.attached.retain(|tx| tx.send((stream, data.to_vec())).is_ok());
                }
            }
            Event::Exited { id, code, oom } => {
                let mut st = self.lock();
                if let Some(e) = st.execs.get_mut(id) {
                    e.running = false;
                    e.exit_code = Some(code);
                    e.pid = 0;
                    e.attached.clear();
                    let (container, exec_id, internal) = (e.container_id.clone(), e.id.clone(), e.config.internal);
                    drop(st);
                    self.changed.notify_all();
                    if !internal {
                        self.emit_container("exec_die", &container, &[("execID", exec_id), ("exitCode", code.to_string())]);
                    }
                    return;
                }
                let mut restart_in = None;
                let mut persist = false;
                let mut closed_ports = None;
                let actor = st.containers.get(id).and_then(|c| container_attrs(c).map(|attrs| (c.spec.image.clone(), attrs)));
                // Docker stops a container's healthcheck when it exits.
                if let Some(h) = st.health.get_mut(id) {
                    close_monitor(h);
                }
                let remove = match st.containers.get_mut(id) {
                    Some(c) => {
                        c.status = Status::Exited;
                        c.exit_code = code;
                        c.pid = 0;
                        c.oom_killed = oom;
                        // Stopped by the daemon's shutdown, not by the app.
                        c.vm_stopped = SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst);
                        if oom {
                            crate::diag::warn!("container killed for memory", name = c.name.clone(), exit_code = code);
                        }
                        c.exits += 1;
                        c.in_guest = false;
                        c.live = false;
                        if let Some(ip) = c.ip.take() {
                            self.release_ip(&c.spec.network_mode, ip);
                        }
                        for (net, ip) in std::mem::take(&mut c.extra_ips) {
                            self.release_ip(&net, ip);
                        }
                        c.extra_ifs.clear();
                        self.release_ip6s(c);
                        c.paused = false;
                        closed_ports = c.forwards.take();
                        c.finished = Some(SystemTime::now());
                        // Dropping the senders ends every attach stream.
                        c.attached.clear();
                        lock_log(&c.log).end_follow();
                        persist = !c.spec.auto_remove;
                        let p = &c.spec.restart;
                        let wants = match p.name.as_str() {
                            "always" | "unless-stopped" => !c.stopped_by_user,
                            "on-failure" => code != 0 && !c.stopped_by_user && (p.max_retries == 0 || c.restart_count < p.max_retries),
                            _ => false,
                        };
                        // Not for the engine's own stop (Stop, the VM's sleep on
                        // request): Start brings those back.
                        let engine_stop = SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst) || self.parking.load(std::sync::atomic::Ordering::SeqCst) || self.is_stopped();
                        if wants && !c.spec.auto_remove && !engine_stop {
                            let backoff = restart_backoff(c.restart_count);
                            c.restart_count = c.restart_count.saturating_add(1);
                            c.restart_pending = true;
                            restart_in = Some(backoff);
                        }
                        c.spec.auto_remove
                    }
                    None => false,
                };
                let mut anon = Vec::new();
                let mut removed_log = None;
                let mut removed_blobs = Vec::new();
                if remove {
                    if let Some(c) = st.containers.remove(id) {
                        removed_blobs = std::iter::once(c.spec.image_id.clone()).chain(c.layers).collect();
                        anon = c.spec.anon_volumes;
                        removed_log = Some(c.log);
                    }
                    note_removed(&mut st, id, code);
                }
                drop(st);
                self.release_image_blobs(&removed_blobs);
                if let Some((image, attrs)) = actor {
                    self.emit_container_with("die", id, &image, attrs.clone(), &[("exitCode", code.to_string())]);
                    if remove {
                        self.emit_container_with("destroy", id, &image, attrs, &[]);
                    }
                }
                // Closing listeners joins their threads: never under the lock.
                drop(closed_ports);
                if let Some(log) = removed_log {
                    lock_log(&log).remove();
                }
                // `--rm` takes the container's anonymous volumes with it.
                for v in anon {
                    self.delete_volume_data(&v);
                }
                self.changed.notify_all();
                if persist {
                    self.save();
                }
                if let (Some(delay), Some(me)) = (restart_in, self.me.get().and_then(Weak::upgrade)) {
                    let id = id.to_owned();
                    let _ = std::thread::Builder::new().name("asagiri-restart".into()).spawn(move || {
                        std::thread::sleep(delay);
                        // Only if nobody stopped or removed it in the meantime.
                        // start_inner checks again, under its lock, that the
                        // restart is still pending: a stop between here and
                        // there cancels it.
                        let still_wanted = !me.is_stopped() && me.lock().containers.get(&id).is_some_and(|c| c.status == Status::Exited && !c.stopped_by_user);
                        if still_wanted {
                            if let Err(e) = me.start_inner(&id, Start::Restart) {
                                crate::diag::warn!("container did not restart", id = &id[..12], error = format!("{e:?}"));
                            }
                        }
                    });
                }
            }
            Event::Pressure => self.grow_memory(),
            Event::Lost => {
                let mut st = self.lock();
                let mut closed = Vec::new();
                for c in st.containers.values_mut() {
                    closed.extend(c.forwards.take());
                    if let Some(ip) = c.ip.take() {
                        self.release_ip(&c.spec.network_mode, ip);
                    }
                    for (net, ip) in std::mem::take(&mut c.extra_ips) {
                        self.release_ip(&net, ip);
                    }
                    c.extra_ifs.clear();
                    self.release_ip6s(c);
                    c.paused = false;
                    c.pid = 0;
                    if c.status == Status::Running {
                        c.status = Status::Exited;
                        c.exits += 1;
                        c.exit_code = 255;
                        c.vm_stopped = true;
                        c.finished = Some(SystemTime::now());
                        c.attached.clear();
                        lock_log(&c.log).end_follow();
                    }
                    if recover_after_vm_loss(c) {
                        c.restart_count = c.restart_count.saturating_add(1);
                    }
                }
                let delay = st.containers.values().filter(|c| recover_after_vm_loss(c))
                    .map(|c| restart_backoff(c.restart_count.saturating_sub(1))).max();
                st.vm_recovery_at = delay.map(|delay| std::time::Instant::now() + delay);
                for e in st.execs.values_mut() {
                    if e.running {
                        e.exit_code = Some(255);
                    }
                    e.running = false;
                    e.attached.clear();
                }
                for h in st.health.values_mut() {
                    close_monitor(h);
                }
                drop(st);
                drop(closed);
                self.k8s_lb.lock().unwrap_or_else(|e| e.into_inner()).clear();
                self.k8s_fwd.lock().unwrap_or_else(|e| e.into_inner()).clear();
                self.k8s_routes.lock().unwrap_or_else(|e| e.into_inner()).take();
                self.changed.notify_all();
                match vm_end(self.sleeping.load(std::sync::atomic::Ordering::SeqCst), SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst)) {
                    // sleep_if_idle says why it slept.
                    VmEnd::Slept => {}
                    // The daemon's own shutdown (`systemctl stop asagiri`, quitting Asagiri): asked for.
                    VmEnd::ShutDown => {
                        self.save();
                        crate::diag::info!("the VM stopped");
                    }
                    VmEnd::Crashed => {
                        self.save();
                        crate::diag::error!("the VM stopped unexpectedly", console = crate::diag::console::path().map(|p| p.display().to_string()).unwrap_or_default());
                        // vm() releases this dead link after the reader has
                        // finished exit accounting. Never wait for it here.
                        self.recover_vm();
                    }
                }
            }
        }
    }

    /// Every second: read what the guest holds and, on the Mac, give it that
    /// plus headroom through the balloon, the rest back to macOS. Pressure
    /// events grow it at once in between. On a Linux host there is no
    /// balloon and nothing to give back: the reading is all, and it takes in
    /// this process too, which runs beside the containers there.
    fn start_balloon(self: &Arc<Self>) {
        if std::env::var_os("ASAGIRI_NO_BALLOON").is_some() {
            return;
        }
        let me = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("asagiri-balloon".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let Some(engine) = me.upgrade() else { return };
                let Some(vm) = engine.awake() else { continue };
                let Ok(stats) = vm.stats() else { continue };
                let mut target = engine.memory_target.lock().unwrap_or_else(|e| e.into_inner());
                let ceiling = engine.ceiling();
                let used = if stats.mem_used_kib > 0 {
                    // The agent's count: anonymous, shared and kernel memory
                    // from the cgroup, which neither the page cache nor the
                    // balloon's own pages get into.
                    stats.mem_used_kib.saturating_mul(1024).saturating_add(if cfg!(target_os = "macos") { 0 } else { self_rss() })
                } else {
                    // An agent from before that count: MemTotal minus
                    // MemAvailable, with the balloon taken back out. Apple's
                    // balloon offers deflate-on-OOM, so Linux counts its pages
                    // as used rather than shrinking MemTotal. The balloon is
                    // guessed at ceiling - target, which it only is once the
                    // guest has inflated that far: short of that, the guess
                    // exceeds the use and the result sits at zero.
                    let apparent = stats.mem_total_kib.saturating_sub(stats.mem_available_kib).saturating_mul(1024);
                    apparent.saturating_sub(ceiling.saturating_sub(target.target))
                };
                engine.memory_used.store(used, std::sync::atomic::Ordering::Relaxed);
                engine.memory_stall.store(stats.mem_stall, std::sync::atomic::Ordering::Relaxed);
                engine.oom_kills.store(stats.oom_kills, std::sync::atomic::Ordering::Relaxed);
                if !cfg!(target_os = "macos") {
                    continue;
                }
                if let Some(next) = target.sample(used, stats.mem_working_set_kib.saturating_mul(1024),
                    stats.mem_refaults, stats.mem_stall, stats.oom_kills, ceiling, std::time::Instant::now())
                {
                    vm.set_memory_target(next);
                }
            }
        });
    }

    fn grow_memory(&self) {
        let mut target = self.memory_target.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(next) = target.pressure(self.ceiling(), std::time::Instant::now()) {
            if let Some(vm) = self.awake() {
                vm.set_memory_target(next);
            }
        }
    }

    /// (ceiling, current target) in bytes. On a Linux host there is no VM
    /// of Asagiri's to bound: both are the machine's memory (in WSL, the memory
    /// .wslconfig gives it), so the window shows use against what is there.
    pub fn memory(&self) -> (u64, u64) {
        if cfg!(target_os = "macos") {
            (self.ceiling(), self.memory_target.lock().unwrap_or_else(|e| e.into_inner()).target)
        } else {
            let ram = physical_memory();
            (ram, ram)
        }
    }

    /// What Asagiri's Linux holds now, in bytes; 0 while asleep.
    pub fn memory_used(&self) -> u64 {
        self.memory_used.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The share of the last 10 s the guest spent stalled on memory, in
    /// percent, and the processes it has killed for memory since it booted.
    pub fn memory_trouble(&self) -> (f64, u64) {
        let stall = self.memory_stall.load(std::sync::atomic::Ordering::Relaxed) as f64 / 100.0;
        (stall, self.oom_kills.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// How busy the VM is, in percent of all its CPUs; 0 while asleep.
    pub fn cpu_percent(&self) -> f64 {
        f64::from(self.cpu_permyriad.load(std::sync::atomic::Ordering::Relaxed)) / 100.0
    }

    /// Keeps the VM's clock on the Mac's. The guest is told the time at
    /// boot, but its clock stops while the Mac sleeps, so after each sleep it
    /// fell further behind: an hour behind after a few nights, and apt
    /// refused Debian's signatures as made in the future. Sent again within
    /// a few seconds of a wake (the wall clock moved on further than the
    /// monotonic one, which stops in sleep too), and once a minute anyway;
    /// the agent only steps its clock when it is off.
    fn start_clock(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("asagiri-clock".into()).spawn(move || {
            let (mut wall, mut mono) = (SystemTime::now(), std::time::Instant::now());
            let mut sent = std::time::Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let Some(engine) = me.upgrade() else { return };
                let (w, m) = (SystemTime::now(), std::time::Instant::now());
                let slept = w.duration_since(wall).unwrap_or_default().saturating_sub(m.duration_since(mono)) > Duration::from_secs(2);
                (wall, mono) = (w, m);
                if !slept && sent.elapsed() < Duration::from_secs(60) {
                    continue;
                }
                if let Some(vm) = engine.awake()
                    && vm.set_time().is_ok()
                {
                    sent = m;
                }
            }
        });
    }

    /// Reads the VM's CPU time every two seconds, for `cpu_percent`.
    fn start_cpu_meter(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("asagiri-cpu".into()).spawn(move || {
            let mut last: Option<(u64, std::time::Instant)> = None;
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let Some(engine) = me.upgrade() else { return };
                let now = engine.awake().and_then(|_| super::vmcpu::cpu_ns()).map(|ns| (ns, std::time::Instant::now()));
                let permyriad = match (last, now) {
                    (Some((a, t0)), Some((b, t1))) if b >= a => {
                        let wall = t1.duration_since(t0).as_nanos() as f64 * engine.cpus().max(1) as f64;
                        ((b - a) as f64 / wall * 10000.0).round().clamp(0.0, 10000.0) as u32
                    }
                    _ => 0,
                };
                engine.cpu_permyriad.store(permyriad, std::sync::atomic::Ordering::Relaxed);
                last = now;
            }
        });
    }

    /// Finds an image by the name a client used.
    pub fn image(&self, name: &str) -> io::Result<Option<Image>> {
        Ok(image_named(self.store.images()?, name))
    }

    /// A forced image removal must not discard data its containers still own.
    pub(super) fn image_blob_users(&self) -> Vec<String> {
        self.each(|c| Some(std::iter::once(c.spec.image_id.clone()).chain(c.layers.iter().cloned()).collect::<Vec<_>>()))
            .into_iter().flatten().collect()
    }

    fn release_image_blobs(&self, candidates: &[String]) {
        if candidates.is_empty() {
            return;
        }
        if let Err(error) = self.store.release_blobs(candidates, &self.image_blob_users()) {
            crate::diag::warn!("kept unused image blobs after container removal", error = error);
        }
    }

    /// Sends the guest every layer of these (digest, media type) it doesn't
    /// have unpacked, from the blob store.
    fn ensure_layers<'a>(&self, vm: &VmLink, layers: impl Iterator<Item = (&'a str, &'a str)>, cancelled: &dyn Fn() -> bool) -> io::Result<()> {
        for (digest, media_type) in layers {
            check_preparation(cancelled)?;
            self.layers.run(digest, cancelled, || {
                check_preparation(cancelled)?;
                if !vm.has_layer(digest)? {
                    vm.send_layer(digest, media_type, &self.store.blob_path(digest)?)?;
                }
                Ok::<_, io::Error>(())
            })?;
        }
        check_preparation(cancelled)
    }

    fn image_preparation(&self) -> ImagePreparation<'_> {
        // Register before vm(), including while waiting for a concurrent boot.
        let _waking = self.waking.lock().unwrap_or_else(|e| e.into_inner());
        self.lock().image_preparations += 1;
        ImagePreparation(self)
    }

    /// A successful pull or load leaves its layers ready for container creation.
    pub fn prepare_image(&self, image: &Image, cancelled: &dyn Fn() -> bool) -> io::Result<()> {
        check_preparation(cancelled)?;
        if image.layers.is_empty() {
            return Ok(());
        }
        let _preparing = self.image_preparation();
        let vm = self.vm()?;
        self.ensure_layers(&vm, image.layers.iter().map(|l| (l.digest.as_str(), l.media_type.as_str())), cancelled)
    }

    /// A container's layers back in the guest before it runs again: the
    /// guest deletes an unpacked layer at boot when it isn't whole (an unpack
    /// cut short, or one from before layers were marked whole), and a
    /// container created before that would otherwise fail to start with
    /// "layer ... is not unpacked". After forced image deletion its container
    /// still owns the compressed blob, whose format supplies the media type.
    fn restore_layers(&self, vm: &VmLink, layers: &[String]) -> Result<(), Error> {
        let mut missing = Vec::new();
        for digest in layers {
            if !vm.has_layer(digest)? {
                missing.push(digest);
            }
        }
        // The usual case: nothing to send, and no need to read the image index.
        if missing.is_empty() {
            return Ok(());
        }
        let images = self.store.images()?;
        let mut wanted = Vec::with_capacity(missing.len());
        for digest in missing {
            let media_type = match images.iter().flat_map(|i| &i.layers).find(|l| &l.digest == digest) {
                Some(layer) => layer.media_type.as_str(),
                None => asagiri_image::archive::media_type_of(&self.store.blob_path(digest)?)?,
            };
            wanted.push((digest.as_str(), media_type));
        }
        Ok(self.ensure_layers(vm, wanted.into_iter(), &|| false)?)
    }

    /// Records a container and makes sure its layers are in the guest, so
    /// `start` has nothing left to do but run it.
    pub fn create(&self, name: Option<String>, spec: Spec, image: &Image) -> Result<String, Error> {
        self.prepare_image(image, &|| false)?;
        let id = random_hex(32);
        let persist = !spec.auto_remove;
        let (log_dir, log_config) = (super::logfile::dir(self.store.root(), &id), spec.log_config.clone());
        let mut st = self.lock();
        let name = match name.filter(|n| !n.is_empty()) {
            Some(n) => {
                let n = n.trim_start_matches('/').to_owned();
                if !valid_container_name(&n) {
                    return Err(Error::BadRequest(format!("Invalid container name ({n}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed")));
                }
                if st.containers.values().any(|c| c.name == n) {
                    return Err(Error::Conflict(format!(
                        "Conflict. The container name \"/{n}\" is already in use. You have to remove (or rename) that container to be able to reuse that name."
                    )));
                }
                n
            }
            None => loop {
                let n = super::names::random();
                if !st.containers.values().any(|c| c.name == n) {
                    break n;
                }
            },
        };
        st.containers.insert(
            id.clone(),
            Container {
                lifecycle: Arc::new(Mutex::new(0)),
                id: id.clone(),
                name,
                layers: image.layers.iter().rev().map(|l| l.digest.clone()).collect(),
                spec,
                created: SystemTime::now(),
                status: Status::Created,
                started: None,
                finished: None,
                exit_code: 0,
                start_error: String::new(),
                oom_killed: false,
                vm_stopped: false,
                stopped_by_user: false,
                restart_count: 0,
                restart_pending: false,
                exits: 0,
                size: None,
                ip: None,
                pid: 0,
                forwards: None,
                in_guest: false,
                live: false,
                early_input: Vec::new(),
                early_close: false,
                paused: false,
                extra_ips: Vec::new(),
                extra_ifs: Vec::new(),
                ip6: None,
                extra_ip6s: Vec::new(),
                log: Arc::new(Mutex::new(super::logfile::Log::new(log_dir, &log_config))),
                attached: Vec::new(),
            },
        );
        drop(st);
        if persist {
            self.save();
        }
        self.emit_container("create", &id, &[]);
        Ok(id)
    }

    /// Resolves an id, an id prefix or a name to the full id.
    pub fn resolve(&self, key: &str) -> Result<String, Error> {
        let st = self.lock();
        resolve(&st, key)
    }

    fn lifecycle(&self, key: &str) -> Result<Arc<Mutex<u64>>, Error> {
        let st = self.lock();
        let id = resolve(&st, key)?;
        Ok(st.containers[&id].lifecycle.clone())
    }

    /// `docker start`: also clears a previous `stop`, as Docker does.
    pub fn start(&self, key: &str) -> Result<(), Error> {
        self.start_inner(key, Start::User)
    }

    fn start_inner(&self, key: &str, reason: Start) -> Result<(), Error> {
        let lifecycle = self.lifecycle(key)?;
        let _starting = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let generation = *_starting;
        {
            let st = self.lock();
            let id = resolve(&st, key)?;
            let c = &st.containers[&id];
            if c.status == Status::Running || (reason == Start::VmRecovery && !recover_after_vm_loss(c))
                || (reason == Start::Restart && c.stopped_by_user)
                || (matches!(reason, Start::Restart | Start::DaemonRestart) && !c.restart_pending)
            {
                return Ok(());
            }
            if c.spec.entrypoint.is_empty() && c.spec.cmd.is_empty() {
                return Err(Error::BadRequest("no command specified".into()));
            }
        }
        drop(_starting);
        // Retirement and boot own no container lock. A manual action can
        // invalidate this attempt while the previous VM is stopping.
        let _preparing = self.image_preparation();
        let prepared = self.vm_inner(reason == Start::VmRecovery);
        let _starting = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if *_starting != generation {
            return Err(Error::Conflict("container start was canceled".into()));
        }
        let vm = match prepared {
            Ok(vm) => vm,
            Err(e) => {
                if reason != Start::VmRecovery {
                    let (id, spec) = self.with(key, |c| (c.id.clone(), c.spec.clone()))?;
                    self.fail_start(&id, &spec, &e);
                }
                return Err(Error::Io(e));
            }
        };
        self.start_on_vm(key, reason, vm, _starting)
    }

    /// Own the lifecycle lock through guest creation, then release it before
    /// input delivery, which may wait indefinitely for the program to read.
    fn start_on_vm(&self, key: &str, reason: Start, vm: Arc<VmLink>, starting: MutexGuard<'_, u64>) -> Result<(), Error> {
        let (id, layers, argv, mut spec, size) = {
            let mut st = self.lock();
            let id = resolve(&st, key)?;
            // Under the same lock as Lost: either this claim belongs to
            // the live VM, or Lost will observe it as Running and end it.
            if vm.is_disconnected() {
                let spec = st.containers[&id].spec.clone();
                drop(st);
                let _ = vm.is_lost();
                if reason != Start::VmRecovery {
                    self.fail_start(&id, &spec, &"the VM stopped");
                }
                return Err(Error::Io(io::Error::other("the VM stopped")));
            }
            let c = st.containers.get_mut(&id).expect("resolved");
            if (reason == Start::VmRecovery && !recover_after_vm_loss(c)) || (reason == Start::Restart && c.stopped_by_user) {
                return Ok(());
            }
            match c.status {
                Status::Running => return Ok(()),
                Status::Exited if c.spec.auto_remove => return Err(Error::Conflict("container is being removed".into())),
                _ => {}
            }
            // A start by the restart policy: only while it is still pending.
            // A `stop` takes that away (mark_stopped) and answered as if it
            // ended the container, so it must not start after all.
            if matches!(reason, Start::Restart | Start::DaemonRestart) && !std::mem::take(&mut c.restart_pending) {
                return Ok(());
            }
            let argv = c.argv();
            if argv.is_empty() {
                return Err(Error::BadRequest("no command specified".into()));
            }
            c.status = Status::Running;
            c.restart_pending = false;
            c.started = Some(SystemTime::now());
            c.finished = None;
            c.oom_killed = false;
            c.vm_stopped = false;
            // Not until the guest has the process, however the last run ended.
            c.in_guest = false;
            c.live = false;
            // `docker run --rm` without -d: its client is reading the output
            // and the log goes with the container, so it stays in memory.
            if c.spec.auto_remove && !c.attached.is_empty() {
                lock_log(&c.log).keep_in_memory();
            }
            if reason == Start::User {
                c.stopped_by_user = false;
                c.restart_count = 0;
            }
            (id, c.layers.clone(), argv, c.spec.clone(), c.size.or(c.spec.console_size))
        };
        // A bind source that is a link is followed again at every start, as
        // Docker does: re-pointing it between runs (a `current` release
        // link) mounts the new target.
        spec.mounts = super::api::fresh_bind_sources(&spec.mounts, &spec.mount_info);
        let mut hostname = if spec.hostname.is_empty() { id[..12].to_owned() } else { spec.hostname.clone() };
        let mut domainname = spec.domainname.clone();
        // `--network container:<id>`: that container's namespace, hostname
        // and /etc files, as with Docker.
        let mut joined_mode = None;
        let joined = match spec.network_mode.strip_prefix("container:") {
            Some(target) => match self.network_container(&id, target) {
                Ok((target, h, d, mode)) => {
                    (hostname, domainname) = (h, d);
                    joined_mode = Some(mode);
                    Some(target)
                }
                Err(e) => {
                    self.fail_start(&id, &spec, &e);
                    return Err(e);
                }
            },
            None => None,
        };
        let mut flags = 0;
        let net = self.lock_networks().find(&spec.network_mode).cloned();
        if let Some(Err(e)) = net.as_ref().map(|n| n.check_mac(&spec.mac_address)) {
            self.fail_start(&id, &spec, &e);
            return Err(Error::Io(io::Error::other(e)));
        }
        let links = match net.as_ref().map(|n| self.link_hosts(&id, &spec, n)).transpose() {
            Ok(l) => l.unwrap_or_default(),
            Err(e) => {
                self.fail_start(&id, &spec, &e);
                return Err(e);
            }
        };
        let mut dns_names: Vec<String> = Vec::new();
        let mut net6_opts: Vec<String> = Vec::new();
        let (bridge, ip, prefix, gateway) = match net.as_ref().map(|n| n.driver.as_str()) {
            Some("host") => {
                flags |= run_flags::NET_HOST;
                (String::new(), [0; 4], 0, [0; 4])
            }
            Some("bridge" | "macvlan" | "ipvlan") => {
                let n = net.as_ref().expect("matched");
                let ip = match super::network::parse_ip(&spec.ipv4_address) {
                    Some(want) => self.lock_networks().reserve(&n.name, want).map(|()| want).map_err(Error::Conflict),
                    None => {
                        let avoid = self.wanted_addresses(&n.name);
                        self.lock_networks().allocate_avoiding(&n.name, &avoid).ok_or_else(|| Error::Io(io::Error::other(format!("no free address left on network {}", n.name))))
                    }
                };
                let ip = match ip {
                    Ok(ip) => ip,
                    Err(e) => {
                        self.fail_start(&id, &spec, &e);
                        return Err(e);
                    }
                };
                let ip6 = if spec.ipv6_disabled("eth0") { Ok(None) } else { self.allocate_ip6(n, &spec.ipv6_address) };
                match ip6 {
                    Ok(ip6) => {
                        if let Some(c) = self.lock().containers.get_mut(&id) {
                            c.ip6 = ip6;
                        }
                        if let (Some(a), Some(v)) = (ip6, &n.v6) {
                            net6_opts.push(format!("{}={}/{}", asagiri_proto::net_opts::IP6, super::network::ip6_str(a), v.prefix));
                            net6_opts.push(format!("{}={}", asagiri_proto::net_opts::GW6, super::network::ip6_str(v.gateway)));
                        }
                        if n.internal {
                            net6_opts.push(format!("{}=1", asagiri_proto::net_opts::INTERNAL));
                        }
                        if n.no_ipv4 {
                            net6_opts.push(format!("{}=1", asagiri_proto::net_opts::NO_IPV4));
                        }
                        net6_opts.push(format!("{}={}", asagiri_proto::net_opts::NETWORK, n.name));
                        if let Some(mtu) = n.mtu() {
                            net6_opts.push(format!("{}={mtu}", asagiri_proto::net_opts::MTU));
                        }
                        if !n.icc() {
                            net6_opts.push(format!("{}=1", asagiri_proto::net_opts::NO_ICC));
                        }
                        if let Some(mode) = n.macvlan_mode() {
                            net6_opts.push(format!("{}={mode}", asagiri_proto::net_opts::MACVLAN));
                        }
                        if let Some((mode, flag)) = n.ipvlan() {
                            net6_opts.push(format!("{}={mode}", asagiri_proto::net_opts::IPVLAN));
                            net6_opts.push(format!("{}={flag}", asagiri_proto::net_opts::IPVLAN_FLAG));
                        }
                    }
                    Err(e) => {
                        self.lock_networks().release(&n.name, ip);
                        self.fail_start(&id, &spec, &e);
                        return Err(e);
                    }
                }
                // User-defined networks resolve container names, as Docker's do;
                // the default bridge does not.
                if !n.builtin() {
                    flags |= run_flags::DNS_GATEWAY;
                    let name = self.lock().containers.get(&id).map(|c| c.name.clone()).unwrap_or_default();
                    dns_names = endpoint_names(&name, &spec.aliases, &id, &hostname);
                }
                (n.bridge.clone(), ip, n.prefix, n.gateway)
            }
            Some(_) => {
                flags |= run_flags::NET_NONE;
                (String::new(), [0; 4], 0, [0; 4])
            }
            None if joined.is_some() => (String::new(), [0; 4], 0, [0; 4]),
            None => {
                let e = Error::NotFound(format!("network {} not found", spec.network_mode));
                self.fail_start(&id, &spec, &e);
                return Err(e);
            }
        };
        if ip != [0; 4] {
            if let Some(c) = self.lock().containers.get_mut(&id) {
                c.ip = Some(ip);
            }
        }
        // A restarted container keeps its writable layer, so only --rm deletes it.
        if spec.auto_remove {
            flags |= run_flags::REMOVE;
        }
        if spec.tty {
            flags |= run_flags::TTY;
        }
        if spec.open_stdin {
            flags |= run_flags::STDIN;
        }
        if spec.privileged {
            flags |= run_flags::PRIVILEGED;
        }
        if spec.init == Some(true) {
            flags |= run_flags::INIT;
        }
        flags |= spec.security_flags();
        let hosts = hosts_lines(&spec.extra_hosts, links);
        let mut opts = net_opts(&spec, joined.as_deref(), &domainname);
        opts.extend(net6_opts);
        let net_args = asagiri_proto::Net { bridge: &bridge, ip, prefix, gateway, extra_hosts: &hosts, dns_names: &dns_names, opts: &opts };
        // Joining a host-network container is the host's namespace too: its
        // net.* sysctls would change the whole VM (or Linux host).
        let joined_host = joined_mode.is_some_and(|m| self.lock_networks().find(&m).is_some_and(|n| n.driver == "host"));
        let host_opts = match spec.guest_opts(flags & run_flags::NET_HOST != 0 || joined_host) {
            Ok(o) => o,
            Err(e) => {
                self.fail_start(&id, &spec, &e);
                return Err(e);
            }
        };
        if let Err(e) = self.restore_layers(&vm, &layers) {
            self.fail_start(&id, &spec, &e);
            return Err(e);
        }
        match vm.run(&id, &layers, &argv, &spec.env, &spec.cwd, &spec.user, &hostname, flags, net_args, &spec.mounts, &spec.limits.wire(), spec.caps, &host_opts) {
            Ok(pid) => {
                if let Some(c) = self.lock().containers.get_mut(&id).filter(|c| c.status == Status::Running) {
                    c.pid = pid;
                }
            }
            Err(e) => {
                crate::diag::warn!("container did not start", id = &id[..12], image = spec.image, error = e);
                self.fail_start(&id, &spec, &e);
                return Err(start_error(e));
            }
        }
        if let (true, Some((rows, cols))) = (spec.tty, size) {
            let _ = vm.resize(&id, rows, cols);
        }
        if flags & (run_flags::NET_HOST | run_flags::NET_NONE) == 0 && joined.is_none() {
            for extra in &spec.extra_networks {
                if let Err(e) = self.attach_extra(&vm, &id, extra) {
                    crate::diag::warn!("container not attached to a network", id = &id[..12], network = extra.name, error = format!("{e:?}"));
                }
            }
        }
        // Nothing outside a macvlan or ipvlan network reaches its
        // containers: Docker publishes no ports for them either.
        let own_link = net.as_ref().is_some_and(|n| n.own_link());
        if !spec.ports.is_empty() && !own_link {
            let target = if flags & run_flags::NET_HOST != 0 { [127, 0, 0, 1] } else { ip };
            match super::ports::start(vm.clone(), &id, &spec.ports, target) {
                Ok(fw) => {
                    let mut st = self.lock();
                    if let Some(c) = st.containers.get_mut(&id).filter(|c| c.status == Status::Running && !vm.is_disconnected()) {
                        c.forwards = Some(fw);
                    } else {
                        drop(st);
                        drop(fw);
                    }
                }
                Err(e) => {
                    crate::diag::warn!("ports not published; container stopped", id = &id[..12], error = e);
                    let message = e.to_string();
                    if let Some(c) = self.lock().containers.get_mut(&id) {
                        c.start_error = message.clone();
                    }
                    // Docker refuses to run a container whose ports clash.
                    let _ = vm.kill(&id, libc::SIGKILL as u32);
                    if !spec.auto_remove {
                        self.save();
                    }
                    return Err(Error::Conflict(message));
                }
            }
        }
        if vm.is_disconnected() {
            self.fail_start(&id, &spec, &"the VM stopped");
            return Err(Error::Io(io::Error::other("the VM stopped")));
        }
        // The guest has the process now. A stop or kill that came in meanwhile
        // waits for this, and not for the input below: handing that over
        // takes as long as the program takes to read it.
        if let Some(c) = self.lock().containers.get_mut(&id) {
            // A short command may already have exited before Started arrived.
            // It still started successfully and must clear the previous error.
            c.start_error.clear();
            if c.status == Status::Running {
                c.in_guest = true;
            }
        }
        self.changed.notify_all();
        drop(starting);
        // Deliver input that arrived early.
        // It is drained before the container goes live, so input still
        // arriving meanwhile (and its CloseStdin) queues behind it rather
        // than overtaking it.
        loop {
            let (early, close) = {
                let mut st = self.lock();
                let Some(c) = st.containers.get_mut(&id) else { break };
                let early = std::mem::take(&mut c.early_input);
                if !early.is_empty() {
                    (early, false)
                } else {
                    c.live = true;
                    (early, std::mem::take(&mut c.early_close))
                }
            };
            if early.is_empty() {
                if close {
                    let _ = vm.close_stdin(&id);
                }
                break;
            }
            let _ = vm.input(&id, &early);
        }
        self.health_start(&id);
        if !spec.auto_remove {
            self.save();
        }
        self.emit_container("start", &id, &[]);
        Ok(())
    }

    /// Sends an event to every listener and keeps it for `--since`.
    pub fn emit(&self, typ: &str, action: &str, actor: &str, attributes: serde_json::Map<String, serde_json::Value>, legacy: Option<(&str, &str)>) {
        log_event(typ, action, actor, &attributes);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let mut ev = serde_json::json!({
            "Type": typ,
            "Action": action,
            "Actor": {"ID": actor, "Attributes": attributes},
            "scope": "local",
            "time": now.as_secs(),
            "timeNano": now.as_nanos() as u64,
        });
        // Old clients read `status`, `id` and `from` on container events.
        if let Some((id, from)) = legacy {
            ev["status"] = action.into();
            ev["id"] = id.into();
            ev["from"] = from.into();
        }
        let mut e = self.events.lock().unwrap_or_else(|e| e.into_inner());
        e.recent.push_back(ev.clone());
        while e.recent.len() > RECENT_EVENTS {
            e.recent.pop_front();
        }
        e.listeners.retain(|tx| !matches!(tx.try_send(ev.clone()), Err(std::sync::mpsc::TrySendError::Disconnected(_))));
    }

    /// Kept events, and a receiver for the ones to come.
    pub fn subscribe(&self) -> (Vec<serde_json::Value>, Receiver<serde_json::Value>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(EVENT_QUEUE);
        let mut e = self.events.lock().unwrap_or_else(|e| e.into_inner());
        e.listeners.push(tx);
        (e.recent.iter().cloned().collect(), rx)
    }

    /// A container event, with its name, image and labels as attributes.
    pub fn emit_container(&self, action: &str, id: &str, extra: &[(&str, String)]) {
        let Some((image, attrs)) = self.lock().containers.get(id).and_then(|c| container_attrs(c).map(|attrs| (c.spec.image.clone(), attrs))) else { return };
        self.emit_container_with(action, id, &image, attrs, extra);
    }

    fn emit_container_with(&self, action: &str, id: &str, image: &str, mut attrs: serde_json::Map<String, serde_json::Value>, extra: &[(&str, String)]) {
        for (k, v) in extra {
            attrs.insert((*k).to_owned(), v.clone().into());
        }
        self.emit("container", action, id, attrs, Some((id, image)));
    }

    /// An image event, as Docker sends one: `pull`, `tag`, `untag`,
    /// `delete`, `load`. Open windows refresh their image list on it.
    pub fn emit_image(&self, action: &str, id: &str, name: &str) {
        let mut attrs = serde_json::Map::new();
        attrs.insert("name".into(), name.into());
        self.emit("image", action, id, attrs, None);
    }

    /// A volume event: `create` or `destroy`.
    fn emit_volume(&self, action: &str, name: &str) {
        let mut attrs = serde_json::Map::new();
        attrs.insert("driver".into(), "local".into());
        self.emit("volume", action, name, attrs, None);
    }

    fn emit_network(&self, action: &str, net: &super::network::Network, container: &str) {
        let mut attrs = serde_json::Map::new();
        attrs.insert("container".into(), container.into());
        attrs.insert("name".into(), net.name.clone().into());
        attrs.insert("type".into(), net.driver.clone().into());
        self.emit("network", action, &net.id, attrs, None);
    }

    /// Fixed addresses containers not running ask for on a network: an
    /// automatic address passes over them, so they are free when those start.
    fn wanted_addresses(&self, network: &str) -> Vec<[u8; 4]> {
        let st = self.lock();
        let mut out = Vec::new();
        for c in st.containers.values().filter(|c| c.status != Status::Running) {
            if c.spec.network_mode == network {
                out.extend(super::network::parse_ip(&c.spec.ipv4_address));
            }
            out.extend(c.spec.extra_networks.iter().filter(|x| x.name == network).filter_map(|x| super::network::parse_ip(&x.ipv4_address)));
        }
        out
    }

    /// For `--network container:<target>`: the target's id, hostname and
    /// domain name. It has to be running, as with Docker.
    /// Also the network mode the namespace finally belongs to (following
    /// `container:` chains), so the caller can tell a host-network one.
    fn network_container(&self, id: &str, target: &str) -> Result<(String, String, String, String), Error> {
        let st = self.lock();
        let target = resolve(&st, target)?;
        if target == id {
            return Err(Error::BadRequest("cannot join own network".into()));
        }
        let c = st.containers.get(&target).expect("resolved");
        if c.status != Status::Running {
            return Err(Error::Conflict(format!("cannot join network of a non running container: {target}")));
        }
        let hostname = if c.spec.hostname.is_empty() { c.short_id().to_owned() } else { c.spec.hostname.clone() };
        let mut mode = c.spec.network_mode.clone();
        for _ in 0..8 {
            let Some(next) = mode.strip_prefix("container:").and_then(|t| resolve(&st, t).ok()).and_then(|t| st.containers.get(&t)) else { break };
            mode = next.spec.network_mode.clone();
        }
        Ok((target, hostname, c.spec.domainname.clone(), mode))
    }

    /// `--link` lines for /etc/hosts, as Docker writes them: `ip alias
    /// hostname name`. Linked containers must be running; on the default
    /// bridge (which has no DNS) they must be on it too. The lines are fixed
    /// at start: a linked container that restarts with another address is
    /// not followed (Docker rewrites them; restart this one).
    fn link_hosts(&self, id: &str, spec: &Spec, net: &super::network::Network) -> Result<Vec<String>, Error> {
        let st = self.lock();
        let me = st.containers.get(id).map(|c| c.name.clone()).unwrap_or_default();
        let mut out = Vec::new();
        for l in &spec.links {
            let c = st.containers.get(&l.id).ok_or_else(|| Error::NotFound(format!("could not get container for {}", l.name)))?;
            if c.status != Status::Running {
                return Err(Error::Conflict(format!("Cannot link to a non running container: /{} AS /{me}/{}", c.name, l.alias)));
            }
            let ip = if c.spec.network_mode == net.name { c.ip } else { c.extra_ips.iter().find(|(n, _)| *n == net.name).map(|(_, ip)| *ip) };
            let Some(ip) = ip else {
                if net.name == "bridge" {
                    return Err(Error::BadRequest(format!("Cannot link to /{}, as it does not belong to the default network", c.name)));
                }
                continue;
            };
            let hostname = if c.spec.hostname.is_empty() { c.short_id() } else { c.spec.hostname.as_str() };
            // A user network's DNS already answers the container's own names
            // (Compose's `links: [db]`) and follows it across restarts; a
            // fixed line would not.
            if net.name != "bridge" {
                let aliases: &[String] = if c.spec.network_mode == net.name { &c.spec.aliases } else { c.spec.extra_networks.iter().find(|x| x.name == net.name).map_or(&[], |x| &x.aliases) };
                if l.alias == c.name || l.alias == hostname || aliases.contains(&l.alias) {
                    continue;
                }
            }
            let mut names = format!("{} {hostname}", l.alias);
            if l.alias != c.name {
                names.push(' ');
                names.push_str(&c.name);
            }
            out.push(format!("{}\t{names}", super::network::ip_str(ip)));
        }
        Ok(out)
    }

    /// Records a start that never reached the guest as an immediate exit,
    /// with the exit code Docker gives that failure (see `start_exit_code`).
    fn fail_start(&self, id: &str, spec: &Spec, err: &dyn std::fmt::Display) {
        let message = err.to_string();
        let code = start_exit_code(&message);
        let vm = self.link.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(vm) = vm {
            let _ = vm.is_lost();
        }
        let mut st = self.lock();
        // Lost may have run before this start finished allocating addresses.
        // Reclaim those even when preserving the crash's exit accounting.
        if let Some(c) = st.containers.get_mut(id) {
            c.start_error = message;
            if let Some(ip) = c.ip.take() {
                self.release_ip(&c.spec.network_mode, ip);
            }
            for (net, ip) in std::mem::take(&mut c.extra_ips) {
                self.release_ip(&net, ip);
            }
            self.release_ip6s(c);
        }
        if !spec.auto_remove && st.containers.get(id).is_some_and(|c| c.vm_stopped) {
            drop(st);
            self.save();
            return;
        }
        let actor = st.containers.get(id).and_then(|c| container_attrs(c).map(|attrs| (c.spec.image.clone(), attrs)));
        if let Some(c) = st.containers.get_mut(id) {
            c.status = Status::Exited;
            c.exits += 1;
            c.exit_code = code;
            c.finished = Some(SystemTime::now());
            c.attached.clear();
            lock_log(&c.log).end_follow();
        }
        if let Some(h) = st.health.get_mut(id) {
            close_monitor(h);
        }
        // `--rm`: Docker removes a container whose start failed, and an
        // attached `docker run --rm` waits for exactly that (condition
        // "removed") before it reports the error; without it, it hung.
        let removed = if spec.auto_remove { st.containers.remove(id) } else { None };
        if removed.is_some() {
            note_removed(&mut st, id, code);
        }
        drop(st);
        self.changed.notify_all();
        if let Some(c) = removed {
            self.release_image_blobs(&std::iter::once(c.spec.image_id.clone()).chain(c.layers.iter().cloned()).collect::<Vec<_>>());
            if let Some(vm) = self.awake() {
                let _ = vm.remove(id);
            }
            lock_log(&c.log).remove();
            for v in c.spec.anon_volumes {
                self.delete_volume_data(&v);
            }
            if let Some((image, attrs)) = actor {
                self.emit_container_with("destroy", id, &image, attrs, &[]);
            }
        }
        if !spec.auto_remove {
            self.save();
        }
    }

    pub fn lock_net(&self) -> MutexGuard<'_, Networks> {
        self.lock_networks()
    }

    fn lock_networks(&self) -> MutexGuard<'_, Networks> {
        self.networks.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn release_ip(&self, network: &str, ip: [u8; 4]) {
        let mut nets = self.lock_networks();
        let name = nets.find(network).map(|n| n.name.clone());
        if let Some(name) = name {
            nets.release(&name, ip);
        }
    }

    /// Gives back a stopped container's IPv6 addresses.
    fn release_ip6s(&self, c: &mut Container) {
        let mut nets = self.lock_networks();
        if let Some(ip) = c.ip6.take() {
            if let Some(name) = nets.find(&c.spec.network_mode).map(|n| n.name.clone()) {
                nets.release6(&name, ip);
            }
        }
        for (net, ip) in std::mem::take(&mut c.extra_ip6s) {
            nets.release6(&net, ip);
        }
    }

    /// Fixed IPv6 addresses containers not running ask for on a network.
    fn wanted_addresses6(&self, network: &str) -> Vec<[u8; 16]> {
        let st = self.lock();
        let mut out = Vec::new();
        for c in st.containers.values().filter(|c| c.status != Status::Running) {
            if c.spec.network_mode == network {
                out.extend(super::network::parse_ip6(&c.spec.ipv6_address));
            }
            out.extend(c.spec.extra_networks.iter().filter(|x| x.name == network).filter_map(|x| super::network::parse_ip6(&x.ipv6_address)));
        }
        out
    }

    /// The container's IPv6 address on `n` if it has IPv6: the one asked
    /// for (`want`), or the next free one.
    fn allocate_ip6(&self, n: &super::network::Network, want: &str) -> Result<Option<[u8; 16]>, Error> {
        if n.v6.is_none() && want.is_empty() {
            return Ok(None);
        }
        let avoid = self.wanted_addresses6(&n.name);
        let requested = super::network::parse_ip6(want);
        // Automatic pool exhaustion is a server allocation failure in
        // Docker, including a connect to an already-running container.
        self.lock_networks().allocate6(&n.name, requested, &avoid).map_err(|message| {
            if requested.is_none() { Error::Io(io::Error::other(message)) } else { Error::Conflict(message) }
        })
    }

    /// Where `<host>` (a `*.asagiri.localhost` name) should go: the container
    /// and the ports that could be its web port (domains.rs picks one).
    /// Names are the container name, or `<service>.<project>` for Compose
    /// services.
    pub fn route(&self, host: &str) -> Option<super::domains::Target> {
        let host = host.split(':').next()?.to_ascii_lowercase();
        let name = host.strip_suffix(".asagiri.localhost").or_else(|| host.strip_suffix(".asagiri.local"))?.to_owned();
        let st = self.lock();
        let c = st.containers.values().find(|c| {
            c.status == Status::Running && {
                let label = |k: &str| c.spec.labels.get(k).and_then(|v| v.as_str()).unwrap_or("");
                let service = format!("{}.{}", label("com.docker.compose.service"), label("com.docker.compose.project"));
                // `dev.asagiri.domain=api.myapp` (comma-separated for several) picks the names.
                let chosen = label(DOMAIN_LABEL).split(',').map(|d| own_name(d.trim())).any(|d| !d.is_empty() && d.eq_ignore_ascii_case(&name));
                chosen || c.name.eq_ignore_ascii_case(&name) || service.eq_ignore_ascii_case(&name) || (name.len() >= 12 && c.id.starts_with(&name))
            }
        })?;
        let ip = if c.spec.network_mode == "host" { [127, 0, 0, 1] } else { c.ip? };
        Some(super::domains::Target {
            ip,
            id: c.id.clone(),
            label: c.spec.labels.get("dev.asagiri.http.port").and_then(|v| v.as_str()).and_then(|p| p.parse().ok()),
            published: c.spec.ports.iter().filter(|b| b.proto != "udp").map(|b| b.container_port).collect(),
            exposed: c.spec.exposed.clone(),
        })
    }

    /// For the Finder view (macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn fs_stat(&self, id: &str, path: &str) -> io::Result<super::vmlink::FileInfo> {
        self.vm()?.fs_stat(id, path)
    }

    /// For the Finder view (macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn fs_list(&self, id: &str, path: &str) -> io::Result<Vec<super::vmlink::FileInfo>> {
        self.vm()?.fs_list(id, path)
    }

    /// For the Finder view (macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn fs_read(&self, id: &str, path: &str, offset: u64, len: u32) -> io::Result<Vec<u8>> {
        self.vm()?.fs_read(id, path, offset, len)
    }

    /// A stream into the guest relay towards `ip:port`.
    pub fn relay_to(&self, ip: [u8; 4], port: u16) -> io::Result<asagiri_vmm::VsockStream> {
        let vm = self.vm()?;
        super::ports::open_relay(&vm, ip, port)
    }

    /// Opens a `docker cp` session with the guest for a container, running
    /// or not: the request frame is sent, the caller does the rest.
    pub fn copy(&self, key: &str, op: u8, path: &str, flags: u32) -> Result<asagiri_vmm::VsockStream, Error> {
        // Lock order: container lifecycle, short state snapshot, then cleanup
        // wait and frame submission with no state lock. The reader's Exited
        // callback never takes this lifecycle lock. This serializes setup with
        // start/remove; an already-open copy stream keeps its existing lifetime.
        let lifecycle = self.lifecycle(key)?;
        let _copying = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let (id, layers, user, mounts) = {
            let st = self.lock();
            let id = resolve(&st, key)?;
            let (layers, user, mounts) = st.containers.get(&id)
                .map(|c| (c.layers.clone(), c.spec.user.clone(), c.spec.mounts.clone())).unwrap_or_default();
            (id, layers, user, mounts)
        };
        let vm = self.vm()?;
        vm.wait_cleanup(&id)?;
        let stream = vm.connect(asagiri_proto::COPY_PORT)?;
        let mut out = Vec::new();
        asagiri_proto::encode_copy(&mut out, op, &id, path, flags, &layers, &user, &mounts);
        std::io::Write::write_all(&mut std::fs::File::from(stream.fd.try_clone()?), &out)?;
        Ok(stream)
    }

    /// Running containers' domains, for the router's "not found" page.
    pub fn domains(&self) -> Vec<String> {
        let suffix = if super::dns::installed() { "asagiri.local" } else { "asagiri.localhost" };
        let mut v = self.each(|c| {
            let chosen = c.spec.labels.get(DOMAIN_LABEL).and_then(|v| v.as_str()).map(|d| own_name(d.split(',').next().unwrap_or("").trim()).to_owned());
            (c.status == Status::Running).then(|| format!("{}.{suffix}", chosen.filter(|d| !d.is_empty()).unwrap_or_else(|| c.name.clone())))
        });
        v.sort();
        v
    }

    /// Containers attached to a network.
    pub fn network_members(&self, network: &str) -> Vec<Member> {
        let canonical = self.lock_networks().find(network).map(|n| n.name.clone()).unwrap_or_default();
        self.each(|c| {
            let theirs = self.lock_networks().find(&c.spec.network_mode).map(|n| n.name.clone()).unwrap_or_default();
            if theirs == canonical {
                return Some((c.id.clone(), c.name.clone(), c.ip, c.ip6, c.spec.mac_address.clone(), c.status == Status::Running));
            }
            c.spec.extra_networks.iter().find(|x| x.name == canonical).map(|x| {
                let ip = c.extra_ips.iter().find(|(n, _)| *n == canonical).map(|(_, ip)| *ip);
                let ip6 = c.extra_ip6s.iter().find(|(n, _)| *n == canonical).map(|(_, ip)| *ip);
                (c.id.clone(), c.name.clone(), ip, ip6, x.mac_address.clone(), c.status == Status::Running)
            })
        })
    }

    /// `DELETE /networks/{id}`: the network goes, and with it its bridge in
    /// the guest (addresses, routes, DNS, the IPv6 NAT rule), if the VM is up.
    pub fn network_remove(&self, key: &str) -> Result<(), String> {
        let n = self.lock_networks().find(key).cloned();
        self.lock_networks().remove(key)?;
        if let Some(n) = n.filter(|n| !n.bridge.is_empty()) {
            if let Some(vm) = self.awake() {
                let _ = vm.remove_bridge(&n.bridge);
            }
        }
        Ok(())
    }

    /// Subscribes to a container's output from now on. Returns the full id
    /// and whether it runs on a terminal (raw stream, no multiplexing).
    pub fn attach(&self, key: &str) -> Result<(String, bool, Receiver<(Stream, Vec<u8>)>), Error> {
        let mut st = self.lock();
        let id = resolve(&st, key)?;
        let (tx, rx) = mpsc::channel();
        let c = st.containers.get_mut(&id).expect("resolved");
        if c.status != Status::Exited || !c.spec.auto_remove {
            c.attached.push(tx);
        }
        Ok((id, c.spec.tty, rx))
    }

    /// Output so far, plus a live subscription if `follow` and still
    /// running; `None` with `--log-driver none`, which cannot be read.
    pub fn logs(&self, key: &str, follow: bool) -> Result<Option<Logs>, Error> {
        let st = self.lock();
        let id = resolve(&st, key)?;
        let c = st.containers.get(&id).expect("resolved");
        if !c.spec.log_config.readable() {
            return Ok(None);
        }
        let (tty, running) = (c.spec.tty, c.status != Status::Exited);
        // Under the engine's lock, so an exit can't end followers between
        // the status check and joining them.
        let log = c.log.clone();
        let mut log = lock_log(&log);
        drop(st);
        let (tx, live) = if follow && running {
            let (tx, rx) = mpsc::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let past = log.snapshot(tx);
        Ok(Some(Logs { tty, past, live }))
    }

    /// Where a container's log is, for inspect's `LogPath`.
    pub fn log_path(c: &Container) -> String {
        lock_log(&c.log).path()
    }

    pub fn input(&self, id: &str, data: &[u8]) -> io::Result<()> {
        {
            let mut st = self.lock();
            if let Some(c) = st.containers.get_mut(id) {
                if !c.live {
                    if c.status == Status::Exited {
                        return Err(io::Error::new(io::ErrorKind::BrokenPipe, "container has exited"));
                    }
                    c.early_input.extend_from_slice(data);
                    return Ok(());
                }
            }
        }
        self.vm()?.input(id, data)
    }

    pub fn close_stdin(&self, id: &str) -> io::Result<()> {
        {
            let mut st = self.lock();
            if let Some(c) = st.containers.get_mut(id) {
                if !c.live {
                    c.early_close = true;
                    return Ok(());
                }
            }
        }
        self.vm()?.close_stdin(id)
    }

    /// Resizes a container's (or exec's) terminal; remembered for a container
    /// that has not started yet.
    pub fn resize(&self, key: &str, rows: u16, cols: u16) -> Result<(), Error> {
        let mut st = self.lock();
        if st.execs.contains_key(key) {
            drop(st);
            return Ok(self.vm()?.resize(key, rows, cols)?);
        }
        let id = resolve(&st, key)?;
        let c = st.containers.get_mut(&id).expect("resolved");
        c.size = Some((rows, cols));
        let running = c.status == Status::Running;
        drop(st);
        if running {
            self.vm()?.resize(&id, rows, cols)?;
        }
        Ok(())
    }

    /// Blocks until the condition holds. Returns the exit code.
    /// `condition`: `not-running` (default), `next-exit` or `removed`.
    pub fn wait(&self, key: &str, condition: &str) -> Result<i32, Error> {
        self.wait_connected(key, condition, || false).map(|code| code.expect("uncancelled wait"))
    }

    /// A Docker wait ends when its client disconnects, even if the container
    /// keeps running. A bounded condvar wait lets that release its API slot.
    pub fn wait_connected(&self, key: &str, condition: &str, disconnected: impl Fn() -> bool) -> Result<Option<i32>, Error> {
        let mut st = self.lock();
        let id = resolve(&st, key)?;
        let exits_before = st.containers.get(&id).map_or(0, |c| c.exits);
        loop {
            match st.containers.get(&id) {
                None => {
                    let code = st.removed.iter().rev().find(|(r, _)| *r == id).map_or(0, |(_, c)| *c);
                    return Ok(Some(code));
                }
                Some(c) => match condition {
                    "removed" => {}
                    "next-exit" if c.exits > exits_before => return Ok(Some(c.exit_code)),
                    "next-exit" => {}
                    _ if c.status != Status::Running => return Ok(Some(c.exit_code)),
                    _ => {}
                },
            }
            if disconnected() {
                return Ok(None);
            }
            st = self.changed.wait_timeout(st, Duration::from_millis(250)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// SIGTERM, then SIGKILL after `timeout` if it is still running.
    /// Plugs a running container into one more network, as the lowest
    /// `eth{n}` it has free.
    fn attach_extra(&self, vm: &VmLink, id: &str, extra: &ExtraNetwork) -> Result<(), Error> {
        let net = self.lock_networks().find(&extra.name).cloned().ok_or_else(|| Error::NotFound(format!("network {} not found", extra.name)))?;
        if !matches!(net.driver.as_str(), "bridge" | "macvlan" | "ipvlan") {
            return Err(Error::BadRequest(format!("only bridge, macvlan and ipvlan networks can be connected, not {}", net.driver)));
        }
        // The number is taken before the guest is asked. Two networks that
        // join at once would otherwise both see it free, and the guest
        // replaces an interface that already has the name.
        let n = match self.lock().containers.get_mut(id) {
            Some(c) => take_ifnum(&mut c.extra_ifs, &net.name),
            None => return Err(Error::NotFound(format!("No such container: {id}"))),
        };
        self.attach_as(vm, id, n, &net, extra).inspect_err(|_| {
            if let Some(c) = self.lock().containers.get_mut(id) {
                drop_ifnum(&mut c.extra_ifs, &net.name, n);
            }
        })
    }

    /// Gives the container its addresses on `net` and has the guest make
    /// `eth{n}` there.
    fn attach_as(&self, vm: &VmLink, id: &str, n: u32, net: &super::network::Network, extra: &ExtraNetwork) -> Result<(), Error> {
        let ip = match super::network::parse_ip(&extra.ipv4_address) {
            Some(want) => self.lock_networks().reserve(&net.name, want).map(|()| want).map_err(Error::Conflict)?,
            None => {
                let avoid = self.wanted_addresses(&net.name);
                self.lock_networks().allocate_avoiding(&net.name, &avoid).ok_or_else(|| Error::Io(io::Error::other(format!("no free address left on network {}", net.name))))?
            }
        };
        let off6 = self.lock().containers.get(id).is_some_and(|c| c.spec.ipv6_disabled(&format!("eth{n}")));
        let ip6 = match if off6 { Ok(None) } else { self.allocate_ip6(net, &extra.ipv6_address) } {
            Ok(a) => a,
            Err(e) => {
                self.lock_networks().release(&net.name, ip);
                return Err(e);
            }
        };
        let (name, hostname) = self.lock().containers.get(id).map(|c| (c.name.clone(), c.spec.hostname.clone())).unwrap_or_default();
        let names = endpoint_names(&name, &extra.aliases, id, &hostname);
        let mut opts: Vec<String> = if extra.mac_address.is_empty() { Vec::new() } else { vec![format!("{}={}", asagiri_proto::net_opts::MAC, extra.mac_address)] };
        if let (Some(a), Some(v)) = (ip6, &net.v6) {
            opts.push(format!("{}={}/{}", asagiri_proto::net_opts::IP6, super::network::ip6_str(a), v.prefix));
            opts.push(format!("{}={}", asagiri_proto::net_opts::GW6, super::network::ip6_str(v.gateway)));
        }
        if net.internal {
            opts.push(format!("{}=1", asagiri_proto::net_opts::INTERNAL));
        }
        if net.no_ipv4 {
            opts.push(format!("{}=1", asagiri_proto::net_opts::NO_IPV4));
        }
        opts.push(format!("{}={}", asagiri_proto::net_opts::NETWORK, net.name));
        if let Some(mtu) = net.mtu() {
            opts.push(format!("{}={mtu}", asagiri_proto::net_opts::MTU));
        }
        if !net.icc() {
            opts.push(format!("{}=1", asagiri_proto::net_opts::NO_ICC));
        }
        if let Some(mode) = net.macvlan_mode() {
            opts.push(format!("{}={mode}", asagiri_proto::net_opts::MACVLAN));
        }
        if let Some((mode, flag)) = net.ipvlan() {
            opts.push(format!("{}={mode}", asagiri_proto::net_opts::IPVLAN));
            opts.push(format!("{}={flag}", asagiri_proto::net_opts::IPVLAN_FLAG));
        }
        if let Err(e) = vm.net_attach(id, n, &net.bridge, ip, net.prefix, net.gateway, &names, &opts) {
            let mut nets = self.lock_networks();
            nets.release(&net.name, ip);
            if let Some(a) = ip6 {
                nets.release6(&net.name, a);
            }
            return Err(Error::Io(e));
        }
        if let Some(c) = self.lock().containers.get_mut(id) {
            c.extra_ips.push((net.name.clone(), ip));
            if let Some(a) = ip6 {
                c.extra_ip6s.push((net.name.clone(), a));
            }
        }
        Ok(())
    }

    /// `docker network connect`.
    pub fn connect(&self, network: &str, key: &str, aliases: Vec<String>, ipv4_address: String, ipv6_address: String, mac_address: String) -> Result<(), Error> {
        let id = self.resolve(key)?;
        let lifecycle = self.lifecycle(&id)?;
        let _connecting = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let net = self.lock_networks().find(network).cloned().ok_or_else(|| Error::NotFound(format!("network {network} not found")))?;
        if let Some(ip) = super::network::parse_ip(&ipv4_address) {
            net.check_static(ip).map_err(Error::BadRequest)?;
        }
        if let Some(ip) = super::network::parse_ip6(&ipv6_address) {
            net.check_static6(ip).map_err(Error::BadRequest)?;
        }
        // Kept to plug in below: by then another network may have joined, and
        // the last one in the container's list is no longer this one.
        let extra = ExtraNetwork { name: net.name.clone(), aliases, ipv4_address, ipv6_address, mac_address };
        let running = {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).ok_or_else(|| Error::NotFound(format!("No such container: {id}")))?;
            if c.spec.network_mode == net.name || c.spec.extra_networks.iter().any(|x| x.name == net.name) {
                return Err(Error::Conflict(format!("endpoint with name {} already exists in network {}", c.name, net.name)));
            }
            if matches!(c.spec.network_mode.as_str(), "host" | "none") {
                return Err(Error::BadRequest(format!("container cannot be connected to {} while on {}", net.name, c.spec.network_mode)));
            }
            if c.spec.network_mode.starts_with("container:") {
                return Err(Error::BadRequest("container sharing network namespace with another container or host cannot be connected to any other network".into()));
            }
            c.spec.extra_networks.push(extra.clone());
            c.status == Status::Running
        };
        self.save();
        if running {
            let vm = self.vm()?;
            if let Err(e) = self.attach_extra(&vm, &id, &extra) {
                if let Some(c) = self.lock().containers.get_mut(&id) {
                    c.spec.extra_networks.retain(|x| x.name != net.name);
                }
                return Err(e);
            }
        }
        self.emit_network("connect", &net, &id);
        Ok(())
    }

    /// `docker network disconnect`.
    pub fn disconnect(&self, network: &str, key: &str) -> Result<(), Error> {
        let id = self.resolve(key)?;
        let lifecycle = self.lifecycle(&id)?;
        let _disconnecting = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let net = self.lock_networks().find(network).cloned().ok_or_else(|| Error::NotFound(format!("network {network} not found")))?;
        let (n, ip, ip6) = {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).ok_or_else(|| Error::NotFound(format!("No such container: {id}")))?;
            if c.spec.network_mode == net.name {
                return Err(Error::BadRequest(format!("Asagiri cannot disconnect a container from its first network ({})", net.name)));
            }
            let Some(pos) = c.spec.extra_networks.iter().position(|x| x.name == net.name) else {
                return Err(Error::NotFound(format!("container {} is not connected to network {}", &id[..12], net.name)));
            };
            c.spec.extra_networks.remove(pos);
            // The interface it was plugged in as, which is not its place in
            // the list once an earlier network has left. The number stays
            // taken until the guest has let the interface go: a network that
            // joins meanwhile must not be given the same name.
            let n = c.extra_ifs.iter().find(|(x, _)| *x == net.name).map(|(_, n)| *n);
            let ip = c.extra_ips.iter().position(|(x, _)| *x == net.name).map(|i| c.extra_ips.remove(i).1);
            let ip6 = c.extra_ip6s.iter().position(|(x, _)| *x == net.name).map(|i| c.extra_ip6s.remove(i).1);
            (n, ip, ip6)
        };
        self.save();
        let detached = match (n, ip, self.awake()) {
            (Some(n), Some(ip), Some(vm)) => vm.net_detach(&id, n, &net.bridge, ip),
            _ => Ok(()),
        };
        // The network has left the container whether or not the guest could
        // be told: its number and addresses are free again either way.
        if let (Some(n), Some(c)) = (n, self.lock().containers.get_mut(&id)) {
            drop_ifnum(&mut c.extra_ifs, &net.name, n);
        }
        if let Some(ip) = ip {
            self.release_ip(&net.name, ip);
        }
        if let Some(a) = ip6 {
            self.lock_networks().release6(&net.name, a);
        }
        detached?;
        self.emit_network("disconnect", &net, &id);
        Ok(())
    }

    /// `docker pause` / `docker unpause`.
    pub fn pause(&self, key: &str, paused: bool) -> Result<(), Error> {
        let id = self.resolve(key)?;
        {
            let st = self.lock();
            let c = st.containers.get(&id).expect("resolved");
            if c.status != Status::Running {
                return Err(Error::Conflict(format!("Container {} is not running", &id[..12])));
            }
            if c.paused == paused {
                return Err(Error::Conflict(format!("Container {} is {}paused", &id[..12], if paused { "already " } else { "not " })));
            }
        }
        self.vm()?.freeze(&id, paused)?;
        if let Some(c) = self.lock().containers.get_mut(&id) {
            c.paused = paused;
        }
        // Docker probes only running, unpaused containers.
        if paused {
            if let Some(h) = self.lock().health.get_mut(&id) {
                close_monitor(h);
            }
            self.changed.notify_all();
        } else {
            self.health_resume(&id);
        }
        self.emit_container(if paused { "pause" } else { "unpause" }, &id, &[]);
        Ok(())
    }

    /// `docker update`: new limits now, and at every later start.
    pub fn update(&self, key: &str, limits: Limits, restart: Option<RestartPolicy>) -> Result<(), Error> {
        let id = self.resolve(key)?;
        limits.validate()?;
        let running = self.with(&id, |c| c.status == Status::Running)?;
        if running { self.vm()?.update(&id, limits.wire())?; }
        {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).ok_or_else(|| Error::NotFound(format!("No such container: {id}")))?;
            c.spec.limits = limits;
            if let Some(r) = restart { c.spec.restart = r; }
        }
        self.save();
        self.emit_container("update", &id, &[]);
        Ok(())
    }

    /// One `docker stats` sample of a running container.
    pub fn stats(&self, key: &str) -> Result<(String, String, asagiri_proto::ContainerStats), Error> {
        let id = self.resolve(key)?;
        let name = self.lock().containers.get(&id).map(|c| c.name.clone()).unwrap_or_default();
        if !self.is_running(&id) {
            return Err(Error::Conflict(format!("Container {} is not running", &id[..12])));
        }
        Ok((id.clone(), name, self.vm()?.container_stats(&id)?))
    }

    /// `docker top`.
    pub fn top(&self, key: &str) -> Result<Vec<String>, Error> {
        let id = self.resolve(key)?;
        if !self.is_running(&id) {
            return Err(Error::Conflict(format!("Container {} is not running", &id[..12])));
        }
        Ok(self.vm()?.top(&id)?)
    }

    /// Asagiri's own: the TCP ports a running container listens on, split into
    /// those a published port would reach and those it would not.
    pub fn listening(&self, key: &str) -> Result<(Vec<u16>, Vec<u16>), Error> {
        let id = self.resolve(key)?;
        if !self.is_running(&id) {
            return Err(Error::Conflict(format!("Container {} is not running", &id[..12])));
        }
        let mode = self.lock().containers.get(&id).map(|c| c.spec.network_mode.clone()).unwrap_or_default();
        let lines = self.vm()?.listening(&id)?;
        let (reach, local) = super::ports::listening(&lines, mode == "host");
        // With no network at all (or another container's), nothing can be published.
        if mode == "none" || mode.starts_with("container:") {
            return Ok((Vec::new(), [reach, local].concat()));
        }
        Ok((reach, local))
    }

    /// `docker rename`.
    /// Asagiri's own: new published ports for a container. A running one keeps
    /// running: its ports are relays on the Mac, so only they are replaced.
    /// If the new ones cannot be bound, the old ones are put back.
    pub fn set_ports(&self, key: &str, ports: Vec<super::ports::Binding>) -> Result<(), Error> {
        let id = self.resolve(key)?;
        let mode = self.lock().containers.get(&id).map(|c| c.spec.network_mode.clone()).unwrap_or_default();
        if let Some(driver) = self.lock_networks().find(&mode).filter(|n| n.own_link() && !ports.is_empty()).map(|n| n.driver.clone()) {
            return Err(Error::BadRequest(format!("a container on a {driver} network cannot publish ports")));
        }
        let (old_ports, old_fw, running, target) = {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).expect("resolved");
            if c.spec.network_mode == "none" && !ports.is_empty() {
                return Err(Error::BadRequest("a container with --network none cannot publish ports".into()));
            }
            if c.spec.network_mode.starts_with("container:") && !ports.is_empty() {
                return Err(Error::BadRequest("conflicting options: port publishing and the container type network mode".into()));
            }
            let target = if c.spec.network_mode == "host" { Some([127, 0, 0, 1]) } else { c.ip };
            let old = std::mem::replace(&mut c.spec.ports, ports.clone());
            (old, c.forwards.take(), c.status == Status::Running, target)
        };
        // Closing listeners joins their threads: never under the lock.
        drop(old_fw);
        if running && !ports.is_empty() {
            let vm = self.vm()?;
            let target = target.unwrap_or([127, 0, 0, 1]);
            match super::ports::start(vm.clone(), &id, &ports, target) {
                Ok(fw) => {
                    if let Some(c) = self.lock().containers.get_mut(&id) {
                        c.forwards = Some(fw);
                    }
                }
                Err(e) => {
                    crate::diag::warn!("new ports not published; the old ones are kept", id = &id[..12], error = e);
                    let back = super::ports::start(vm, &id, &old_ports, target).ok();
                    if let Some(c) = self.lock().containers.get_mut(&id) {
                        c.spec.ports = old_ports;
                        c.forwards = back;
                    }
                    return Err(Error::Conflict(e.to_string()));
                }
            }
        }
        self.save();
        self.emit_container("update", &id, &[]);
        Ok(())
    }

    /// Asagiri's own: a container's environment. Takes effect at its next start;
    /// says whether it is running now, so the caller can restart it.
    pub fn set_env(&self, key: &str, env: Vec<String>) -> Result<bool, Error> {
        let id = self.resolve(key)?;
        let running = {
            let mut st = self.lock();
            let c = st.containers.get_mut(&id).expect("resolved");
            c.spec.env = env;
            c.status == Status::Running
        };
        self.save();
        self.emit_container("update", &id, &[]);
        Ok(running)
    }

    pub fn rename(&self, key: &str, new: &str) -> Result<(), Error> {
        let new = new.trim_start_matches('/');
        if !valid_container_name(new) {
            return Err(Error::BadRequest(format!("Invalid container name ({new}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed")));
        }
        let id = self.resolve(key)?;
        let lifecycle = self.lifecycle(&id)?;
        let _renaming = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let updates = {
            let mut st = self.lock();
            if st.containers.values().any(|c| c.name == new && c.id != id) {
                return Err(Error::Conflict(format!("Conflict. The container name \"/{new}\" is already in use by another container.")));
            }
            let c = st.containers.get_mut(&id).ok_or_else(|| Error::NotFound(format!("No such container: {id}")))?;
            if c.name == new {
                return Err(Error::Conflict(format!("Renaming a container with the same name as its current name")));
            }
            c.name = new.to_owned();
            let networks = self.lock_networks();
            let primary = c.ip.map(|ip| (c.spec.network_mode.as_str(), ip, c.spec.aliases.as_slice()));
            let extra = c.spec.extra_networks.iter().filter_map(|endpoint| {
                let (_, ip) = c.extra_ips.iter().find(|(name, _)| *name == endpoint.name)?;
                Some((endpoint.name.as_str(), *ip, endpoint.aliases.as_slice()))
            });
            primary.into_iter().chain(extra).filter_map(|(network, ip, aliases)| {
                let net = networks.find(network).filter(|net| !net.builtin())?;
                Some((net.bridge.clone(), ip, endpoint_names(new, aliases, &id, &c.spec.hostname)))
            }).collect::<Vec<_>>()
        };
        self.save();
        if let Some(vm) = self.awake() {
            for (bridge, ip, names) in updates {
                if let Err(error) = vm.dns_rename(&id, &bridge, ip, &names) {
                    crate::diag::warn!("container DNS rename could not reach the guest", id = &id[..12], error = error.to_string());
                    break;
                }
            }
        }
        self.emit_container("rename", &id, &[]);
        Ok(())
    }

    /// Pauses an idle BuildKit container, and resumes it at once if an exec
    /// started in it meanwhile: exec_start marks its exec running before it
    /// looks for the pause, so one of the two sees the other.
    pub fn pause_idle_builder(&self, id: &str) -> Result<(), Error> {
        self.pause(id, true)?;
        if self.running_execs(id) > 0 {
            self.pause(id, false)?;
            return Err(Error::Conflict("an exec started in it".into()));
        }
        Ok(())
    }

    /// Whether a container is frozen.
    pub fn is_paused(&self, id: &str) -> bool {
        self.lock().containers.get(id).is_some_and(|c| c.paused)
    }

    /// `docker stop`. False when there was nothing to stop: the container
    /// was not running and its restart policy was not about to start it.
    pub fn stop(&self, key: &str, timeout: Duration) -> Result<bool, Error> {
        let lifecycle = self.lifecycle(key)?;
        let mut stopping = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let id = self.resolve(key)?;
        *stopping = stopping.wrapping_add(1);
        self.thaw_for_signal(&id);
        let cancelled = self.mark_stopped(&id);
        if !self.wait_started(&id) {
            return Ok(cancelled);
        }
        // A start accepted since the mark above took it away again, and the
        // restart policy would bring back what this stop is about to end.
        self.mark_stopped(&id);
        self.emit_container("kill", &id, &[("signal", (libc::SIGTERM).to_string())]);
        // The run being stopped: a start that follows its exit is another run, not this one still going.
        let (sig, run) = {
            let st = self.lock();
            let c = st.containers.get(&id);
            (c.and_then(|c| c.spec.stop_signal).unwrap_or(libc::SIGTERM as u32), c.map_or(0, |c| c.exits))
        };
        let vm = self.awake();
        if let Some(vm) = &vm {
            vm.kill(&id, sig)?;
        }
        let deadline = std::time::Instant::now() + timeout;
        let mut st = self.lock();
        while st.containers.get(&id).is_some_and(|c| c.status == Status::Running && c.exits == run) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                drop(st);
                if let Some(vm) = &vm {
                    vm.kill(&id, libc::SIGKILL as u32)?;
                }
                // Docker's stop response means the process has exited, even
                // with t=0. Wait for its exit (or auto-removal) to be recorded,
                // as long as `kill` does: the exit may never be reported.
                let killed = std::time::Instant::now() + Duration::from_secs(10);
                let mut exited = self.lock();
                while exited.containers.get(&id).is_some_and(|c| c.status == Status::Running && c.exits == run) {
                    let left = killed.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return Err(Error::Io(io::Error::other(format!("cannot stop container: {}: it did not exit after SIGKILL", &id[..12]))));
                    }
                    exited = self.changed.wait_timeout(exited, left).unwrap_or_else(|e| e.into_inner()).0;
                }
                drop(exited);
                self.emit_container("stop", &id, &[]);
                return Ok(true);
            }
            st = self.changed.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        drop(st);
        self.emit_container("stop", &id, &[]);
        Ok(true)
    }

    /// A frozen process cannot handle a signal; Docker thaws it first.
    fn thaw_for_signal(&self, id: &str) {
        if self.is_paused(id) {
            if let Some(vm) = self.awake() {
                if vm.freeze(id, false).is_ok() {
                    if let Some(c) = self.lock().containers.get_mut(id) {
                        c.paused = false;
                    }
                    self.health_resume(id);
                }
            }
        }
    }

    /// Waits for a start that is under way to finish, and says whether the
    /// container is running then. Until the guest has the process there is
    /// nothing to signal: a stop or kill sent now would be dropped, and
    /// would report success while the container kept running. A start that
    /// never finishes is not waited for forever.
    ///
    /// The answer is the one to act on. Asking whether the container is
    /// running in a second look would let a start that is accepted between
    /// the two through, with the guest still without its process.
    fn wait_started(&self, id: &str) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut st = self.lock();
        while st.containers.get(id).is_some_and(|c| c.status == Status::Running && !c.in_guest) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            st = self.changed.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        st.containers.get(id).is_some_and(|c| c.status == Status::Running)
    }

    pub fn kill(&self, key: &str, signal: u32) -> Result<(), Error> {
        let lifecycle = self.lifecycle(key)?;
        let mut killing = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let id = self.resolve(key)?;
        self.thaw_for_signal(&id);
        if !self.wait_started(&id) {
            if (signal == libc::SIGKILL as u32 || signal == libc::SIGTERM as u32)
                && self.with(&id, recover_after_vm_loss)?
            {
                *killing = killing.wrapping_add(1);
                self.mark_stopped(&id);
                self.save();
                return Ok(());
            }
            return Err(Error::Conflict(format!("Container {} is not running", &id[..12])));
        }
        if signal == libc::SIGKILL as u32 || signal == libc::SIGTERM as u32 {
            *killing = killing.wrapping_add(1);
            self.mark_stopped(&id);
        }
        self.emit_container("kill", &id, &[("signal", signal.to_string())]);
        if let Some(vm) = self.awake() {
            vm.kill(&id, signal)?;
        }
        // Like Docker: a SIGKILL returns only once the exit is recorded, so
        // an immediate inspect, start or rm sees the container stopped.
        if signal == libc::SIGKILL as u32 {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut st = self.lock();
            while st.containers.get(&id).is_some_and(|c| c.status == Status::Running) {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                st = self.changed.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
            }
        }
        Ok(())
    }

    pub fn remove(&self, key: &str, force: bool, with_volumes: bool) -> Result<(), Error> {
        let lifecycle = self.lifecycle(key)?;
        let mut removing = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let id = self.resolve(key)?;
        if self.is_running(&id) {
            if !force {
                return Err(Error::Conflict(format!(
                    "cannot remove container \"{}\": container is running: stop the container before removing or force remove",
                    &id[..12]
                )));
            }
            self.wait_started(&id);
            self.mark_stopped(&id);
            let vm = self.awake();
            if let Some(vm) = &vm {
                vm.kill(&id, libc::SIGKILL as u32)?;
            }
            // The guest drops a kill for a process it does not have yet, as
            // when the start was accepted a moment ago: send it again for as
            // long as the container runs.
            const AGAIN: Duration = Duration::from_millis(200);
            let mut sent = std::time::Instant::now();
            let mut st = self.lock();
            while st.containers.get(&id).is_some_and(|c| c.status == Status::Running) {
                if sent.elapsed() >= AGAIN {
                    drop(st);
                    if let Some(vm) = &vm {
                        vm.kill(&id, libc::SIGKILL as u32)?;
                    }
                    sent = std::time::Instant::now();
                    st = self.lock();
                    continue;
                }
                st = self.changed.wait_timeout(st, AGAIN).unwrap_or_else(|e| e.into_inner()).0;
            }
        }
        *removing = removing.wrapping_add(1);
        // Keep the stopped container addressable if cleanup times out. The
        // lifecycle guard excludes a new start while the guest finishes, and
        // a failed removal can be retried without losing its writable layer.
        // Copies can create an upper before the first start. A replacement
        // VM also retains layers for crash-stopped containers. Ask any awake
        // VM to remove them; an asleep or lost VM defers this to retain().
        // Never boot a replacement or wait for a retiring VM just for rm.
        if let Some(vm) = self.awake() && let Err(error) = vm.remove(&id) {
            if !vm.is_lost() {
                return Err(error.into());
            }
            crate::diag::warn!("writable layer left for the next boot", id = id, error = error);
        }
        let mut st = self.lock();
        // The lifecycle guard excludes a new start until removal is complete.
        let removed = st.containers.remove(&id);
        // A container that removed itself when it exited has its code noted
        // already: a second note, without one, would have a wait report 0.
        if let Some(c) = &removed {
            note_removed(&mut st, &id, c.exit_code);
        }
        st.execs.retain(|_, e| e.container_id != id);
        drop(st);
        if let Some(c) = &removed && let Some(attrs) = container_attrs(c) {
            self.emit_container_with("destroy", &id, &c.spec.image, attrs, &[]);
        }
        self.changed.notify_all();
        let anon = removed.as_ref().map(|c| c.spec.anon_volumes.clone()).unwrap_or_default();
        if let Some(c) = &removed {
            lock_log(&c.log).remove();
            self.release_image_blobs(&std::iter::once(c.spec.image_id.clone()).chain(c.layers.iter().cloned()).collect::<Vec<_>>());
        }
        if removed.as_ref().is_some_and(|c| !c.spec.auto_remove) {
            self.save();
        }
        if with_volumes {
            for v in anon {
                self.delete_volume_data(&v);
            }
        }
        Ok(())
    }

    fn lock_volumes(&self) -> MutexGuard<'_, super::volumes::Volumes> {
        self.volumes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records a volume (creating it if needed). Its directory appears in
    /// the guest on first use.
    pub fn volume_ensure(&self, name: &str, labels: serde_json::Value, anonymous: bool) -> Result<super::volumes::Volume, Error> {
        let new = self.volume_get(name).is_none();
        let v = self.lock_volumes().ensure(name, labels, anonymous).map_err(Error::BadRequest)?;
        if new {
            self.emit_volume("create", name);
        }
        Ok(v)
    }

    /// [`Engine::volume_ensure`] with the local driver's options, which a
    /// new volume keeps (an existing one keeps its own).
    pub fn volume_ensure_with(&self, name: &str, labels: serde_json::Value, anonymous: bool, options: std::collections::BTreeMap<String, String>) -> Result<super::volumes::Volume, Error> {
        let new = self.volume_get(name).is_none();
        let v = self.lock_volumes().ensure_with(name, labels, anonymous, options).map_err(Error::BadRequest)?;
        if new {
            self.emit_volume("create", name);
        }
        Ok(v)
    }

    /// Containers using a volume, by name.
    pub fn volume_users(&self, name: &str) -> Vec<String> {
        self.each(|c| (c.spec.volumes.iter().chain(&c.spec.anon_volumes).any(|v| v == name)).then(|| c.name.clone()))
    }

    pub fn volume_list(&self) -> Vec<super::volumes::Volume> {
        self.lock_volumes().all().to_vec()
    }

    pub fn volume_get(&self, name: &str) -> Option<super::volumes::Volume> {
        self.lock_volumes().get(name).cloned()
    }

    pub fn volume_remove(&self, name: &str, force: bool) -> Result<(), Error> {
        if self.volume_get(name).is_none() {
            return Err(Error::NotFound(format!("get {name}: no such volume")));
        }
        let users = self.volume_users(name);
        if !users.is_empty() && !force {
            return Err(Error::Conflict(format!("remove {name}: volume is in use - [{}]", users.join(", "))));
        }
        self.delete_volume_data(name);
        Ok(())
    }

    /// Forgets a volume and deletes its data in the guest.
    fn delete_volume_data(&self, name: &str) {
        if self.lock_volumes().forget(name) {
            if let Ok(vm) = self.vm() {
                let _ = vm.remove_volume(name);
            }
            self.emit_volume("destroy", name);
        }
    }

    /// `docker exec` step one: record what to run.
    pub fn exec_create(&self, key: &str, config: ExecConfig) -> Result<String, Error> {
        let mut st = self.lock();
        let container_id = resolve(&st, key)?;
        if st.containers.get(&container_id).is_none_or(|c| c.status != Status::Running) {
            return Err(Error::Conflict(format!("container {} is not running", &container_id[..12])));
        }
        // A paused BuildKit container is only idle: the exec resumes it. If the
        // idle pause comes back before the exec starts, exec_start resumes it.
        let builder = st.containers.get(&container_id).is_some_and(|c| c.paused && is_builder(c));
        if builder {
            drop(st);
            match self.pause(&container_id, false) {
                Ok(()) | Err(Error::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
            st = self.lock();
        }
        if !builder && st.containers.get(&container_id).is_some_and(|c| c.paused) {
            return Err(Error::Conflict(format!("Container {} is paused, unpause the container before exec", &container_id[..12])));
        }
        if config.argv.is_empty() {
            return Err(Error::BadRequest("no exec command specified".into()));
        }
        let id = random_hex(32);
        let cmd = config.argv.join(" ");
        let internal = config.internal;
        st.execs.insert(id.clone(), Exec { id: id.clone(), container_id: container_id.clone(), config, running: false, exit_code: None, pid: 0, attached: Vec::new() });
        drop(st);
        if !internal {
            self.emit_container(&format!("exec_create: {cmd}"), &container_id, &[("execID", id.clone())]);
        }
        Ok(id)
    }

    /// `docker exec` step two: start it. Output arrives on the receiver
    /// until the process exits.
    pub fn exec_start(&self, exec_id: &str) -> Result<(bool, Receiver<(Stream, Vec<u8>)>), Error> {
        let (tx, rx) = mpsc::channel();
        let exec_security;
        let (container_id, config, env) = {
            let mut st = self.lock();
            // Execs get the container's ulimits, groups and seccomp profile,
            // as with runc. Without them the exec is not run: it would be
            // under the default profile and not the container's.
            let of = st.execs.get(exec_id).and_then(|e| st.containers.get(&e.container_id));
            let opts = of.map(|c| c.spec.guest_opts(false)).transpose()?.unwrap_or_default();
            let e = st.execs.get_mut(exec_id).ok_or_else(|| Error::NotFound(format!("No such exec instance: {exec_id}")))?;
            if e.running || e.exit_code.is_some() {
                return Err(Error::Conflict("exec has already run".into()));
            }
            e.running = true;
            // Subscribe before the process exists, so no output is missed.
            e.attached.push(tx);
            let (container_id, config) = (e.container_id.clone(), e.config.clone());
            let (security, caps) = st.containers.get(&container_id).map(|c| (c.spec.security_flags() & !run_flags::READONLY_ROOTFS, c.spec.caps)).unwrap_or_default();
            exec_security = (security, caps, opts);
            let mut env = st.containers.get(&container_id).map(|c| c.spec.env.clone()).unwrap_or_default();
            for e in &config.env {
                let key = e.split('=').next().unwrap_or_default();
                env.retain(|x| x.split('=').next() != Some(key));
                env.push(e.clone());
            }
            (container_id, config, env)
        };
        // Paused by the idle watch since the exec was made: started frozen, it
        // would hang. Marked running first, so pause_idle_builder sees it if
        // it pauses after this look.
        if self.lock().containers.get(&container_id).is_some_and(|c| c.paused && is_builder(c))
            && let Err(e) = self.pause(&container_id, false)
            && !matches!(e, Error::Conflict(_))
        {
            crate::diag::warn!("BuildKit container not resumed for an exec", error = format!("{e:?}"));
        }
        let mut flags = 0;
        if config.tty {
            flags |= run_flags::TTY;
        }
        if config.stdin {
            flags |= run_flags::STDIN;
        }
        if config.privileged {
            flags |= run_flags::PRIVILEGED;
        }
        flags |= exec_security.0;
        let console = config.console_size;
        let started = self.vm()?.exec(&container_id, exec_id, &config.argv, &env, &config.cwd, &config.user, flags, exec_security.1, &exec_security.2);
        if let Ok(pid) = started {
            if let Some(x) = self.lock().execs.get_mut(exec_id).filter(|x| x.exit_code.is_none()) {
                x.pid = pid;
            }
        }
        if let Err(e) = started {
            let mut st = self.lock();
            if let Some(x) = st.execs.get_mut(exec_id) {
                x.running = false;
                x.exit_code = Some(126);
                x.attached.clear();
            }
            drop(st);
            self.changed.notify_all();
            return Err(Error::Io(e));
        }
        if let (true, Some((rows, cols))) = (config.tty, console) {
            let _ = self.vm()?.resize(exec_id, rows, cols);
        }
        if !config.internal {
            self.emit_container(&format!("exec_start: {}", config.argv.join(" ")), &container_id, &[("execID", exec_id.to_owned())]);
        }
        Ok((config.tty, rx))
    }

    /// Sets an exec's initial terminal size, given at start.
    pub fn exec_console(&self, exec_id: &str, size: (u16, u16)) {
        if let Some(e) = self.lock().execs.get_mut(exec_id) {
            e.config.console_size = Some(size);
        }
    }

    pub fn with_exec<T>(&self, exec_id: &str, f: impl FnOnce(&Exec) -> T) -> Result<T, Error> {
        let st = self.lock();
        st.execs.get(exec_id).map(f).ok_or_else(|| Error::NotFound(format!("No such exec instance: {exec_id}")))
    }

    /// Execs running in a container now (buildx's `buildctl dial-stdio`
    /// while it builds, for one): the managed builder isn't idle while any are.
    pub fn running_execs(&self, container_id: &str) -> usize {
        self.lock().execs.values().filter(|e| e.container_id == container_id && e.running).count()
    }

    /// Drops a finished exec's record: Asagiri's own, which nobody inspects.
    pub fn forget_exec(&self, exec_id: &str) {
        self.lock().execs.remove(exec_id);
    }

    fn is_running(&self, id: &str) -> bool {
        self.lock().containers.get(id).is_some_and(|c| c.status == Status::Running)
    }

    /// Records a stop asked for by the user. True when that cancelled a
    /// restart its policy had pending.
    fn mark_stopped(&self, id: &str) -> bool {
        let (persist, cancelled) = {
            let mut st = self.lock();
            match st.containers.get_mut(id) {
                Some(c) => {
                    let cancelled = c.restart_pending || recover_after_vm_loss(c);
                    c.stopped_by_user = true;
                    c.restart_pending = false;
                    (!c.spec.auto_remove, cancelled)
                }
                None => (false, false),
            }
        };
        self.changed.notify_all();
        if persist {
            self.save();
        }
        cancelled
    }

    /// Runs `f` over every container, under the lock.
    pub fn each<T>(&self, mut f: impl FnMut(&Container) -> Option<T>) -> Vec<T> {
        self.lock().containers.values().filter_map(&mut f).collect()
    }

    pub fn with<T>(&self, key: &str, f: impl FnOnce(&Container) -> T) -> Result<T, Error> {
        let st = self.lock();
        let id = resolve(&st, key)?;
        Ok(f(st.containers.get(&id).expect("resolved")))
    }

    /// A start reserves Running before the guest returns its PID. Inspect,
    /// like stop and remove, waits for that lifecycle operation to finish.
    /// The caller has already resolved a public name or prefix to this ID.
    /// Container and health state come from the same post-wait state guard.
    pub fn inspect<T>(&self, id: &str, f: impl FnOnce(&Container, Option<&super::health::Health>) -> T) -> Result<T, Error> {
        let lifecycle = {
            let st = self.lock();
            st.containers.get(id).ok_or_else(|| Error::NotFound(format!("No such container: {id}")))?.lifecycle.clone()
        };
        let _inspecting = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        // A name, including this full ID, may have been reused while we waited.
        let st = self.lock();
        let c = st.containers.get(id).ok_or_else(|| Error::NotFound(format!("No such container: {id}")))?;
        Ok(f(c, st.health.get(id).map(|h| &h.health)))
    }

    pub fn guest_memory(&self) -> Option<(u64, u64)> {
        self.awake()?.stats().ok().map(|s| (s.mem_total_kib * 1024, s.mem_available_kib * 1024))
    }

    /// Resident memory of the guest agent, in bytes.
    pub fn agent_rss(&self) -> Option<u64> {
        self.awake()?.stats().ok().map(|s| s.agent_rss_kib * 1024)
    }

    /// Stops everything for a daemon shutdown, as Docker does: each running
    /// container gets its stop signal and its own stop timeout, then
    /// SIGKILL. They are not stopped by the user, so restart policies bring
    /// them back when the daemon next starts.
    pub fn shutdown(&self) {
        crate::diag::info!("VM stopping for shutdown");
        SHUTTING_DOWN.store(true, std::sync::atomic::Ordering::SeqCst);
        self.save();
        for log in self.each(|c| Some(c.log.clone())) {
            lock_log(&log).flush();
        }
        let ids: Vec<String> = self.each(|c| (c.status == Status::Running).then(|| c.id.clone()));
        if let Some(vm) = self.awake() {
            self.stop_together(&vm, &ids, Stopping::Shutdown);
        }
        if let Some(vm) = self.awake() {
            vm.shutdown();
            if !vm.wait_stopped(Duration::from_secs(10)) {
                crate::diag::warn!("the VM did not power off in 10 s; stopped it");
            }
        }
    }
}

/// The lowest `n` for which a running container has no `eth{n}` yet, given
/// its extra networks as (network, n). `eth0` is its first network.
fn free_ifnum(taken: &[(String, u32)]) -> u32 {
    (1..).find(|n| taken.iter().all(|(_, t)| t != n)).expect("fewer networks than numbers")
}

/// Takes the lowest free interface number for `network`.
fn take_ifnum(taken: &mut Vec<(String, u32)>, network: &str) -> u32 {
    let n = free_ifnum(taken);
    taken.push((network.to_owned(), n));
    n
}

/// Gives back the number `network` took. By number as well as by name: the
/// network may have joined again, under another number, since.
fn drop_ifnum(taken: &mut Vec<(String, u32)>, network: &str, n: u32) {
    taken.retain(|(x, t)| !(x == network && *t == n));
}

/// Keep explicit host mappings and links in order. A mapped host name must
/// not also resolve to a built-in address, including with different casing.
fn hosts_lines(extra: &[String], links: Vec<String>) -> Vec<String> {
    let mut hosts = extra.to_vec();
    hosts.extend(links);
    let mapped = |name: &str| hosts.iter().flat_map(|text| text.lines()).any(|line| line.split('#').next().unwrap_or("").split_whitespace().skip(1).any(|alias| alias.eq_ignore_ascii_case(name)));
    let docker = !mapped("host.docker.internal");
    let asagiri = !mapped("host.asagiri.internal");
    let gateway = !mapped("gateway.docker.internal");
    let names = match (docker, asagiri) {
        (true, true) => Some("host.docker.internal host.asagiri.internal"),
        (true, false) => Some("host.docker.internal"),
        (false, true) => Some("host.asagiri.internal"),
        (false, false) => None,
    };
    if let Some(names) = names {
        hosts.push(format!("{}\t{names}", super::ports::HOST_ADDR));
    }
    if gateway {
        hosts.push(format!("{}\tgateway.docker.internal", super::ports::HOST_ADDR));
    }
    hosts
}

#[cfg(test)]
mod hosts_tests {
    use super::hosts_lines;

    fn lines(input: &[&str]) -> Vec<String> {
        input.iter().map(|line| (*line).to_owned()).collect()
    }

    fn addresses<'a>(hosts: &'a [String], name: &str) -> Vec<&'a str> {
        hosts.iter().flat_map(|text| text.lines()).filter_map(|line| {
            let mut fields = line.split('#').next()?.split_whitespace();
            let address = fields.next()?;
            fields.any(|alias| alias.eq_ignore_ascii_case(name)).then_some(address)
        }).collect()
    }

    #[test]
    fn defaults_and_unrelated_mappings_keep_their_order() {
        let hosts = hosts_lines(&lines(&["203.0.113.1\tcustom"]), lines(&["203.0.113.2\tpeer"]));
        assert_eq!(&hosts[..2], &lines(&["203.0.113.1\tcustom", "203.0.113.2\tpeer"]));
        for name in ["host.docker.internal", "host.asagiri.internal", "gateway.docker.internal"] {
            assert_eq!(addresses(&hosts, name), [super::super::ports::HOST_ADDR]);
        }
        assert_eq!(hosts.len(), 4);
        assert_eq!(hosts_lines(&[], Vec::new()), hosts[2..]);
    }

    #[test]
    fn an_ipv4_override_keeps_the_other_default_aliases() {
        let hosts = hosts_lines(&lines(&["203.0.113.19\thost.docker.internal"]), Vec::new());
        assert_eq!(addresses(&hosts, "host.docker.internal"), ["203.0.113.19"]);
        assert_eq!(addresses(&hosts, "host.asagiri.internal"), [super::super::ports::HOST_ADDR]);
        assert_eq!(addresses(&hosts, "gateway.docker.internal"), [super::super::ports::HOST_ADDR]);
    }

    #[test]
    fn uppercase_ipv6_overrides_do_not_get_an_ipv4_fallback() {
        let hosts = hosts_lines(&lines(&["2001:db8::19\tHOST.DOCKER.INTERNAL"]), Vec::new());
        assert_eq!(addresses(&hosts, "host.docker.internal"), ["2001:db8::19"]);
    }

    #[test]
    fn explicit_aliases_can_replace_every_builtin() {
        let extra = lines(&["203.0.113.19 host.docker.internal HOST.ASAGIRI.INTERNAL", "203.0.113.20\tgateway.docker.internal"]);
        assert_eq!(hosts_lines(&extra, Vec::new()), extra);
    }

    #[test]
    fn repeated_user_addresses_are_preserved_without_an_extra_fallback() {
        let extra = lines(&["203.0.113.19\thost.docker.internal", "203.0.113.20\thost.docker.internal"]);
        let hosts = hosts_lines(&extra, Vec::new());
        assert_eq!(&hosts[..2], &extra);
        assert_eq!(addresses(&hosts, "host.docker.internal"), ["203.0.113.19", "203.0.113.20"]);
    }

    #[test]
    fn links_can_override_a_builtin_alias() {
        let hosts = hosts_lines(&[], lines(&["203.0.113.21\tpeer host.asagiri.internal"]));
        assert_eq!(addresses(&hosts, "host.asagiri.internal"), ["203.0.113.21"]);
        assert_eq!(addresses(&hosts, "host.docker.internal"), [super::super::ports::HOST_ADDR]);
    }

    #[test]
    fn an_expanded_host_gateway_is_not_duplicated() {
        let extra = vec![format!("{}\thost.docker.internal", super::super::ports::HOST_ADDR)];
        let hosts = hosts_lines(&extra, Vec::new());
        assert_eq!(addresses(&hosts, "host.docker.internal"), [super::super::ports::HOST_ADDR]);
    }

    #[test]
    fn names_in_comments_do_not_override_defaults() {
        let extra = lines(&["203.0.113.19\tunrelated # host.docker.internal host.asagiri.internal gateway.docker.internal"]);
        let hosts = hosts_lines(&extra, Vec::new());
        assert_eq!(hosts, vec![
            extra[0].clone(),
            format!("{}\thost.docker.internal host.asagiri.internal", super::super::ports::HOST_ADDR),
            format!("{}\tgateway.docker.internal", super::super::ports::HOST_ADDR),
        ]);
    }

    #[test]
    fn a_mapping_after_a_comment_line_keeps_its_own_address() {
        let extra = lines(&["203.0.113.19\tunrelated # comment\n203.0.113.20\tHOST.DOCKER.INTERNAL"]);
        let hosts = hosts_lines(&extra, Vec::new());
        assert_eq!(hosts[0], extra[0]);
        assert_eq!(addresses(&hosts, "host.docker.internal"), ["203.0.113.20"]);
        assert_eq!(addresses(&hosts, "host.asagiri.internal"), [super::super::ports::HOST_ADDR]);
        assert_eq!(addresses(&hosts, "gateway.docker.internal"), [super::super::ports::HOST_ADDR]);
    }
}

/// A Docker event in Asagiri's log: containers and images coming and going,
/// with name, image and exit code; never labels (they can hold anything).
/// A BuildKit container's exec is a build session (`docker build`).
/// The `key=value` network settings the guest takes with `Run`.
fn net_opts(spec: &Spec, joined: Option<&str>, domainname: &str) -> Vec<String> {
    use asagiri_proto::net_opts as k;
    let mut opts = Vec::new();
    if let Some(target) = joined {
        opts.push(format!("{}={target}", k::NETNS));
    }
    if !spec.mac_address.is_empty() {
        opts.push(format!("{}={}", k::MAC, spec.mac_address));
    }
    opts.extend(spec.dns.iter().map(|d| format!("{}={d}", k::NAMESERVER)));
    opts.extend(spec.dns_search.iter().map(|d| format!("{}={d}", k::SEARCH)));
    opts.extend(spec.dns_options.iter().map(|d| format!("{}={d}", k::OPTION)));
    if !domainname.is_empty() {
        opts.push(format!("{}={domainname}", k::DOMAINNAME));
    }
    opts
}

fn log_event(typ: &str, action: &str, actor: &str, attrs: &serde_json::Map<String, serde_json::Value>) {
    let attr = |k: &str| attrs.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let id: String = actor.strip_prefix("sha256:").unwrap_or(actor).chars().take(12).collect();
    let (name, image) = (attr("name"), attr("image"));
    match (typ, action) {
        ("container", "die") => {
            let code = attr("exitCode");
            if code == "0" {
                crate::diag::info!("container exited", id = id, name = name, image = image, exit_code = code);
            } else {
                crate::diag::warn!("container exited", id = id, name = name, image = image, exit_code = code);
            }
        }
        ("container", "exec_die") if name.starts_with("buildx_buildkit_") => {
            crate::diag::info!("build session ended", builder = name, exit_code = attr("exitCode"));
        }
        ("container", "create" | "start" | "stop" | "destroy" | "kill" | "rename" | "commit") => {
            crate::diag::info!(format!("container {action}"), id = id, name = name, image = image);
        }
        ("image", _) => crate::diag::info!(format!("image {action}"), id = id, name = attr("name")),
        _ => crate::diag::debug!(format!("{typ} {action}"), id = id, name = name),
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// How the VM came to an end, for what the log says about it.
#[derive(Debug, PartialEq, Eq)]
enum VmEnd {
    /// Put to sleep: idle, Stop, or a restart of the VM.
    Slept,
    /// Stopped with the daemon, which was asked to stop.
    ShutDown,
    /// Nobody asked: an error.
    Crashed,
}

fn vm_end(sleeping: bool, shutting_down: bool) -> VmEnd {
    if sleeping {
        VmEnd::Slept
    } else if shutting_down {
        VmEnd::ShutDown
    } else {
        VmEnd::Crashed
    }
}

/// Whether the container's restart policy starts it when the daemon comes
/// up, or on Start after Stop: `always`, and `unless-stopped` unless the
/// user stopped it (`docker stop`), as Docker's daemon does.
fn comes_back(c: &Container) -> bool {
    let policy = c.spec.restart.name.as_str();
    (policy == "always" || (policy == "unless-stopped" && !c.stopped_by_user)) && !c.spec.auto_remove
}

/// Containers that join another's network or link to others start after
/// those, as Docker orders them.
fn joins_others(c: &Container) -> bool {
    c.spec.network_mode.starts_with("container:") || !c.spec.links.is_empty()
}

/// What a container event carries: labels, then name and image.
fn container_attrs(c: &Container) -> Option<serde_json::Map<String, serde_json::Value>> {
    if c.spec.engine_service { return None; }
    let mut attrs = c.spec.labels.as_object().cloned().unwrap_or_default();
    attrs.insert("name".into(), c.name.clone().into());
    attrs.insert("image".into(), c.spec.image.clone().into());
    Some(attrs)
}

/// The exit code Docker records for a container that never started: 127
/// when the command (or a file it needs) is missing, or a mount puts a
/// directory on a file; 126 when it may not be run; and 128 for anything
/// else, such as a network that could not be set up.
fn start_exit_code(err: &str) -> i32 {
    let err = err.to_lowercase();
    if ["executable file not found", "no such file or directory", "system cannot find the file specified", "not a directory"].iter().any(|m| err.contains(m)) {
        127
    } else if err.contains("permission denied") {
        126
    } else {
        128
    }
}

fn note_removed(st: &mut State, id: &str, code: i32) {
    // A removed container's monitor sees its record gone and quits.
    st.health.remove(id);
    st.removed.push_back((id.to_owned(), code));
    while st.removed.len() > 1024 {
        st.removed.pop_front();
    }
}

/// A BuildKit daemon that buildx runs for its docker-container driver.
pub fn is_builder(c: &Container) -> bool {
    c.name.starts_with("buildx_buildkit_")
}

fn resolve(st: &State, key: &str) -> Result<String, Error> {
    let key = key.trim_start_matches('/');
    if st.containers.contains_key(key) {
        return Ok(key.to_owned());
    }
    if let Some(c) = st.containers.values().find(|c| c.name == key) {
        return Ok(c.id.clone());
    }
    let mut matches = st.containers.keys().filter(|id| !key.is_empty() && id.starts_with(key));
    match (matches.next(), matches.next()) {
        (Some(id), None) => Ok(id.clone()),
        (Some(_), Some(_)) => Err(Error::NotFound(format!("multiple containers match {key}"))),
        _ => Err(Error::NotFound(format!("No such container: {key}"))),
    }
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| io::Read::read_exact(&mut f, &mut b)).is_err() {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        for (i, x) in b.iter_mut().enumerate() {
            *x = (t >> ((i % 16) * 8)) as u8 ^ i as u8;
        }
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The label a container chooses its local names with.
pub const DOMAIN_LABEL: &str = "dev.asagiri.domain";

/// `api.myapp`, from any of `api.myapp`, `api.myapp.asagiri.local` or
/// `api.myapp.asagiri.localhost`.
fn own_name(d: &str) -> &str {
    d.strip_suffix(".asagiri.localhost").or_else(|| d.strip_suffix(".asagiri.local")).unwrap_or(d)
}

/// Docker's container-name rule: non-empty, `[a-zA-Z0-9_.-]` only. A name
/// is a DNS label on user networks and part of `<name>.asagiri.localhost`, so
/// it must not carry spaces, slashes or control characters.
fn valid_container_name(n: &str) -> bool {
    !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// The image `name` means: a name, an ID, or `name@digest` and
/// `name:tag@digest` by the platform manifest's digest or the index's it was
/// pulled by.
fn image_named(images: Vec<Image>, name: &str) -> Option<Image> {
    let reference = asagiri_image::Reference::parse(name).ok();
    let short = reference.as_ref().map(|r| r.short()).unwrap_or_else(|| name.to_owned());
    if let Some(image) = images.iter().find(|i| {
        i.name == short
            || i.name == name
            || reference.as_ref().is_some_and(|r| i.matches(r))
            || i.config_digest == name
    }) {
        return Some(image.clone());
    }
    let prefix = name.strip_prefix("sha256:").unwrap_or(name);
    if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut matches = images.into_iter().filter(|i| {
        i.config_digest.strip_prefix("sha256:").is_some_and(|h| h.starts_with(prefix))
    });
    let first = matches.next()?;
    // Multiple tags of one image are not an ambiguous image-ID prefix.
    if matches.any(|image| image.config_digest != first.config_digest) { None } else { Some(first) }
}

#[cfg(test)]
mod image_lookup_tests {
    use asagiri_image::{Image, RunConfig};

    const INDEX: &str = "sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5";
    const ARM64: &str = "sha256:6a3345f517a7df05185c648f83d18d464f2a94ea4a69eaee39b7e1de02611e59";

    fn image(name: &str, index: Option<&str>) -> Image {
        Image {
            name: name.into(),
            digest: ARM64.into(),
            index_digest: index.map(str::to_owned),
            local_tag: false,
            config_digest: format!("sha256:{}", "c".repeat(64)),
            layers: Vec::new(),
            size: 0,
            platform: "linux/arm64".into(),
            created: String::new(),
            pulled_at: 0,
            config: RunConfig::default(),
        }
    }

    fn found(images: &[Image], name: &str) -> bool {
        super::image_named(images.to_vec(), name).is_some()
    }

    #[test]
    fn image_id_prefixes_accept_algorithm_names_and_duplicate_aliases() {
        let images = [image("app:latest", None), image("app:alias", None)];
        for length in [8, 12, 64] {
            let prefix = "c".repeat(length);
            for name in [prefix.clone(), format!("sha256:{prefix}")] {
                let found = super::image_named(images.to_vec(), &name).expect(&name);
                assert_eq!(found.config_digest, images[0].config_digest, "{name}");
            }
        }
    }

    #[test]
    fn ambiguous_image_id_prefixes_do_not_select_an_arbitrary_image() {
        let first = image("app:first", None);
        let mut second = image("app:second", None);
        second.config_digest = format!("sha256:{}d", "c".repeat(63));
        let images = [first, second];
        for name in ["cccccccc", "cccccccccccc", "sha256:cccccccccccc", "", "sha256:"] {
            assert!(!found(&images, name), "{name}");
        }
        for image in &images {
            let found = super::image_named(images.to_vec(), &image.config_digest).unwrap();
            assert_eq!(found.config_digest, image.config_digest);
        }
    }

    #[test]
    fn an_exact_image_name_takes_precedence_over_an_id_prefix() {
        let first = image("app:latest", None);
        let mut named = image("cccccccccccc:latest", None);
        named.config_digest = format!("sha256:{}", "d".repeat(64));
        let found = super::image_named(vec![first, named.clone()], "cccccccccccc").unwrap();
        assert_eq!(found.config_digest, named.config_digest);
    }

    #[test]
    fn image_id_prefixes_resolve_unique_images_across_aliases() {
        let first = image("first:latest", None);
        let alias = image("alias:latest", None);
        assert!(found(&[first.clone(), alias.clone()], "cccccccc"));
        assert!(found(&[first.clone(), alias.clone()], "sha256:cccccccc"));
        assert!(!found(&[first.clone()], ""));
        assert!(!found(&[first.clone()], "sha256:"));
        let mut other = image("other:latest", None);
        other.config_digest = format!("sha256:cccccccc{}", "d".repeat(56));
        assert!(!found(&[first.clone(), alias, other.clone()], "cccccccc"));
        assert!(!found(&[first.clone(), other.clone()], "sha256:cccccccc"));
        assert!(found(&[first, other], "first:latest"));
    }

    #[test]
    fn a_pull_by_index_digest_is_found_by_either_digest() {
        // What `kind create cluster` pulls, then creates the node from.
        let pulled = [image(&format!("kindest/node@{INDEX}"), Some(INDEX))];
        for name in [
            format!("kindest/node@{INDEX}"),
            format!("kindest/node:v1.37.0@{INDEX}"),
            format!("docker.io/kindest/node:v1.37.0@{INDEX}"),
            format!("kindest/node@{ARM64}"),
            format!("kindest/node:v1.37.0@{ARM64}"),
        ] {
            assert!(found(&pulled, &name), "{name}");
        }
        assert_eq!(pulled[0].repo_digest(), Some(format!("kindest/node@{INDEX}")));
    }

    #[test]
    fn a_digest_only_finds_images_of_its_repository() {
        let pulled = [image("kindest/node:v1.37.0", Some(INDEX))];
        assert!(found(&pulled, &format!("kindest/node@{INDEX}")));
        assert!(found(&pulled, "kindest/node:v1.37.0"));
        assert!(!found(&pulled, &format!("kindest/haproxy@{INDEX}")));
        assert!(!found(&pulled, &format!("ghcr.io/kindest/node@{INDEX}")));
        assert!(!found(&pulled, &format!("kindest/node@sha256:{}", "b".repeat(64))));
        assert!(!found(&pulled, "kindest/node:v1.36.0"));
    }

    #[test]
    fn an_image_without_an_index_shows_its_manifest_digest() {
        let pulled = image("alpine:latest", None);
        assert_eq!(pulled.repo_digest(), Some(format!("alpine@{ARM64}")));
        assert!(found(&[pulled], &format!("alpine:3@{ARM64}")));
    }
}

#[cfg(test)]
mod k8s_range_tests {
    #[test]
    fn only_the_clusters_pod_and_service_ranges_can_be_forwarded() {
        assert!(super::in_k8s_ranges([10, 43, 42, 10]));
        assert!(super::in_k8s_ranges([10, 42, 0, 17]));
        assert!(!super::in_k8s_ranges([10, 44, 0, 1]));
        assert!(!super::in_k8s_ranges([172, 17, 0, 4]));
        assert!(!super::in_k8s_ranges([127, 0, 0, 1]));
        assert!(!super::in_k8s_ranges([192, 168, 64, 1]));
    }
}

/// Docker's `CloseMonitorChannel`: the monitor stops, and a container whose
/// monitor has stopped reads as unhealthy.
fn close_monitor(h: &mut HealthRun) {
    if h.monitoring {
        h.monitoring = false;
        h.epoch += 1;
        h.health.status = super::health::UNHEALTHY.into();
    }
}

fn start_error(e: io::Error) -> Error {
    if e.get_ref().is_some_and(|error| error.is::<super::vmlink::StartRejected>()) {
        Error::BadRequest(e.to_string())
    } else {
        Error::Io(e)
    }
}

fn error_text(e: &Error) -> String {
    match e {
        Error::NotFound(m) | Error::Conflict(m) | Error::BadRequest(m) | Error::Unauthorized(m) => m.clone(),
        Error::Io(e) => e.to_string(),
    }
}

/// The longest a daemon shutdown waits for one container, whatever its
/// `--stop-timeout` says.
const SHUTDOWN_WAIT_CAP: Duration = Duration::from_secs(120);

/// Who stops every container: the daemon shutting down (each wait capped,
/// no events), or Asagiri's Stop (`docker stop`'s waits and events).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stopping {
    Shutdown,
    Engine,
}

impl Engine {
    /// How long `docker stop` waits when the request does not say: the
    /// container's `--stop-timeout`, else 10 s.
    pub fn stop_timeout(&self, key: &str) -> Duration {
        stop_wait(self.with(key, |c| c.spec.stop_timeout).ok().flatten().unwrap_or(DEFAULT_STOP_TIMEOUT))
    }

    /// Daemon shutdown and Asagiri's Stop: every running container gets its
    /// stop signal and its own stop timeout, as `docker stop` would, then
    /// SIGKILL, all at once. Returns the ones still running after SIGKILL.
    fn stop_together(&self, vm: &VmLink, ids: &[String], how: Stopping) -> Vec<String> {
        let engine = how == Stopping::Engine;
        let t0 = std::time::Instant::now();
        // (id, when to SIGKILL it, when it was SIGKILLed)
        let mut pending: Vec<(String, std::time::Instant, Option<std::time::Instant>)> = Vec::new();
        for id in ids {
            let Some((sig, secs)) = self.lock().containers.get(id).map(|c| (c.spec.stop_signal.unwrap_or(libc::SIGTERM as u32), c.spec.stop_timeout.unwrap_or(DEFAULT_STOP_TIMEOUT)))
            else {
                continue;
            };
            self.thaw_for_signal(id);
            if engine {
                self.emit_container("kill", id, &[("signal", sig.to_string())]);
            }
            let _ = vm.kill(id, sig);
            let wait = if engine { stop_wait(secs) } else { stop_wait(secs).min(SHUTDOWN_WAIT_CAP) };
            pending.push((id.clone(), t0 + wait, None));
        }
        // A SIGKILLed container's exit is quick; don't hang on one that
        // isn't. Asagiri's Stop waits as long as `docker stop` does.
        let grace = if engine { Duration::from_secs(10) } else { Duration::from_secs(2) };
        let mut exited = Vec::new();
        let mut st = self.lock();
        loop {
            let now = std::time::Instant::now();
            pending.retain(|(id, _, _)| {
                let running = st.containers.get(id).is_some_and(|c| c.status == Status::Running);
                if !running {
                    exited.push(id.clone());
                }
                running
            });
            for (id, deadline, killed) in &mut pending {
                if *deadline <= now && killed.is_none() {
                    let _ = vm.kill(id, libc::SIGKILL as u32);
                    *killed = Some(now);
                }
            }
            if pending.iter().all(|(_, _, k)| k.is_some_and(|k| now.duration_since(k) > grace)) {
                break;
            }
            let next = pending.iter().filter(|(_, _, k)| k.is_none()).map(|(_, d, _)| d.saturating_duration_since(now)).min().unwrap_or(Duration::from_millis(100));
            st = self.changed.wait_timeout(st, next.clamp(Duration::from_millis(10), Duration::from_millis(250))).unwrap_or_else(|e| e.into_inner()).0;
        }
        drop(st);
        if engine {
            for id in &exited {
                self.emit_container("stop", id, &[]);
            }
        }
        pending.into_iter().map(|(id, _, _)| id).collect()
    }

    /// Health status by container id, for `docker ps`.
    pub fn health_statuses(&self) -> HashMap<String, super::health::Health> {
        self.lock().health.iter().map(|(id, h)| (id.clone(), h.health.clone())).collect()
    }

    /// Docker's `initHealthMonitor` at every start: status back to
    /// starting, failing streak 0, the log kept, and a monitor probing.
    fn health_start(&self, id: &str) {
        {
            let mut st = self.lock();
            let wanted = st.containers.get(id).is_some_and(|c| c.spec.healthcheck.as_ref().and_then(|h| h.probe(&c.spec.shell)).is_some());
            if !wanted {
                return;
            }
            let run = st.health.entry(id.to_owned()).or_default();
            close_monitor(run);
            run.health.status = super::health::STARTING.into();
            run.health.failing_streak = 0;
        }
        self.health_resume(id);
    }

    /// Docker's `updateHealthMonitor`: a monitor while the container runs
    /// unpaused (after `unpause`, its status stays until the next probe).
    fn health_resume(&self, id: &str) {
        let Some(me) = self.me.get().and_then(Weak::upgrade) else { return };
        let epoch = {
            let mut st = self.lock();
            let wanted = st
                .containers
                .get(id)
                .is_some_and(|c| c.status == Status::Running && !c.paused && c.spec.healthcheck.as_ref().and_then(|h| h.probe(&c.spec.shell)).is_some());
            let Some(run) = st.health.get_mut(id) else { return };
            if !wanted || run.monitoring {
                return;
            }
            run.monitoring = true;
            run.epoch += 1;
            run.epoch
        };
        let id = id.to_owned();
        let spawned = std::thread::Builder::new().name("asagiri-health".into()).spawn(move || me.monitor(&id, epoch));
        if let Err(e) = spawned {
            crate::diag::error!("healthcheck not started", error = e);
        }
    }

    /// Probes a container every interval until its monitor is stopped
    /// (exit, pause, removal: `epoch` moves on).
    fn monitor(&self, id: &str, epoch: u64) {
        let current = |st: &State| st.health.get(id).is_some_and(|h| h.epoch == epoch);
        loop {
            let (check, argv, cwd, user, started, wait) = {
                let st = self.lock();
                let (Some(c), Some(run)) = (st.containers.get(id), st.health.get(id)) else { return };
                if run.epoch != epoch {
                    return;
                }
                let Some(check) = c.spec.healthcheck.clone() else { return };
                let Some(argv) = check.probe(&c.spec.shell) else { return };
                let started = c.started.unwrap_or_else(SystemTime::now);
                let since = started.elapsed().unwrap_or_default();
                let wait = run.health.next_wait(since, &check);
                (check, argv, c.spec.cwd.clone(), c.spec.user.clone(), started, wait)
            };
            let deadline = std::time::Instant::now() + wait;
            {
                let mut st = self.lock();
                loop {
                    if !current(&st) {
                        return;
                    }
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    st = self.changed.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
            let start = SystemTime::now();
            let (exit_code, output) = self.probe(id, argv, cwd, user, check.timeout());
            let end = SystemTime::now();
            let changed = {
                let mut st = self.lock();
                let auto_remove = st.containers.get(id).is_some_and(|c| c.spec.auto_remove);
                let Some(run) = st.health.get_mut(id).filter(|h| h.epoch == epoch) else { return };
                // Stopped meanwhile: Docker drops the result.
                let before = run.health.status.clone();
                let since = start.duration_since(started).unwrap_or_default();
                let probe = super::health::Probe { start: super::time::rfc3339(start), end: super::time::rfc3339(end), exit_code, output };
                run.health.record(probe, since, &check);
                (run.health.status != before).then(|| (run.health.status.clone(), auto_remove))
            };
            if let Some((status, auto_remove)) = changed {
                self.changed.notify_all();
                self.emit_container(&format!("health_status: {status}"), id, &[]);
                if !auto_remove {
                    self.save();
                }
            }
        }
    }

    /// One healthcheck probe: an exec in the container, as Docker runs it
    /// (the container's user and working directory, no terminal). Returns
    /// the exit code (-1 when it could not run or timed out) and output.
    fn probe(&self, id: &str, argv: Vec<String>, cwd: String, user: String, timeout: Duration) -> (i64, String) {
        use super::health::{OUTPUT_LIMIT, cap_output, go_duration};
        let config = ExecConfig { console_size: None, argv, env: Vec::new(), cwd, user, tty: false, stdin: false, privileged: false, internal: false };
        let exec_id = match self.exec_create(id, config) {
            Ok(e) => e,
            Err(e) => return (-1, error_text(&e)),
        };
        let rx = match self.exec_start(&exec_id) {
            Ok((_, rx)) => rx,
            Err(e) => {
                self.lock().execs.remove(&exec_id);
                return (-1, error_text(&e));
            }
        };
        let deadline = std::time::Instant::now() + timeout;
        let mut out = Vec::new();
        let mut timed_out = false;
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                Ok((_, data)) => {
                    // One byte past the limit is enough to know it was cut.
                    let room = (OUTPUT_LIMIT + 1).saturating_sub(out.len());
                    out.extend_from_slice(&data[..data.len().min(room)]);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    timed_out = true;
                    break;
                }
            }
        }
        if timed_out {
            // Not left running: probes would pile up in a stuck container.
            if let Some(vm) = self.awake() {
                let _ = vm.kill(&exec_id, libc::SIGKILL as u32);
            }
            let until = std::time::Instant::now() + Duration::from_secs(5);
            while rx.recv_timeout(until.saturating_duration_since(std::time::Instant::now())).is_ok() {}
        }
        let code = self.with_exec(&exec_id, |e| e.exit_code).ok().flatten();
        self.lock().execs.remove(&exec_id);
        if timed_out {
            return (-1, format!("Health check exceeded timeout ({})", go_duration(timeout)));
        }
        (code.map_or(-1, i64::from), cap_output(&out))
    }
}

#[cfg(test)]
mod ifnum_tests {
    #[test]
    fn a_new_network_takes_the_lowest_interface_number_not_in_use() {
        let on = |ns: &[u32]| ns.iter().map(|n| (format!("net{n}"), *n)).collect::<Vec<_>>();
        assert_eq!(super::free_ifnum(&on(&[])), 1);
        assert_eq!(super::free_ifnum(&on(&[1, 2])), 3);
        // The first of two has left: its number is free, the second's is not.
        assert_eq!(super::free_ifnum(&on(&[2])), 1);
        assert_eq!(super::free_ifnum(&on(&[2, 1])), 3);
        assert_eq!(super::free_ifnum(&on(&[1, 3])), 2);
    }

    #[test]
    fn an_interface_number_is_taken_from_the_moment_it_is_picked() {
        // Two networks joining at once: the second picks while the first is
        // still being plugged in.
        let mut taken = Vec::new();
        assert_eq!(super::take_ifnum(&mut taken, "a"), 1);
        assert_eq!(super::take_ifnum(&mut taken, "b"), 2);
        // "a" is leaving and its interface is not gone yet: "c" keeps clear
        // of its number. Once it is gone the number is free.
        assert_eq!(super::take_ifnum(&mut taken, "c"), 3);
        super::drop_ifnum(&mut taken, "a", 1);
        assert_eq!(super::take_ifnum(&mut taken, "d"), 1);
        // "b" joined again while its old interface was on the way out: only
        // the old number is given back.
        assert_eq!(super::take_ifnum(&mut taken, "b"), 4);
        super::drop_ifnum(&mut taken, "b", 2);
        assert_eq!(taken, [("c".to_owned(), 3), ("d".to_owned(), 1), ("b".to_owned(), 4)]);
    }
}

#[cfg(test)]
mod vm_recovery_tests {
    use super::*;

    /// Run only in an isolated HOME and ASAGIRI_DATA_DIR containing an Alpine
    /// image and no containers, with a signed test binary and guest assets.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires an isolated store, signed binary and a real VM"]
    fn vm_loss_live_recovers_without_a_request() {
        assert_eq!(std::env::var("ASAGIRI_LIVE_VM_RECOVERY").as_deref(), Ok("1"));
        let root = PathBuf::from(std::env::var_os("ASAGIRI_DATA_DIR").expect("isolated ASAGIRI_DATA_DIR"));
        let records = root.join("containers.json");
        if records.exists() {
            let saved: Vec<Saved> = serde_json::from_slice(&std::fs::read(records).unwrap()).unwrap();
            assert!(saved.is_empty(), "the live test requires an empty container store");
        }
        let store = Store::open(&root).unwrap();
        let image = store.images().unwrap().into_iter().find(|i| i.name == "alpine:latest").expect("pull alpine:latest into the isolated store first");
        let engine = Engine::boot(store).unwrap();
        let mut ids = Vec::new();
        for (name, policy) in [("live-always", "always"), ("live-unless", "unless-stopped"), ("live-no", "no"), ("live-stopped", "unless-stopped"), ("live-failure", "on-failure")] {
            let spec: Spec = serde_json::from_value(serde_json::json!({
                "image": image.name, "image_id": image.digest, "entrypoint": [], "cmd": ["sleep", "600"],
                "env": [], "cwd": "/", "user": "", "auto_remove": false,
                "network_mode": if name == "live-always" { "bridge" } else { "none" },
                "labels": if name == "live-always" { serde_json::json!({"dev.asagiri.k8s": "true"}) } else { serde_json::json!({}) },
                "restart": {"name": policy, "max_retries": 0}
            })).unwrap();
            let id = engine.create(Some(name.into()), spec, &image).unwrap();
            engine.start(&id).unwrap();
            ids.push(id);
        }
        engine.stop(&ids[3], Duration::ZERO).unwrap();
        for count in 1..=2 {
            let shared = engine.k8s_forward([10, 42, 0, 2], 8080).unwrap();
            let vm = engine.awake().unwrap();
            assert!(!vm.wait_stopped(Duration::ZERO), "force-stop this test's VM");
            drop(vm);
            let until = std::time::Instant::now() + Duration::from_secs(10);
            let mut st = engine.lock();
            while !ids[..2].iter().all(|id| st.containers[id].status == Status::Running && st.containers[id].restart_count == count) {
                let left = until.saturating_duration_since(std::time::Instant::now());
                assert!(!left.is_zero(), "policy containers stayed down without a new VM request");
                st = engine.changed.wait_timeout(st, left.min(Duration::from_millis(100))).unwrap().0;
            }
            for id in &ids[2..] {
                assert_eq!(st.containers[id].status, Status::Exited);
                assert_eq!(st.containers[id].restart_count, 0);
            }
            assert!(engine.k8s_fwd.lock().unwrap().is_empty(), "VM loss must release old Kubernetes shares");
            let _rebound = std::net::TcpListener::bind(("127.0.0.1", shared)).expect("the old share's listening socket must be released");
            println!("PASS: actual VM loss {count} recovered without a boot request; RestartCount={count}; excluded policies stayed down");
        }
        for (fault, count) in [("missing", 3), ("truncated", 4)] {
            let mut canceled = Vec::new();
            for action in ["stop", "kill", "remove"] {
                let mut spec = engine.with(&ids[1], |c| c.spec.clone()).unwrap();
                spec.restart.name = "always".into();
                let id = engine.create(Some(format!("retire-{fault}-{action}")), spec, &image).unwrap();
                engine.start(&id).unwrap();
                canceled.push(id);
            }
            let old = engine.awake().unwrap();
            let held = old.hold_retirement();
            let attempts = engine.boot_attempts.load(std::sync::atomic::Ordering::SeqCst);
            let blob = root.join(format!("retirement-{fault}.blob"));
            if fault == "truncated" {
                std::fs::write(&blob, b"").unwrap();
            }
            assert!(old.interrupted_layer(&blob).is_err());
            let _ = old.is_lost();
            let requested_engine = engine.clone();
            let (sent, received) = std::sync::mpsc::channel();
            let request = std::thread::spawn(move || sent.send(requested_engine.vm()).unwrap());
            assert!(received.recv_timeout(Duration::from_millis(200)).is_err(), "fallback must wait for retirement");
            assert_eq!(engine.boot_attempts.load(std::sync::atomic::Ordering::SeqCst), attempts, "neither recovery nor fallback may open replacement storage before retirement");
            let stopping = std::time::Instant::now();
            engine.stop(&canceled[0], Duration::ZERO).unwrap();
            engine.kill(&canceled[1], libc::SIGKILL as u32).unwrap();
            engine.remove(&canceled[2], false, false).unwrap();
            assert!(stopping.elapsed() < Duration::from_secs(2), "retirement must not hold container lifecycle locks");
            drop(held);
            let replacement = received.recv_timeout(Duration::from_secs(15)).unwrap().unwrap();
            request.join().unwrap();
            let until = std::time::Instant::now() + Duration::from_secs(15);
            let mut st = engine.lock();
            while !ids[..2].iter().all(|id| st.containers[id].status == Status::Running && st.containers[id].restart_count == count) {
                let left = until.saturating_duration_since(std::time::Instant::now());
                assert!(!left.is_zero(), "retirement must allow recovery after confirmed stop");
                st = engine.changed.wait_timeout(st, left.min(Duration::from_millis(100))).unwrap().0;
            }
            assert!(st.containers[&canceled[0]].stopped_by_user);
            assert_eq!(st.containers[&canceled[0]].status, Status::Exited);
            assert_eq!(st.containers[&canceled[1]].status, Status::Exited);
            assert!(!st.containers.contains_key(&canceled[2]));
            drop(st);
            old.retire().unwrap();
            drop(old);
            assert!(replacement.stats().is_ok(), "late destruction of the old generation must not affect its successor");
            if blob.exists() { std::fs::remove_file(blob).unwrap(); }
            println!("PASS: {fault} layer after header; no replacement before retirement; retained old handle inert; stop/kill/remove cancel");
        }
        {
            let mut st = engine.lock();
            for id in &ids[..2] {
                st.containers.get_mut(id).unwrap().restart_count = 10;
            }
        }
        let vm = engine.awake().unwrap();
        assert!(!vm.wait_stopped(Duration::ZERO));
        let lost_vm = vm;
        let until = std::time::Instant::now() + Duration::from_secs(5);
        let mut st = engine.lock();
        while ids[..2].iter().any(|id| st.containers[id].status == Status::Running) {
            let left = until.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "the lost VM must finish exit accounting");
            st = engine.changed.wait_timeout(st, left).unwrap().0;
        }
        assert!(st.vm_recovery_at.unwrap() > std::time::Instant::now() + Duration::from_secs(50));
        let spec = st.containers[&ids[0]].spec.clone();
        drop(st);
        let lifecycle = engine.lifecycle(&ids[0]).unwrap();
        let starting = lifecycle.lock().unwrap();
        assert!(engine.start_on_vm(&ids[0], Start::VmRecovery, lost_vm, starting).is_err(), "a start borrowing the old VM cannot claim Running after Lost");
        assert_eq!(engine.with(&ids[0], |c| (c.status, c.vm_stopped, c.exit_code)).unwrap(), (Status::Exited, true, 255));
        engine.fail_start(&ids[0], &spec, &"late VM start failure");
        assert_eq!(engine.with(&ids[0], |c| c.exit_code).unwrap(), 255, "a late start failure must preserve VM-loss accounting");
        assert!(!engine.is_awake(), "a disconnected VM must not be reported as awake");
        assert!(matches!(engine.start_inner(&ids[1], Start::VmRecovery), Err(Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock), "a remaining candidate must not reboot during backoff");
        let stopping = std::time::Instant::now();
        for id in &ids[..2] {
            engine.stop(id, Duration::ZERO).unwrap();
        }
        assert!(stopping.elapsed() < Duration::from_secs(2), "backoff must not hold a lifecycle lock");
        let mut st = engine.lock();
        while st.vm_recovery_running {
            let left = until.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "stopping the candidates cancels the recovery worker");
            st = engine.changed.wait_timeout(st, left.min(Duration::from_millis(100))).unwrap().0;
        }
        drop(st);
        assert!(!engine.is_awake(), "no eligible containers means the VM stays down");
        println!("PASS: a crash loop backs off for 60 s; manual stops cancel recovery without waiting for the delay");
        engine.shutdown();
    }

    #[test]
    fn vm_loss_recovers_running_policies_but_preserves_user_stops() {
        let dir = std::env::temp_dir().join(format!("asagiri-vm-recovery-{}-{}", std::process::id(), random_hex(8)));
        // A fresh, exclusively created store cannot contain restart records.
        // Engine::boot therefore cannot launch a VM before stopped is set.
        std::fs::create_dir(&dir).unwrap();
        let store = Store::open(&dir).unwrap();
        let engine = Engine::boot(store).unwrap();
        // Prevent any real VM boot: this test supplies the guest events.
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let image = Image {
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, local_tag: true, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        let cases = [
            ("always-live", "always", Status::Running, false),
            ("unless-live", "unless-stopped", Status::Running, false),
            ("always-stopped", "always", Status::Exited, true),
            ("unless-stopped", "unless-stopped", Status::Exited, true),
            ("always-created", "always", Status::Created, false),
            ("unless-created", "unless-stopped", Status::Created, false),
            ("no-live", "no", Status::Running, false),
            ("failure-live", "on-failure", Status::Running, false),
        ];
        let mut ids = HashMap::new();
        for (name, policy, status, stopped_by_user) in cases {
            let spec: Spec = serde_json::from_value(serde_json::json!({
                "image": "test:local", "image_id": "sha256:test", "entrypoint": [], "cmd": ["sleep", "60"],
                "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
                "restart": {"name": policy, "max_retries": 0}
            })).unwrap();
            let id = engine.create(Some(name.into()), spec, &image).unwrap();
            let mut st = engine.lock();
            let c = st.containers.get_mut(&id).unwrap();
            c.status = status;
            c.stopped_by_user = stopped_by_user;
            c.started = (status != Status::Created).then(SystemTime::now);
            ids.insert(name, id);
        }
        let lost_at = std::time::Instant::now();
        *engine.k8s_routes.lock().unwrap() = Some((1, [10, 99, 0, 2]));
        engine.on_event(Event::Lost);
        let accounted_at = std::time::Instant::now();
        assert!(engine.k8s_routes.lock().unwrap().is_none(), "the replacement VM must install fresh Kubernetes routes");
        // A VM preparation wait must not own a container's lifecycle.
        // The stopped engine prevents this synthetic test booting a VM.
        let link = engine.link.lock().unwrap();
        let starting_engine = engine.clone();
        let starting_id = ids["always-live"].clone();
        let start = std::thread::spawn(move || starting_engine.start(&starting_id));
        std::thread::sleep(Duration::from_millis(50));
        let stopping_engine = engine.clone();
        let stopping_id = ids["always-live"].clone();
        let (sent, received) = std::sync::mpsc::channel();
        let stop = std::thread::spawn(move || sent.send(stopping_engine.stop(&stopping_id, Duration::ZERO)).unwrap());
        let canceled_during_preparation = received.recv_timeout(Duration::from_millis(200));
        drop(link);
        let started = start.join().unwrap();
        stop.join().unwrap();
        assert!(matches!(canceled_during_preparation, Ok(Ok(true))), "stop must promptly report that it canceled VM recovery");
        assert!(matches!(started, Err(Error::Conflict(_))), "a newer manual stop invalidates the prepared user start");
        engine.lock().containers.get_mut(&ids["always-live"]).unwrap().stopped_by_user = false;
        // A start can finish reserving its address after Lost has already
        // reclaimed the previous run. Its failure must reclaim that too.
        let ipam = super::super::network::parse_ipam(&serde_json::json!({"Config": [{"Subnet": "10.99.0.0/24"}]})).unwrap();
        let net = engine.lock_networks().create("late-start", super::super::network::Settings { ipam, ..Default::default() }).unwrap();
        let mut late_ip = net.subnet;
        late_ip[3] = 42;
        engine.lock_networks().reserve("late-start", late_ip).unwrap();
        let late_spec = {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&ids["always-live"]).unwrap();
            c.spec.network_mode = "late-start".into();
            c.ip = Some(late_ip);
            c.spec.clone()
        };
        engine.fail_start(&ids["always-live"], &late_spec, &"late VM start failure");
        assert_eq!(engine.with(&ids["always-live"], |c| c.exit_code).unwrap(), 255);
        assert_eq!(engine.with(&ids["always-live"], |c| c.start_error.clone()).unwrap(), "late VM start failure");
        engine.lock_networks().reserve("late-start", late_ip).expect("failed recovery must release a late fixed-IP reservation");
        engine.lock_networks().release("late-start", late_ip);
        let retry_at = engine.lock().vm_recovery_at.expect("a VM loss schedules recovery with backoff");
        assert!(retry_at >= lost_at + RESTART_BASE);
        assert!(retry_at <= accounted_at + RESTART_BASE);
        let mut wanted = engine.vm_restart_candidates();
        wanted.sort();
        let mut expected = vec![ids["always-live"].clone(), ids["unless-live"].clone()];
        expected.sort();
        assert_eq!(wanted, expected);
        let saved: Vec<Saved> = serde_json::from_slice(&std::fs::read(&engine.records).unwrap()).unwrap();
        let live = saved.iter().find(|c| c.name == "always-live").unwrap();
        assert_eq!(live.status, Status::Exited);
        assert_eq!(live.exit_code, 255);
        assert_eq!(live.start_error, "late VM start failure");
        assert!(live.vm_stopped);
        assert_eq!(live.restart_count, 1, "a VM recovery counts as a restart");
        assert_eq!(saved.iter().find(|c| c.name == "unless-live").unwrap().restart_count, 1);
        for name in ["always-stopped", "unless-stopped", "always-created", "unless-created", "no-live", "failure-live"] {
            assert_eq!(saved.iter().find(|c| c.name == name).unwrap().restart_count, 0, "{name} must not restart");
        }
        // A replacement VM can die before any container has started.
        let before = std::time::Instant::now();
        engine.on_event(Event::Lost);
        let after = std::time::Instant::now();
        {
            let st = engine.lock();
            assert_eq!(st.containers[&ids["always-live"]].restart_count, 2);
            let at = st.vm_recovery_at.unwrap();
            assert!(at >= before + Duration::from_millis(200) && at <= after + Duration::from_millis(200));
        }
        // A successful request-driven boot while the worker is still
        // unwinding a failure must leave a retry for that worker to see.
        let generation = {
            let mut st = engine.lock();
            st.vm_recovery_running = true;
            st.vm_recovery_generation
        };
        engine.stopped.store(false, std::sync::atomic::Ordering::SeqCst);
        engine.recover_vm();
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let mut st = engine.lock();
            assert!(st.vm_recovery_generation > generation, "a boot racing an active recovery must not lose its retry");
            st.vm_recovery_running = false;
        }
        for (previous, delay) in [(1, Duration::from_millis(200)), (2, Duration::from_millis(400)), (10, Duration::from_secs(60)), (u32::MAX, Duration::from_secs(60))] {
            {
                let mut st = engine.lock();
                for name in ["always-live", "unless-live"] {
                    let c = st.containers.get_mut(&ids[name]).unwrap();
                    c.status = Status::Running;
                    c.restart_count = previous;
                }
            }
            let before = std::time::Instant::now();
            engine.on_event(Event::Lost);
            let after = std::time::Instant::now();
            let st = engine.lock();
            let at = st.vm_recovery_at.unwrap();
            assert!(at >= before + delay && at <= after + delay);
            assert_eq!(st.containers[&ids["always-live"]].restart_count, previous.saturating_add(1));
        }
        // A stop that arrives between the crash and the next boot wins.
        let lifecycle = engine.lifecycle(&ids["unless-live"]).unwrap();
        let starting = lifecycle.lock().unwrap();
        let stop_engine = engine.clone();
        let stop_id = ids["unless-live"].clone();
        let (sent, received) = std::sync::mpsc::channel();
        let stop = std::thread::spawn(move || {
            sent.send(stop_engine.stop(&stop_id, Duration::ZERO)).unwrap();
        });
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err(), "stop must wait for an in-flight start");
        drop(starting);
        assert!(received.recv_timeout(Duration::from_secs(2)).unwrap().unwrap(), "canceling VM recovery is a successful stop");
        stop.join().unwrap();
        assert!(!engine.stop(&ids["unless-live"], Duration::ZERO).unwrap(), "a second stop has nothing left to cancel");
        assert_eq!(engine.vm_restart_candidates(), vec![ids["always-live"].clone()]);
        engine.start_inner(&ids["unless-live"], Start::VmRecovery).unwrap();
        engine.start_inner(&ids["unless-live"], Start::Restart).unwrap();
        engine.start_inner(&ids["unless-live"], Start::DaemonRestart).unwrap();
        // A queued daemon restart can also be canceled before VM preparation,
        // even when its container was not explicitly marked stopped.
        {
            let mut st = engine.lock();
            let c = st.containers.get_mut(&ids["always-created"]).unwrap();
            c.restart_pending = true;
        }
        assert!(engine.stop(&ids["always-created"], Duration::ZERO).unwrap());
        engine.lock().containers.get_mut(&ids["always-created"]).unwrap().stopped_by_user = false;
        engine.start_inner(&ids["always-created"], Start::DaemonRestart).unwrap();
        assert_eq!(engine.lock().containers[&ids["always-created"]].status, Status::Created);
        assert_eq!(engine.lock().containers[&ids["unless-live"]].status, Status::Exited);
        assert!(engine.lock().containers[&ids["unless-live"]].stopped_by_user);
        assert!(matches!(engine.kill(&ids["always-live"], libc::SIGUSR1 as u32), Err(Error::Conflict(_))));
        engine.kill(&ids["always-live"], libc::SIGTERM as u32).expect("a terminal kill cancels a pending VM recovery");
        engine.on_event(Event::Lost);
        assert!(engine.vm_restart_candidates().is_empty());
        assert!(engine.lock().vm_recovery_at.is_none(), "no eligible containers means no automatic boot");
        let mut spec = engine.lock().containers[&ids["always-live"]].spec.clone();
        spec.auto_remove = true;
        spec.restart.name = "no".into();
        let failed = engine.create(Some("failed-auto-remove".into()), spec, &image).unwrap();
        let waiting = engine.clone();
        let waiting_id = failed.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || sent.send(waiting.wait(&waiting_id, "removed")).unwrap());
        assert!(matches!(engine.start(&failed), Err(Error::Io(_))));
        assert!(engine.resolve(&failed).is_err(), "a failed boot must remove a --rm container and wake its waiter");
        assert!(matches!(received.recv_timeout(Duration::from_secs(2)).unwrap(), Ok(128) | Err(Error::NotFound(_))));
        waiter.join().unwrap();
        assert_eq!(engine.boot_attempts.load(std::sync::atomic::Ordering::SeqCst), 0, "this synthetic test must never boot a real VM");
        drop(engine);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod net_opts_tests {
    #[test]
    fn endpoint_names_are_dockers_dns_names() {
        let id = "0123456789abcdef0123";
        assert_eq!(super::endpoint_names("web", &["api".to_owned(), "web".to_owned()], id, ""), ["web", "api", "0123456789ab"]);
        assert_eq!(super::endpoint_names("web", &[], id, "front"), ["web", "0123456789ab", "front"]);
        // Docker's default hostname is the short id, named once.
        assert_eq!(super::endpoint_names("web", &[], id, "0123456789ab"), ["web", "0123456789ab"]);
    }

    #[test]
    fn dns_mac_and_joined_namespace_reach_the_guest() {
        let spec: super::Spec = serde_json::from_value(serde_json::json!({
            "image": "alpine", "image_id": "sha256:x", "entrypoint": [], "cmd": ["sh"], "env": [], "cwd": "", "user": "",
            "auto_remove": false, "network_mode": "bridge", "labels": {},
            "mac_address": "02:42:ac:11:00:09", "dns": ["8.8.8.8", "1.1.1.1"], "dns_search": ["corp.example"], "dns_options": ["ndots:2"],
        }))
        .unwrap();
        assert_eq!(
            super::net_opts(&spec, Some("abc"), "example.com"),
            ["netns=abc", "mac=02:42:ac:11:00:09", "nameserver=8.8.8.8", "nameserver=1.1.1.1", "search=corp.example", "option=ndots:2", "domainname=example.com"]
        );
        // Records saved before these settings existed still load.
        let old: super::Spec = serde_json::from_value(serde_json::json!({
            "image": "alpine", "image_id": "sha256:x", "entrypoint": [], "cmd": ["sh"], "env": [], "cwd": "", "user": "",
            "auto_remove": false, "network_mode": "bridge", "labels": {},
        }))
        .unwrap();
        assert!(super::net_opts(&old, None, "").is_empty());
    }
}

#[cfg(test)]
mod stdin_once_tests {
    #[test]
    fn a_record_without_it_still_closes_stdin_with_the_clients_input() {
        let spec = |stdin_once: Option<bool>| -> super::Spec {
            let mut record = serde_json::json!({
                "image": "alpine", "image_id": "sha256:x", "entrypoint": [], "cmd": ["cat"], "env": [], "cwd": "", "user": "",
                "auto_remove": false, "network_mode": "bridge", "labels": {}, "open_stdin": true,
            });
            if let Some(once) = stdin_once { record["stdin_once"] = once.into(); }
            serde_json::from_value(record).unwrap()
        };
        assert!(spec(None).stdin_once);
        assert!(spec(Some(true)).stdin_once);
        assert!(!spec(Some(false)).stdin_once);
    }
}

/// A passed-through USB device keeps the VM awake: sleeping would unplug it.
#[cfg(target_os = "macos")]
fn usb_attached() -> bool {
    super::usb::any_attached()
}

#[cfg(not(target_os = "macos"))]
fn usb_attached() -> bool {
    false
}

#[cfg(target_os = "macos")]
fn detach_usb() {
    super::usb::detach_all();
}

#[cfg(not(target_os = "macos"))]
fn detach_usb() {}

#[cfg(test)]
mod vm_end_tests {
    use super::{VmEnd, vm_end};

    /// A stop that was asked for (the daemon's shutdown, `systemctl stop
    /// asagiri`) is no error; only a VM that ended on its own is.
    #[test]
    fn only_an_unrequested_vm_end_is_an_error() {
        assert_eq!(vm_end(false, true), VmEnd::ShutDown);
        assert_eq!(vm_end(true, false), VmEnd::Slept);
        assert_eq!(vm_end(true, true), VmEnd::Slept);
        assert_eq!(vm_end(false, false), VmEnd::Crashed);
    }
}

#[cfg(test)]
mod records_tests {
    use super::*;

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn an_unreadable_container_index_never_becomes_an_empty_retention_list() {
        let root = std::env::temp_dir().join(format!("asagiri-record-integrity-{}", random_hex(16)));
        std::fs::create_dir(&root).unwrap();
        let _cleanup = Cleanup(root.clone());
        let engine = Engine::boot(Store::open(&root).unwrap()).unwrap();
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let image = Image {
            name: "records:local".into(), digest: "sha256:test".into(), index_digest: None, local_tag: true,
            config_digest: "sha256:test".into(), layers: Vec::new(), size: 0, platform: "linux/arm64".into(),
            created: String::new(), pulled_at: 0, config: Default::default(),
        };
        let spec: Spec = serde_json::from_value(serde_json::json!({
            "image": "records:local", "image_id": "sha256:test", "entrypoint": [], "cmd": ["true"],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
        })).unwrap();
        let id = engine.create(Some("retained-record".into()), spec, &image).unwrap();
        assert_eq!(engine.boot_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
        let path = root.join("containers.json");
        let valid = std::fs::read(&path).unwrap();
        drop(engine);
        let logs = root.join("containers").join(&id);
        std::fs::create_dir_all(&logs).unwrap();
        let marker = logs.join("retained.log");
        std::fs::write(&marker, b"preserve the original log\n").unwrap();

        for damaged in [&valid[..valid.len() - 1], b"", b"[{}]", b"[]trailing bytes"] {
            std::fs::write(&path, damaged).unwrap();
            for _ in 0..2 {
                let error = Engine::boot(Store::open(&root).unwrap()).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert!(error.to_string().contains(&path.display().to_string()));
                assert!(error.to_string().contains("startup stopped before cleanup"));
                assert_eq!(std::fs::read(&path).unwrap(), damaged);
                assert_eq!(std::fs::read(&marker).unwrap(), b"preserve the original log\n");
            }
        }

        std::fs::write(&path, &valid).unwrap();
        let restored = Engine::boot(Store::open(&root).unwrap()).unwrap();
        assert_eq!(restored.with(&id, |c| c.name.clone()).unwrap(), "retained-record");
        assert_eq!(restored.boot_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(&marker).unwrap(), b"preserve the original log\n");
    }

    #[test]
    fn an_index_that_cannot_be_read_also_preserves_container_logs() {
        let root = std::env::temp_dir().join(format!("asagiri-record-io-{}", random_hex(16)));
        std::fs::create_dir(&root).unwrap();
        let _cleanup = Cleanup(root.clone());
        let store = Store::open(&root).unwrap();
        std::fs::create_dir(root.join("containers.json")).unwrap();
        let logs = root.join("containers").join("retained");
        std::fs::create_dir_all(&logs).unwrap();
        let marker = logs.join("original.log");
        std::fs::write(&marker, b"preserved").unwrap();
        let error = Engine::boot(store).unwrap_err();
        assert!(error.to_string().contains("containers.json"));
        assert!(error.to_string().contains("startup stopped before cleanup"));
        assert_eq!(std::fs::read(marker).unwrap(), b"preserved");
    }
}

#[cfg(test)]
mod image_loading_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    fn load_to_stopped_engine(with_layer: bool) -> (String, usize) {
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
        }
        let root = std::env::temp_dir().join(format!("asagiri-load-preparation-{}", random_hex(16)));
        let _cleanup = Cleanup(root.clone());
        let source = Store::open(root.join("source")).unwrap();
        let mut layers = Vec::new();
        if with_layer {
            let mut writer = source.ingest().unwrap();
            writer.write_all(&[0u8; 1024]).unwrap();
            let (digest, size) = writer.finish().unwrap();
            layers.push(asagiri_image::Layer {
                digest, size, media_type: "application/vnd.oci.image.layer.v1.tar".into(),
            });
        }
        let config = serde_json::json!({
            "os": "linux", "architecture": "amd64", "config": {},
            "rootfs": {"type": "layers", "diff_ids": layers.iter().map(|l| &l.digest).collect::<Vec<_>>()},
        });
        let mut writer = source.ingest().unwrap();
        writer.write_all(&serde_json::to_vec(&config).unwrap()).unwrap();
        let (digest, _) = writer.finish().unwrap();
        let image = asagiri_image::archive::from_config("load-preparation:fixture".into(), &digest, &config, layers);
        let mut archive = Vec::new();
        asagiri_image::archive::save(&source, &[image], &mut archive).unwrap();
        let engine = Engine::boot(Store::open(root.join("engine")).unwrap()).unwrap();
        // Exercise the real API and preparation error without booting any guest.
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let (mut client, server) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let serving = engine.clone();
        let worker = std::thread::spawn(move || super::super::api::serve(server, serving));
        write!(client, "POST /v1.47/images/load HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", archive.len()).unwrap();
        client.write_all(&archive).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        worker.join().unwrap().unwrap();
        assert_eq!(engine.boot_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
        let loads = engine.events.lock().unwrap().recent.iter().filter(|event| event["Action"] == "load").count();
        (response, loads)
    }

    #[test]
    fn image_load_reports_layer_preparation_failure_without_success() {
        let (response, loads) = load_to_stopped_engine(true);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"errorDetail\"") && response.contains(STOPPED), "{response}");
        assert!(!response.contains("Loaded image"), "{response}");
        assert_eq!(loads, 0, "a failed preparation is not a successful load event");
    }

    #[test]
    fn image_load_without_layers_needs_no_guest() {
        let (response, loads) = load_to_stopped_engine(false);
        assert!(response.contains("Loaded image: load-preparation:fixture"), "{response}");
        assert!(!response.contains("\"error\""), "{response}");
        assert_eq!(loads, 1);
    }
}

#[cfg(test)]
mod image_blob_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn forced_image_deletion_keeps_persisted_containers_data_until_last_removal() {
        let root = std::env::temp_dir().join(format!("asagiri-image-blob-{}", random_hex(16)));
        let engine = Engine::boot(Store::open(&root).unwrap()).unwrap();
        let config = serde_json::json!({"os": "linux", "architecture": "amd64", "config": {}});
        let mut writer = engine.store.ingest().unwrap();
        writer.write_all(&serde_json::to_vec(&config).unwrap()).unwrap();
        let (digest, _) = writer.finish().unwrap();
        let image = asagiri_image::archive::from_config("owned:latest".into(), &digest, &config, vec![]);
        engine.store.put_image(image.clone()).unwrap();
        let spec: Spec = serde_json::from_value(serde_json::json!({
            "image": image.name, "image_id": digest, "entrypoint": [], "cmd": ["true"],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
        })).unwrap();
        // Empty layers keep this a host test; no guest or other runtime boots.
        let first = engine.create(None, spec.clone(), &image).unwrap();
        let second = engine.create(None, spec, &image).unwrap();
        engine.store.remove_image_references_retaining(&digest, None, false, &engine.image_blob_users()).unwrap();
        let blob = engine.store.blob_path(&digest).unwrap();
        assert!(engine.store.images().unwrap().is_empty() && blob.exists());
        drop(engine);
        let restored = Engine::boot(Store::open(&root).unwrap()).unwrap();
        assert_eq!(restored.each(|c| Some(c.spec.image_id.clone())), [digest.clone(), digest]);
        restored.remove(&first, false, false).unwrap();
        assert!(blob.exists(), "the second container still owns the deleted image config");
        restored.remove(&second, false, false).unwrap();
        assert!(!blob.exists(), "the last container removal reclaims the retained data");
        assert_eq!(restored.boot_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(restored);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod start_exit_tests {
    use super::*;

    #[test]
    fn only_a_guest_command_rejection_is_an_invalid_start_request() {
        let message = "exec: \"/missing\": no such file or directory";
        let rejected = io::Error::other(super::super::vmlink::StartRejected(message.into()));
        assert!(matches!(start_error(rejected), Error::BadRequest(s) if s == message));
        // The writer's oversized frame and all unrelated runtime errors keep
        // their 500 contract, even when their wording resembles a command error.
        for error in [
            io::Error::new(io::ErrorKind::InvalidInput, "agent frame exceeds maximum"),
            io::Error::other(message),
        ] {
            assert!(matches!(start_error(error), Error::Io(_)));
        }
    }

    #[test]
    fn a_failed_start_gets_dockers_exit_code() {
        let io = |m: &str| Error::Io(io::Error::other(m.to_owned())).to_string();
        assert_eq!(super::start_exit_code(&io("dns: No such file or directory (os error 2)")), 127);
        assert_eq!(super::start_exit_code(&io("exec: \"nope\": executable file not found in $PATH")), 127);
        assert_eq!(super::start_exit_code(&io("mount etc/hosts: mount /proc/self/fd/28: Not a directory (os error 20)")), 127);
        assert_eq!(super::start_exit_code(&io("exec /app: Permission denied (os error 13)")), 126);
        assert_eq!(super::start_exit_code(&Error::Conflict("no free address left on network n".into()).to_string()), 128);
        assert_eq!(super::start_exit_code(&Error::NotFound("network x not found".into()).to_string()), 128);
    }

    #[test]
    fn failed_start_error_survives_daemon_restart_and_old_records_still_load() {
        let root = std::env::temp_dir().join(format!("asagiri-start-error-{}", random_hex(16)));
        std::fs::create_dir(&root).unwrap();
        let engine = Engine::boot(Store::open(&root).unwrap()).unwrap();
        // No saved policy or real guest: starting must take the production
        // failed-boot path, without touching another engine's runtime.
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let image = Image {
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, local_tag: true, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/amd64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        let spec: Spec = serde_json::from_value(serde_json::json!({
            "image": "test:local", "image_id": "sha256:test", "entrypoint": [], "cmd": ["true"],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
            "restart": {"name": "no", "max_retries": 0}
        })).unwrap();
        let id = engine.create(Some("failed-start".into()), spec, &image).unwrap();
        assert!(engine.with(&id, |c| c.start_error.is_empty()).unwrap());
        let error = engine.start(&id).unwrap_err();
        assert!(matches!(&error, Error::Io(e) if e.to_string() == STOPPED));
        let expected = (Status::Exited, 128, error_text(&error));
        assert_eq!(engine.with(&id, |c| (c.status, c.exit_code, c.start_error.clone())).unwrap(), expected);
        assert_eq!(engine.boot_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(engine);

        let restored = Engine::boot(Store::open(&root).unwrap()).unwrap();
        assert_eq!(restored.with(&id, |c| (c.status, c.exit_code, c.start_error.clone())).unwrap(), expected);
        assert_eq!(restored.boot_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(restored);
        // Upgrade compatibility: old records have no start_error field.
        let records = root.join("containers.json");
        let mut legacy: serde_json::Value = serde_json::from_slice(&std::fs::read(&records).unwrap()).unwrap();
        assert_eq!(legacy[0].as_object_mut().unwrap().remove("start_error").unwrap(), expected.2);
        std::fs::write(&records, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let restored = Engine::boot(Store::open(&root).unwrap()).unwrap();
        assert_eq!(restored.with(&id, |c| (c.status, c.exit_code, c.start_error.clone())).unwrap(), (Status::Exited, 128, String::new()));
        assert_eq!(restored.boot_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(restored);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod image_preparation_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    struct Fixture {
        engine: Arc<Engine>,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("asagiri-image-idle-{}", random_hex(16)));
            std::fs::create_dir(&root).unwrap();
            let engine = Engine::boot(Store::open(&root).unwrap()).unwrap();
            // A new empty store has no restart records. Never boot a real
            // guest in these tests; exercise the production retirement gate.
            engine.stopped.store(true, Ordering::SeqCst);
            engine.last_activity.store(now_secs().saturating_sub(61), Ordering::Relaxed);
            Self { engine, root }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            assert_eq!(self.engine.boot_attempts.load(Ordering::SeqCst), 0);
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn overlapping_preparations_block_idle_but_not_explicit_stop() {
        let f = Fixture::new();
        let first = f.engine.image_preparation();
        let second = f.engine.image_preparation();
        assert!(!f.engine.when_idle(true, || panic!("active preparation was retired")));
        assert!(f.engine.when_idle(false, || true), "explicit Stop may interrupt preparation");
        drop(first);
        assert!(!f.engine.when_idle(true, || panic!("the second preparation is still active")));
        drop(second);
        assert!(f.engine.when_idle(true, || true));
    }

    #[test]
    fn failed_cancelled_and_panicking_preparations_release_idle() {
        let f = Fixture::new();
        let image: Image = serde_json::from_value(serde_json::json!({
            "name": "idle:test", "digest": "", "config_digest": "",
            "layers": [{"digest": format!("sha256:{}", "a".repeat(64)), "size": 1,
                "media_type": "application/vnd.oci.image.layer.v1.tar"}],
            "size": 1, "platform": "linux/amd64", "pulled_at": 0, "config": {}
        })).unwrap();
        assert_eq!(f.engine.prepare_image(&image, &|| false).unwrap_err().to_string(), STOPPED);
        assert!(f.engine.when_idle(true, || true), "a failed boot must release preparation");
        assert_eq!(f.engine.prepare_image(&image, &|| true).unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(f.engine.when_idle(true, || true), "cancellation must not leave a preparation");
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _preparing = f.engine.image_preparation();
            panic!("interrupted layer installation");
        }));
        assert!(failed.is_err());
        assert!(f.engine.when_idle(true, || true), "unwinding must release preparation");
    }

    #[test]
    fn new_preparation_waits_for_retirement_then_blocks_the_next_idle_attempt() {
        let f = Fixture::new();
        let wait = Duration::from_secs(3);
        let (retiring, retirement_started) = mpsc::channel();
        let (finish, finished) = mpsc::channel();
        let (attempting, attempted) = mpsc::channel();
        let (prepared, preparation_started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        std::thread::scope(|scope| {
            let engine = &f.engine;
            let retiring = scope.spawn(move || engine.when_idle(true, || {
                retiring.send(()).unwrap();
                finished.recv_timeout(wait).unwrap();
                true
            }));
            retirement_started.recv_timeout(wait).unwrap();
            let preparing = scope.spawn(move || {
                attempting.send(()).unwrap();
                let _preparing = engine.image_preparation();
                prepared.send(()).unwrap();
                released.recv_timeout(wait).unwrap();
            });
            attempted.recv_timeout(wait).unwrap();
            let early = preparation_started.recv_timeout(Duration::from_millis(50));
            // Release retirement even if the old gate incorrectly admitted it.
            finish.send(()).unwrap();
            assert!(retiring.join().unwrap());
            assert!(early.is_err(), "preparation entered before retirement completed");
            preparation_started.recv_timeout(wait).unwrap();
            let idle = engine.when_idle(true, || true);
            release.send(()).unwrap();
            preparing.join().unwrap();
            assert!(!idle, "the new preparation must exclude the next idle attempt");
        });
        assert!(f.engine.when_idle(true, || true));
    }
}

#[cfg(test)]
mod inspect_start_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[derive(Clone, Copy)]
    enum Completion { Started, Rejected, Exited, Removed, HealthyExited }

    fn inspect_during_start(completion: Completion) -> serde_json::Value {
        let root = std::env::temp_dir().join(format!("asagiri-inspect-start-{}", random_hex(16)));
        let _cleanup = Cleanup(root.clone());
        let engine = Engine::boot(Store::open(&root).unwrap()).unwrap();
        // An isolated metadata engine: this test cannot boot a guest.
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let image = Image {
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, local_tag: true, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/amd64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        let mut spec: Spec = serde_json::from_value(serde_json::json!({
            "image": "test:local", "image_id": "sha256:test", "entrypoint": [], "cmd": ["true"],
            "env": [], "cwd": "/", "user": "", "auto_remove": false, "network_mode": "none", "labels": {},
            "restart": {"name": "no", "max_retries": 0}
        })).unwrap();
        spec.auto_remove = matches!(completion, Completion::Removed);
        let healthy = matches!(completion, Completion::HealthyExited);
        if healthy {
            spec.healthcheck = Some(super::super::health::Healthcheck {
                test: vec!["CMD".into(), "true".into()], ..Default::default()
            });
        }
        let id = engine.create(Some("starting".into()), spec.clone(), &image).unwrap();
        let lifecycle = engine.lifecycle(&id).unwrap();
        let starting = lifecycle.lock().unwrap();
        let lifecycle_refs = Arc::strong_count(&lifecycle);
        {
            // Hold precisely the state start_on_vm exposes while vm.run is
            // waiting for Started, or a healthy process while stop waits.
            let mut state = engine.lock();
            let c = state.containers.get_mut(&id).unwrap();
            c.status = Status::Running;
            c.started = Some(SystemTime::now());
            c.pid = if healthy { 1234 } else { 0 };
            c.in_guest = healthy;
            if healthy {
                state.health.insert(id.clone(), HealthRun {
                    health: super::super::health::Health {
                        status: super::super::health::HEALTHY.into(), ..Default::default()
                    },
                    epoch: 1, monitoring: true,
                });
            }
        }
        let (mut client, server) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let serving = engine.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            super::super::api::serve(server, serving)
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        write!(client, "GET /v1.47/containers/starting/json HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut response = String::new();
            let result = client.read_to_string(&mut response).map(|_| response);
            tx.send(result).unwrap();
        });
        // Inspect clones this private lifecycle Arc before waiting. Observe
        // that point so the exit cannot precede its old pre-wait health read.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&lifecycle) == lifecycle_refs {
            assert!(std::time::Instant::now() < deadline, "inspect did not reach the lifecycle wait");
            std::thread::sleep(Duration::from_millis(1));
        }
        let early = rx.recv_timeout(Duration::from_millis(100));
        let waited = matches!(early, Err(mpsc::RecvTimeoutError::Timeout));
        match completion {
            Completion::Started => {
                let mut state = engine.lock();
                let c = state.containers.get_mut(&id).unwrap();
                c.pid = 1234;
                c.in_guest = true;
            }
            Completion::Rejected => engine.fail_start(&id, &spec, &"exec: no such file or directory"),
            // The exit event is allowed to precede the Started reply. An
            // inspection must not resurrect that process or invent a PID.
            Completion::Exited | Completion::HealthyExited => engine.on_event(Event::Exited { id: &id, code: 0, oom: false }),
            Completion::Removed => {
                engine.on_event(Event::Exited { id: &id, code: 0, oom: false });
                // A full ID is also a valid name. The waiting inspection
                // must not resolve the removed ID to this new container.
                let replacement = engine.create(Some(id.clone()), spec.clone(), &image).unwrap();
                assert_ne!(replacement, id);
                assert_eq!(engine.resolve(&id).unwrap(), replacement);
            }
        }
        drop(starting);
        let response = match early {
            Ok(response) => response.unwrap(),
            Err(mpsc::RecvTimeoutError::Timeout) => rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
            Err(error) => panic!("inspect reader disconnected: {error}"),
        };
        reader.join().unwrap();
        worker.join().unwrap().unwrap();
        assert!(waited, "inspect published the pending start: {response}");
        let expected = if matches!(completion, Completion::Removed) { "HTTP/1.1 404" } else { "HTTP/1.1 200" };
        assert!(response.starts_with(expected), "{response}");
        assert_eq!(engine.boot_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
        if matches!(completion, Completion::Removed) {
            assert!(matches!(engine.inspect(&id, |_, _| ()), Err(Error::NotFound(_))),
                    "an already resolved ID must not bind a replacement before the wait either");
        }
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    #[test]
    fn inspect_waits_for_start_pid() {
        let response = inspect_during_start(Completion::Started);
        let state = &response["State"];
        assert_eq!(state["Status"], "running");
        assert_eq!(state["Running"], true);
        assert_eq!(state["Pid"], 1234);
    }

    #[test]
    fn inspect_returns_failed_start_after_waiting() {
        let response = inspect_during_start(Completion::Rejected);
        let state = &response["State"];
        assert_eq!(state["Status"], "exited");
        assert_eq!(state["Running"], false);
        assert_eq!(state["Pid"], 0);
        assert_eq!(state["ExitCode"], 127);
        assert_eq!(state["Error"], "exec: no such file or directory");
    }

    #[test]
    fn inspect_preserves_exit_before_start_reply() {
        let response = inspect_during_start(Completion::Exited);
        let state = &response["State"];
        assert_eq!(state["Status"], "exited");
        assert_eq!(state["Running"], false);
        assert_eq!(state["Pid"], 0);
        assert_eq!(state["ExitCode"], 0);
    }

    #[test]
    fn inspect_captures_health_after_waiting_for_exit() {
        let response = inspect_during_start(Completion::HealthyExited);
        let state = &response["State"];
        assert_eq!(state["Status"], "exited");
        assert_eq!(state["Running"], false);
        assert_eq!(state["Pid"], 0);
        assert_eq!(state["ExitCode"], 0);
        assert_eq!(state["Health"]["Status"], "unhealthy");
        assert_eq!(state["Health"]["FailingStreak"], 0);
    }

    #[test]
    fn inspect_does_not_rebind_a_removed_id_as_a_name() {
        let response = inspect_during_start(Completion::Removed);
        assert!(response["message"].as_str().unwrap().starts_with("No such container: "));
    }
}

#[cfg(test)]
mod builder_trust_tests {
    use super::*;

    #[test]
    fn running_builder_noop_start_cannot_clear_pending_trust() {
        let dir = std::env::temp_dir().join(format!("asagiri-builder-trust-{}-{}", std::process::id(), random_hex(8)));
        std::fs::create_dir(&dir).unwrap();
        let engine = Engine::boot(Store::open(&dir).unwrap()).unwrap();
        // Match the existing synthetic engine tests: no restart records,
        // no real guest, and any attempted guest boot is disabled.
        engine.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let image = Image {
            name: "test:local".into(), digest: "sha256:test".into(), index_digest: None, local_tag: true, config_digest: "sha256:test".into(),
            layers: Vec::new(), size: 0, platform: "linux/arm64".into(), created: String::new(), pulled_at: 0, config: Default::default(),
        };
        let spec: Spec = serde_json::from_value(serde_json::json!({
            "image":"test:local", "image_id":"sha256:test", "entrypoint":[], "cmd":["true"],
            "env":[], "cwd":"/", "user":"", "auto_remove":false, "network_mode":"none", "labels":{},
            "restart":{"name":"no", "max_retries":0}
        })).unwrap();
        let name = super::super::builder::container_name();
        let id = engine.create(Some(name.clone()), spec.clone(), &image).unwrap();
        engine.lock().containers.get_mut(&id).unwrap().status = Status::Running;
        // Exercise the real behavior that made the bug possible, not a fake
        // start callback: start_inner succeeds without touching the guest.
        assert!(engine.start(&id).is_ok());
        let pending = std::sync::atomic::AtomicBool::new(true);
        let mut aliases = vec![id.clone(), format!("/{id}"), name.clone(), format!("/{name}"), id[..12].to_owned()];
        for length in 1..6 {
            aliases.push(id[..length].to_owned());
            aliases.push(format!("/{}", &id[..length]));
        }
        for key in &aliases {
            assert_eq!(engine.resolve(key).unwrap(), id);
            assert!(matches!(super::super::builder::start_with_trust(&engine, key, &pending), Err(Error::Conflict(_))), "{key}");
            assert!(pending.load(std::sync::atomic::Ordering::Acquire));
            assert_eq!(engine.with(&id, |c| c.status).unwrap(), Status::Running);
        }
        pending.store(false, std::sync::atomic::Ordering::Release);
        for key in &aliases {
            assert!(super::super::builder::start_with_trust(&engine, key, &pending).is_ok());
        }
        // A container name wins over another container's ID prefix. Starting
        // that unrelated container must not touch the builder's pending trust.
        let other_name = &id[..6];
        let other = engine.create(Some(other_name.into()), spec, &image).unwrap();
        engine.lock().containers.get_mut(&other).unwrap().status = Status::Running;
        pending.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(engine.resolve(other_name).unwrap(), other);
        assert!(super::super::builder::start_with_trust(&engine, other_name, &pending).is_ok());
        assert!(pending.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(engine.boot_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(engine);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
