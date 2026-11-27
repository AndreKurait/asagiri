//! Start CRIU as a direct child so the agent can adopt the restored init.
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::{Runtime, Worker, dump_failure, private_bridge_namespace, receive, restore_mounts, stdio};
use super::super::container::{self, Process};

pub(in super::super) struct RestoreProcess {
    worker: Worker,
    previous: libc::pid_t,
    group: PathBuf,
    namespace: File,
    work: PathBuf,
    deadline: Instant,
    timed_out: bool,
    pub(in super::super) container_id: String,
}

impl RestoreProcess {
    pub(in super::super) fn start(runtime: &Runtime, containers: &Path, run: &Process, name: &str, volumes: asagiri_proto::StrList<'_>) -> io::Result<Self> {
        if !run.awaiting_checkpoint_restore || run.pid_shared {
            return Err(io::Error::other("restore requires a waiting receiver with a private PID namespace"));
        }
        let mounts = restore_mounts::external_mounts(containers, &run.id, run.pid, name, volumes)?;
        let base = containers.join(&run.id);
        let images = base.join("checkpoints").join(name);
        let network: serde_json::Value = receive::manifest(&images.join("network.json"))?;
        if network["externalNetwork"].as_str() != Some(asagiri_proto::CHECKPOINT_NETNS) {
            return Err(io::Error::other("restore requires a captured external bridge namespace"));
        }
        let namespace = private_bridge_namespace(run)?;
        let pipes = stdio::restore_pipes(&images, run)?;
        let group = container::cgroup(run).ok_or_else(|| io::Error::other("receiver has no cgroup"))?.to_owned();
        if fs::read_to_string(group.join("cgroup.procs"))?.trim().parse() != Ok(run.pid) {
            return Err(io::Error::other("receiver cgroup must contain only its waiting init"));
        }
        let cgroup = OpenOptions::new().write(true).open(group.join("cgroup.procs"))?;
        let work = base.join(".restore-work");
        super::remove_staging(&work);
        DirBuilder::new().mode(0o700).create(&work)?;
        let mut command = runtime.command();
        command.current_dir(&work).process_group(0)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .args(["restore", "--no-default-config", "--manage-cgroups=ignore", "--leave-stopped",
                   "--restore-sibling", "--restore-detached", "--shell-job", "--tcp-established",
                   "-v4", "--log-file", "restore.log", "--images-dir"])
            .arg(&images).arg("--work-dir").arg(&work)
            .arg("--root").arg(base.join("rootfs"))
            .arg("--pidfile").arg(work.join("root.pid"));
        command.arg("--inherit-fd").arg(format!("fd[{}]:{}", namespace.as_raw_fd(), asagiri_proto::CHECKPOINT_NETNS));
        for (id, pipe) in &pipes {
            command.arg("--inherit-fd").arg(format!("fd[{}]:pipe:[{id}]", pipe.as_raw_fd()));
        }
        for (key, path) in mounts {
            command.arg("--external").arg(format!("mnt[{key}]:{}", path.display()));
        }
        let namespace_fd = namespace.as_raw_fd();
        // SAFETY: the closure uses only async-signal-safe syscalls and descriptors
        // already open before fork. The original namespace stays live through spawn.
        unsafe {
            command.pre_exec(move || {
                if libc::write(cgroup.as_raw_fd(), b"0".as_ptr().cast(), 1) != 1 {
                    return Err(io::Error::last_os_error());
                }
                for fd in std::iter::once(namespace_fd).chain(pipes.iter().map(|(_, file)| file.as_raw_fd())) {
                    if libc::fcntl(fd, libc::F_SETFD, 0) != 0 { return Err(io::Error::last_os_error()); }
                }
                Ok(())
            });
        }
        let worker = match command.spawn() {
            Ok(child) => Worker::new(child, run.pid),
            Err(error) => {
                super::remove_staging(&work);
                return Err(error);
            }
        };
        Ok(Self { worker, previous: run.pid, group, namespace, work,
            deadline: Instant::now() + Duration::from_secs(120), timed_out: false, container_id: run.id.clone() })
    }

    pub(in super::super) fn worker(&mut self) -> &mut Worker { &mut self.worker }

    pub(in super::super) fn clean_failed_restore(&mut self) {
        self.worker.stop_and_wait();
        if fs::read_to_string(self.group.join("cgroup.procs")).is_ok_and(|tasks| {
            tasks.lines().any(|line| line.parse() != Ok(self.previous))
        }) && let Err(error) = super::super::cgroup_kill::kill(&self.group) {
            eprintln!("checkpoint: could not clean failed restore: {error}");
        }
    }

    pub(in super::super) fn poll(&mut self) -> io::Result<Option<libc::pid_t>> {
        let Some(status) = self.worker.poll()? else {
            if Instant::now() >= self.deadline {
                self.worker.cancel();
                self.timed_out = true;
            }
            return Ok(None);
        };
        if self.timed_out {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "CRIU restore timed out"));
        }
        if self.worker.cancelled {
            return Err(io::Error::other("receiver exited during checkpoint restore"));
        }
        if !status.success() {
            let detail = dump_failure(&self.work.join("restore.log")).unwrap_or_default();
            return Err(io::Error::other(format!("CRIU restore exited with {status}{detail}")));
        }
        let pid = fs::read_to_string(self.work.join("root.pid"))?.trim().parse::<libc::pid_t>().map_err(io::Error::other)?;
        let mut exit = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: waitid writes into the local structure without reaping a child.
        if pid <= 1 || pid == self.previous
            || unsafe { libc::waitid(libc::P_PID, pid as _, exit.as_mut_ptr(), libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) } != 0
            || unsafe { exit.assume_init().si_pid() } != 0 {
            return Err(io::Error::other("restore did not produce a live agent child"));
        }
        if !fs::read_to_string(self.group.join("cgroup.procs"))?.lines().any(|line| line.parse() == Ok(pid))
            || fs::metadata(format!("/proc/{pid}/ns/net"))?.ino() != self.namespace.metadata()?.ino() {
            return Err(io::Error::other("restored init left the receiver boundary"));
        }
        if !fs::read_to_string(format!("/proc/{pid}/status"))?.lines().any(|line| line.starts_with("State:\tT")) {
            return Err(io::Error::other("CRIU did not leave the restored init stopped"));
        }
        Ok(Some(pid))
    }
}

impl Drop for RestoreProcess {
    fn drop(&mut self) {
        self.worker.stop_and_wait();
        super::remove_staging(&self.work);
    }
}
