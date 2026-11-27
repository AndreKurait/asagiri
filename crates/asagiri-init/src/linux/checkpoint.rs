use std::collections::BTreeSet;
use std::fs::{self, DirBuilder};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod changeset;
mod network_files;
pub(super) use network_files::{RESTORED_NETWORK_FILES, restore_network_files};
mod receive;
pub(super) use receive::receive;
mod restore_volume;
pub(super) use restore_volume::{installed_volume, restore_volume};
mod pipe_data;
mod stdio;
pub(super) use stdio::{Input as CheckpointInput, Replay as InputReplay};
mod restore_mounts;
mod restore_rootfs;
pub(super) use restore_rootfs::restore_rootfs;
mod volumes;
mod writable;
pub(super) use volumes::{FILESYSTEM_HOOK, run_filesystem_hook};

const TOOL_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

pub(super) fn verify_receiver(containers: &Path, run: &super::container::Process, name: &str, destinations: asagiri_proto::StrList<'_>) -> io::Result<()> {
    restore_mounts::external_mounts(containers, &run.id, run.pid, name, destinations)?;
    if run.awaiting_checkpoint_restore {
        let images = containers.join(&run.id).join("checkpoints").join(name);
        let network: serde_json::Value = receive::manifest(&images.join("network.json"))?;
        if let Some(external) = network.get("externalNetwork") {
            if external.as_str() != Some(asagiri_proto::CHECKPOINT_NETNS) {
                return Err(io::Error::other("unsupported checkpoint network namespace key"));
            }
            private_bridge_namespace(run)?;
        }
        stdio::restore_pipes(&images, run)?;
    }
    Ok(())
}

fn private_bridge_namespace(run: &super::container::Process) -> io::Result<fs::File> {
    let namespace = fs::File::open(format!("/proc/{}/ns/net", run.pid))?;
    let inode = namespace.metadata()?.ino();
    if run.veths.is_empty() || !run.macvlans.is_empty() || inode != run.netns || inode == fs::metadata("/proc/self/ns/net")?.ino() {
        return Err(io::Error::other("container does not own a private bridge network namespace"));
    }
    Ok(namespace)
}

#[derive(Debug)]
pub(super) enum Runtime {
    Bundled { package_version: Option<String> },
    Native { executable: PathBuf, package_version: Option<String> },
}

impl Runtime {
    pub(super) fn detect(bundled: bool) -> Option<Self> {
        if let Some(workdir) = super::transport::host_dir() {
            let executable = system_tool("criu")?;
            for helper in ["iptables-restore", "ip6tables-restore", "tar", "gzip"] {
                system_tool(helper)?;
            }
            let mut check = Self::native_command(&executable);
            check.arg("check").current_dir(workdir).stdout(Stdio::null()).stderr(Stdio::null());
            match probe(&mut check) {
                Ok(output) if output.status.success() => Some(Self::Native { executable, package_version: native_criu_version() }),
                result => {
                    eprintln!("checkpoint: system CRIU kernel check failed: {:?}", result.map(|output| output.status));
                    None
                }
            }
        } else if bundled && ["/usr/bin/tar", "/usr/bin/gzip", "/usr/sbin/iptables-restore", "/usr/sbin/ip6tables-restore"].iter().all(|path| Path::new(path).is_file()) {
            Some(Self::Bundled { package_version: bundled_criu_version() })
        } else { None }
    }

    fn command(&self) -> Command {
        match self {
            Self::Bundled { .. } => {
                #[cfg(target_arch = "aarch64")]
                let (loader, libs) = ("/bundle/lib/ld-linux-aarch64.so.1", "/bundle/usr/lib/aarch64-linux-gnu");
                #[cfg(target_arch = "x86_64")]
                let (loader, libs) = ("/bundle/lib64/ld-linux-x86-64.so.2", "/bundle/usr/lib/x86_64-linux-gnu");
                let mut command = Command::new(loader);
                command.args(["--library-path", libs, "/bundle/usr/sbin/criu"]).env("PATH", TOOL_PATH);
                command
            }
            Self::Native { executable, .. } => Self::native_command(executable),
        }
    }

