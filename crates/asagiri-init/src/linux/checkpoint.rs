use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::Command;

/// Called by a guest worker with the PID resolved from the agent's process table.
/// Keep checkpoints outside the container root and publish only complete images.
pub(super) fn capture(pid: libc::pid_t, container_dir: &Path, name: &str) -> io::Result<()> {
    super::container::valid_id(name)?;
    let root = container_dir.join("checkpoints");
    DirBuilder::new().recursive(true).mode(0o700).create(&root)?;
    let destination = root.join(name);
    if destination.try_exists()? {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "checkpoint already exists"));
    }
    let staging = root.join(format!(".{name}.partial"));
    DirBuilder::new().mode(0o700).create(&staging)?;
    let result = dump(pid, &staging).and_then(|()| fs::rename(&staging, destination));
    if result.is_err() {
        fs::remove_dir_all(staging)?;
    }
    result
}

fn dump(pid: libc::pid_t, images: &Path) -> io::Result<()> {
    #[cfg(target_arch = "aarch64")]
    let (loader, libs) = ("/bundle/lib/ld-linux-aarch64.so.1", "/bundle/usr/lib/aarch64-linux-gnu");
    #[cfg(target_arch = "x86_64")]
    let (loader, libs) = ("/bundle/lib64/ld-linux-x86-64.so.2", "/bundle/usr/lib/x86_64-linux-gnu");
    let status = Command::new(loader)
        .env("PATH", "/usr/bin:/bin:/usr/sbin")
        .current_dir(images)
        .args(["--library-path", libs, "/bundle/usr/sbin/criu", "dump", "-v4", "--log-file", "dump.log", "--tree"])
        .arg(pid.to_string())
        .args(["--images-dir", ".", "--leave-running", "--shell-job", "--tcp-established"])
        .args(["--external", "mnt[/etc/hostname]:asagiri-hostname", "--external", "mnt[/etc/hosts]:asagiri-hosts", "--external", "mnt[/etc/resolv.conf]:asagiri-resolv"])
        .status()?;
    if !status.success() {
        if let Ok(log) = fs::read_to_string(images.join("dump.log")) {
            for line in log.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev() {
                eprintln!("checkpoint: {line}");
            }
        }
        return Err(io::Error::other(format!("CRIU dump exited with {status}")));
    }
    Ok(())
}
