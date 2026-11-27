use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

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
    staging: PathBuf,
    destination: PathBuf,
    cancelled: bool,
    finished: bool,
}

impl Capture {
    pub(super) fn start(pid: libc::pid_t, group: &Path, container_dir: &Path, name: &str) -> io::Result<Self> {
        super::container::valid_id(name)?;
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
        let child = match dump(pid, group, &staging) {
            Ok(child) => child,
            Err(error) => {
                remove_staging(&staging);
                return Err(error);
            }
        };
        Ok(Self { child, target_pid: pid, staging, destination, cancelled: false, finished: false })
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
            if let Ok(log) = fs::read_to_string(self.staging.join("dump.log")) {
                for line in log.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev() {
                    eprintln!("checkpoint: {line}");
                }
            }
            Err(io::Error::other(format!("CRIU dump exited with {status}")))
        } else {
            fs::rename(&self.staging, &self.destination)
        };
        if result.is_err() { remove_staging(&self.staging); }
        result.map(Some)
    }
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

fn dump(pid: libc::pid_t, group: &Path, images: &Path) -> io::Result<Child> {
    #[cfg(target_arch = "aarch64")]
    let (loader, libs) = ("/bundle/lib/ld-linux-aarch64.so.1", "/bundle/usr/lib/aarch64-linux-gnu");
    #[cfg(target_arch = "x86_64")]
    let (loader, libs) = ("/bundle/lib64/ld-linux-x86-64.so.2", "/bundle/usr/lib/x86_64-linux-gnu");
    Command::new(loader)
        .process_group(0)
        .env("PATH", "/usr/bin:/bin:/usr/sbin")
        .current_dir(images)
        .args(["--library-path", libs, "/bundle/usr/sbin/criu", "dump", "-v4", "--log-file", "dump.log", "--tree"])
        .arg(pid.to_string())
        .arg("--freeze-cgroup").arg(group)
        .args(["--images-dir", ".", "--leave-running", "--shell-job", "--tcp-established"])
        .args(["--external", "mnt[/etc/hostname]:asagiri-hostname", "--external", "mnt[/etc/hosts]:asagiri-hosts", "--external", "mnt[/etc/resolv.conf]:asagiri-resolv"])
        .spawn()
}