    fn native_command(path: &Path) -> Command {
        let mut command = Command::new(path);
        command.env_clear().env("PATH", TOOL_PATH);
        command
    }

    fn package_version(&self) -> Option<&str> {
        match self {
            Self::Bundled { package_version } | Self::Native { package_version, .. } => package_version.as_deref(),
        }
    }
}

fn native_criu_version() -> Option<String> {
    let mut query = Command::new(system_tool("dpkg-query")?);
    query.env_clear().env("PATH", TOOL_PATH).args(["-W", "-f=${Status}\\n${Version}", "criu"])
        .stdout(Stdio::piped()).stderr(Stdio::null());
    let output = probe(&mut query).ok()?;
    let text = std::str::from_utf8(&output.stdout).ok()?;
    let (status, version) = text.trim_end().split_once('\n')?;
    (output.status.success() && status.ends_with(" ok installed") && !version.is_empty()).then(|| version.to_owned())
}

/// Resolve executables in the system binary directories, writable only by root.
fn system_tool(name: &str) -> Option<PathBuf> {
    ["/usr/sbin", "/usr/bin"].iter().find_map(|dir| {
        let input = Path::new(dir).join(name);
        let path = input.canonicalize().ok()?;
        if !matches!(path.parent()?.to_str()?, "/usr/sbin" | "/usr/bin") { return None; }
        for node in input.ancestors().chain(path.ancestors()) {
            let meta = fs::metadata(node).ok()?;
            if meta.uid() != 0 || meta.mode() & 0o022 != 0 { return None; }
        }
        let meta = fs::metadata(&path).ok()?;
        (meta.is_file() && meta.mode() & 0o111 != 0).then_some(path)
    })
}

fn probe(command: &mut Command) -> io::Result<std::process::Output> {
    let mut child = command.process_group(0).stdin(Stdio::null()).spawn()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait()?.is_some() { return child.wait_with_output(); }
        if Instant::now() >= deadline {
            // SAFETY: this child owns its process group and has not been reaped.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL); }
            child.wait()?;
            return Err(io::Error::new(io::ErrorKind::TimedOut, "checkpoint runtime probe timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn protected_paths() -> impl Iterator<Item = String> {
    super::container::protected_system_paths().map(|p| format!("/{}", p.to_str().expect("static ASCII path")))
}

fn system_mount_targets() -> io::Result<String> {
    let mut available = Vec::new();
    for path in protected_paths() {
        if Path::new(&path).try_exists()? { available.push(path); }
    }
    Ok(available.join(" "))
}

pub(super) fn mounted_system_paths(pid: libc::pid_t) -> io::Result<Vec<String>> {
    let table = fs::File::open(format!("/proc/{pid}/mountinfo"))?;
    protected_mounts_in(BufReader::new(table))
}

fn protected_mounts_in(table: impl BufRead) -> io::Result<Vec<String>> {
    let mut lines = table.split(b'\n').peekable();
    if lines.peek().is_none() { return Err(io::Error::other("empty container mount table")); }
    let mut wanted: BTreeSet<_> = protected_paths().collect();
    let mut mounted = Vec::new();
    for line in lines {
        let line = line?;
        // Mountinfo escapes whitespace in path fields. All known targets are ASCII without whitespace.
        let target = line.split(|&b| b == b' ').nth(4).ok_or_else(|| io::Error::other("invalid mountinfo record"))?;
        if let Ok(path) = std::str::from_utf8(target) && let Some(path) = wanted.take(path) { mounted.push(path); }
    }
    Ok(mounted)
}

pub(super) fn platform(out: &mut Vec<u8>, runtime: Option<&Runtime>) {
    use asagiri_proto::Msg;
    use std::ffi::CStr;
    let mut uts = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: uname fills the whole structure and NUL-terminates its strings on success.
    if unsafe { libc::uname(uts.as_mut_ptr()) } != 0 {
        Msg::Error(&format!("guest uname: {}", io::Error::last_os_error())).encode(out);
        return;
    }
    // SAFETY: uname succeeded above; both pointers remain valid for the encoding.
    let uts = unsafe { uts.assume_init() };
    let (arch, kernel) = unsafe { (CStr::from_ptr(uts.machine.as_ptr()), CStr::from_ptr(uts.release.as_ptr())) };
    let features = fs::File::open("/proc/cpuinfo").and_then(|f| common_cpu_features(BufReader::new(f))).ok().flatten();
    let version = runtime.and_then(Runtime::package_version);
    let targets = system_mount_targets().ok();
    Msg::GuestPlatform {
        architecture: &arch.to_string_lossy(), kernel: &kernel.to_string_lossy(),
        cpu_features: features.as_deref(), criu_package_version: version.unwrap_or(""), system_mount_targets: targets.as_deref(),
    }.encode(out);
}

/// Linux lists flags per processor. A missing list on any CPU makes the
/// intersection unknown, including when an older kernel omits these fields.
fn common_cpu_features(cpuinfo: impl BufRead) -> io::Result<Option<String>> {
    let mut common: Option<BTreeSet<String>> = None;
    let mut needs_flags = false;
    for line in cpuinfo.lines() {
        let line = line?;
        let Some((key, value)) = line.split_once(':') else { continue };
        match key.trim() {
            "processor" => {
                if needs_flags { return Ok(None); }
                needs_flags = true;
            }
            "flags" | "Features" if needs_flags => {
                let flags: BTreeSet<_> = value.split_ascii_whitespace().collect();
                match &mut common {
                    Some(common) => common.retain(|flag| flags.contains(flag.as_str())),
                    None => common = Some(flags.into_iter().map(str::to_owned).collect()),
                }
                needs_flags = false;
            }
            _ => {}
        }
    }
    if needs_flags { return Ok(None); }
    Ok(common.map(|flags| flags.into_iter().collect::<Vec<_>>().join(" ")))
}

fn bundled_criu_version() -> Option<String> {
    let inventory: serde_json::Value = serde_json::from_reader(BufReader::new(fs::File::open("/bundle/packages.json").ok()?)).ok()?;
    let criu = inventory["packages"].as_array()?.iter().find(|package| package["package"] == "criu")?;
    criu["version"].as_str().map(str::to_owned)
}

pub(super) fn list(containers: &Path, id: &str) -> io::Result<Vec<String>> {
    super::container::valid_id(id)?;
    let entries = match fs::read_dir(containers.join(id).join("checkpoints")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        if let Ok(name) = entry.file_name().into_string()
            && super::container::valid_id(&name).is_ok() && entry.file_type()?.is_dir() {
            names.push(name);
        }
    }
    names.sort_unstable();
    Ok(names)
}

/// Owns the CRIU child and unpublished directory until the terminal reply.
/// The agent reserves a single capture and never reaps its target PID while
/// CRIU could still use it.
pub(super) struct Capture {
    child: Child,
    target_pid: libc::pid_t,
    pub(super) container_id: String,
    staging: PathBuf,
    destination: PathBuf,
    _queued_input: Option<fs::File>,
    _replay_input: Option<fs::File>,
    cancelled: bool,
    finished: bool,
    volume_mounts: Vec<volumes::Mount>,
}

impl Capture {
    pub(super) fn uses_volume(&self, name: &str) -> bool {
        self.volume_mounts.iter().any(|m| m.name == name)
    }

    pub(super) fn start(runtime: &Runtime, run: &super::container::Process, group: &Path, containers: &Path, spec: asagiri_proto::CheckpointSpec<'_>, input: CheckpointInput<'_>, busy: impl Fn(&str) -> bool) -> io::Result<Self> {
        if run.awaiting_checkpoint_restore {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "receiver is waiting for checkpoint restore"));
        }
        let name = spec.name;
        super::container::valid_id(name)?;
        let pid = run.pid;
        let container_dir = containers.join(&run.id);
        let volume_mounts = volumes::inventory(spec.volume_mounts, pid)?;
        if let Some(mount) = volume_mounts.iter().find(|m| busy(&m.name)) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, format!("volume {} has another active writer; pause its containers and finish uploads before checkpointing", mount.name)));
        }
        let root = container_dir.join("checkpoints");
        DirBuilder::new().recursive(true).mode(0o700).create(&root)?;
        let destination = root.join(name);
        if destination.try_exists()? {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "checkpoint already exists"));
        }
        let staging = root.join(format!(".{name}.partial"));
        // A prior interrupted capture can leave unpublished images.
        match fs::remove_dir_all(&staging) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        DirBuilder::new().mode(0o700).create(&staging)?;
        let start = || {
            let queued = if run.tty { None } else { stdio::queued_input(input.queued)? };
            stdio::capture(&staging, run, input.close_when_drained)?;
            if !spec.network.is_empty() { fs::write(staging.join("network.json"), spec.network)?; }
            let mut command = runtime.command();
            command.env_remove("ASAGIRI_CHECKPOINT_REPLAY_FILE").env_remove("ASAGIRI_CHECKPOINT_REPLAY_OFFSET");
            let replay = input.replay.map(|r| r.snapshot(&mut command)).transpose()?.flatten();
            if let Some(file) = &queued {
                command.env("ASAGIRI_CHECKPOINT_QUEUED_INPUT", format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd()));
            } else { command.env_remove("ASAGIRI_CHECKPOINT_QUEUED_INPUT"); }
            let child = dump(command, run, group, &staging, &container_dir, &volume_mounts, spec.external_network)?;
            Ok((child, queued, replay))
        };
        let (child, queued, replay) = match start() {
            Ok(child) => child,
            Err(error) => {
                remove_staging(&staging);
                return Err(error);
            }
        };
        Ok(Self { child, target_pid: pid, container_id: run.id.clone(), staging, destination, _queued_input: queued, _replay_input: replay, cancelled: false, finished: false, volume_mounts })
    }

    /// Peek without making the target PID reusable. On target exit, stop the
    /// capture first and let the next SIGCHLD finish it before reaping the target.
    pub(super) fn next_reapable_pid(&mut self) -> libc::pid_t {
        loop {
            // SAFETY: waitid writes to a valid local siginfo_t. The WNOWAIT
            // flag leaves any child's exit available to its reaper.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::waitid(libc::P_ALL, 0, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
            if rc != 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted { continue; }
                return 0;
            }
            let pid = unsafe { info.si_pid() };
            if pid == self.target_pid {
                self.cancel();
                return 0;
            }
            if pid == self.child.id() as libc::pid_t { return 0; }
            return pid;
        }
    }

    fn cancel(&mut self) {
        if !self.cancelled {
            self.cancelled = true;
            // SAFETY: this child has not been reaped and owns its process group.
            unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL); }
        }
    }

    pub(super) fn poll(&mut self) -> io::Result<Option<()>> {
        let Some(status) = self.child.try_wait()? else { return Ok(None) };
        self.finished = true;
        let result = if self.cancelled {
            Err(io::Error::other("container exited during checkpoint capture"))
        } else if !status.success() {
            let detail = dump_failure(&self.staging.join("dump.log")).unwrap_or_default();
            Err(io::Error::other(format!("CRIU dump exited with {status}{detail}")))
        } else {
            volumes::finish(&self.staging, &self.volume_mounts)
                .and_then(|()| fs::rename(&self.staging, &self.destination))
        };
        if result.is_err() { remove_staging(&self.staging); }
        result.map(Some)
    }
}

fn dump_failure(path: &Path) -> io::Result<String> {
    // CRIU may produce a large trace. Only process its final 16 KiB on the agent loop.
    let mut file = fs::File::open(path)?;
    let offset = file.metadata()?.len().saturating_sub(16 * 1024);
    file.seek(SeekFrom::Start(offset))?;
    let mut tail = Vec::new();
    file.take(16 * 1024).read_to_end(&mut tail)?;
    let text = String::from_utf8_lossy(&tail);
    let mut lines = text.lines();
    if offset > 0 { lines.next(); }
    for line in lines.clone().rev().take(12).collect::<Vec<_>>().into_iter().rev() {
        eprintln!("checkpoint: {line}");
    }
    let Some(error) = lines.find_map(|line| line.split_once(") Error (").map(|(_, message)| message)) else {
        return Ok(String::new());
    };
    let message: String = error.chars().filter(|c| !c.is_control()).take(512).collect();
    Ok(format!(": Error ({message}"))
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.finished {
            self.cancel();
            let _ = self.child.wait();
            remove_staging(&self.staging);
        }
    }
}

fn remove_staging(staging: &Path) {
    if let Err(error) = fs::remove_dir_all(staging)
        && error.kind() != io::ErrorKind::NotFound {
        eprintln!("checkpoint: could not remove {}: {error}", staging.display());
    }
}

fn dump(mut command: Command, run: &super::container::Process, group: &Path, images: &Path, container_dir: &Path, mounts: &[volumes::Mount], external_network: bool) -> io::Result<Child> {
    command
        .process_group(0)
        .current_dir(images)
        .args(["dump", "-v4", "--log-file", "dump.log", "--tree"])
        .arg(run.pid.to_string())
        .arg("--freeze-cgroup").arg(group)
        .args(["--images-dir", ".", "--leave-running", "--shell-job", "--tcp-established"]);
    if external_network {
        let inode = private_bridge_namespace(run)?.metadata()?.ino();
        command.arg("--external").arg(format!("net[{inode}]:{}", asagiri_proto::CHECKPOINT_NETNS));
    }
    network_files::prepare(&mut command, &run.network_files_dir);
    writable::prepare(images, &run.image_layers)?;
    command.env("ASAGIRI_CHECKPOINT_CONTAINER_DIR", container_dir);
    volumes::prepare(&mut command, images, mounts)?;
    command.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_mounts_use_only_exact_mountpoints() {
        let table = b"1 0 0:1 / / rw - rootfs rootfs rw\n\
                      2 1 0:2 / /proc/sys ro - proc proc rw\n\
                      3 1 0:3 /proc/kcore /data/\xff rw - tmpfs tmpfs rw\n\
                      4 1 0:2 / /proc/sys ro - proc proc rw\n";
        assert_eq!(protected_mounts_in(&table[..]).unwrap(), ["/proc/sys"]);
        assert!(protected_mounts_in(&b""[..]).is_err());
        assert!(protected_mounts_in(&b"truncated"[..]).is_err());
    }

    #[test]
    fn cpu_features_describe_every_processor() {
        let parse = |s: &str| common_cpu_features(s.as_bytes()).unwrap();
        assert_eq!(parse("processor: 0\nflags: aes sse avx\n\nprocessor: 1\nflags: sse aes\n"), Some("aes sse".into()));
        assert_eq!(parse("processor: 0\nFeatures: fp\n\nprocessor: 1\nFeatures: aes"), Some(String::new()));
        assert_eq!(parse("processor: 0\nFeatures: fp\n\nprocessor: 1\n"), None);
        assert_eq!(parse("processor: 0\n\nprocessor: 1\nflags: sse\n"), None);
        assert_eq!(parse(""), None);
    }
}
