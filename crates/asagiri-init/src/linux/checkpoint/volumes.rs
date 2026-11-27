use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use super::super::{container::VOLUMES, inroot};
use serde::{Deserialize, Serialize};

pub(in super::super) const FILESYSTEM_HOOK: &str = ".asagiri-filesystem-hook";
pub(super) const MANIFEST: &str = "volume-mounts.json";

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Mount {
    pub(super) name: String,
    target: String,
    read_only: bool,
}

pub(super) fn valid_name(name: &str) -> bool {
    name.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

impl Mount {
    fn source(&self) -> PathBuf { Path::new(VOLUMES).join(&self.name).join("_data") }

    pub(super) fn archive_dir(&self, images: &Path) -> PathBuf { images.join("volumes").join(&self.name) }

    pub(super) fn validate(&self) -> io::Result<()> {
        if !valid_name(&self.name) {
            return Err(io::Error::other("invalid checkpoint volume name"));
        }
        let path = Path::new(&self.target);
        if !path.is_absolute() || path == Path::new("/") || path.components().any(|c| c == Component::ParentDir) {
            return Err(io::Error::other("invalid checkpoint volume target"));
        }
        Ok(())
    }

    fn verify_target(&self, root: BorrowedFd<'_>) -> io::Result<()> {
        self.validate()?;
        let target = inroot::lookup(root, Path::new(&self.target))?;
        let actual = File::from(target.fd).metadata()?;
        let source = fs::metadata(self.source())?;
        if !source.is_dir() || (actual.dev(), actual.ino()) != (source.dev(), source.ino()) {
            return Err(io::Error::other(format!("{} does not expose volume {}", self.target, self.name)));
        }
        Ok(())
    }
}

pub(super) fn inventory(json: &str, pid: libc::pid_t) -> io::Result<Vec<Mount>> {
    if json.is_empty() { return Ok(Vec::new()); }
    let mounts: Vec<Mount> = serde_json::from_str(json)?;
    let root = inroot::open_root(Path::new(&format!("/proc/{pid}/root")))?;
    for mount in &mounts { mount.verify_target(root.as_fd())?; }
    Ok(mounts)
}

pub(super) fn prepare(command: &mut Command, images: &Path, mounts: &[Mount]) -> io::Result<()> {
    if !mounts.is_empty() { fs::create_dir(images.join("volumes"))?; }
    serde_json::to_writer(File::create_new(images.join(MANIFEST))?, mounts)?;
    // The guest PID 1 can still execute its pinned inode after switching root.
    let hook = images.join(FILESYSTEM_HOOK);
    symlink(format!("/proc/{}/exe", std::process::id()), &hook)?;
    command.arg("--action-script").arg(hook);
    for mount in mounts {
        command.arg("--external").arg(format!("mnt[{}]:asagiri-volume-{}", mount.target, mount.name));
    }
    Ok(())
}

/// CRIU invokes this hook in a separate process before resuming the dumped tree.
pub(in super::super) fn run_filesystem_hook() -> io::Result<()> {
    if std::env::var_os("CRTOOLS_SCRIPT_ACTION").as_deref() != Some("post-dump".as_ref()) { return Ok(()); }
    let images = PathBuf::from(std::env::var_os("CRTOOLS_IMAGE_DIR").ok_or_else(|| io::Error::other("missing CRIU image directory"))?);
    // A post-dump script error changes CRIU's handling of the stopped tree.
    // Let leave-running finish normally and reject publication in the agent.
    match capture_filesystems(&images) {
        Ok(()) => { File::create_new(images.join(".filesystem-complete"))?; }
        Err(error) => { fs::write(images.join(".filesystem-error"), error.to_string())?; }
    }
    Ok(())
}

fn capture_filesystems(images: &Path) -> io::Result<()> {
    super::pipe_data::capture(images)?;
    let container = std::env::var_os("ASAGIRI_CHECKPOINT_CONTAINER_DIR").ok_or_else(|| io::Error::other("missing checkpoint container directory"))?;
    super::writable::capture(Path::new(&container), images)?;
    let mounts: Vec<Mount> = serde_json::from_reader(File::open(images.join(MANIFEST))?)?;
    let pid: u32 = std::env::var("CRTOOLS_INIT_PID").map_err(io::Error::other)?.parse().map_err(io::Error::other)?;
    let root = inroot::open_root(Path::new(&format!("/proc/{pid}/root")))?;
    super::network_files::capture(root.as_fd(), images)?;
    let mountinfo = fs::read("/proc/self/mountinfo")?;
    let mut archived = BTreeSet::new();
    for mount in &mounts {
        mount.verify_target(root.as_fd())?;
        if !archived.insert(&mount.name) { continue; }
        let source = mount.source();
        // Volume names contain no mountinfo escape characters. Driver mounts
        // and nested filesystems need their own snapshot and restore policy.
        if mountinfo.split(|&b| b == b'\n').filter_map(|l| l.split(|&b| b == b' ').nth(4))
            .filter_map(|p| p.strip_prefix(source.as_os_str().as_bytes())).any(|suffix| suffix.is_empty() || suffix.starts_with(b"/")) {
            return Err(io::Error::other(format!("volume {} contains an external mount", mount.name)));
        }
        let directory = mount.archive_dir(images);
        fs::create_dir(&directory)?;
        let part = directory.join("data.partial");
        let status = Command::new("/usr/bin/tar").env_clear().env("PATH", super::TOOL_PATH)
            .current_dir(source)
            .args(["--create", "--file=-", "--numeric-owner", "--acls", "--xattrs", "--selinux", "--sparse", "--one-file-system", "--", "."])
            .stdin(Stdio::null()).stdout(File::create_new(&part)?).status()?;
        if !status.success() { return Err(io::Error::other(format!("tar of volume {} exited with {status}", mount.name))); }
        fs::rename(part, directory.join("data.tar"))?;
    }
    Ok(())
}

pub(super) fn finish(images: &Path, mounts: &[Mount]) -> io::Result<()> {
    if let Ok(error) = fs::read_to_string(images.join(".filesystem-error")) {
        return Err(io::Error::other(format!("checkpoint filesystem capture: {error}")));
    }
    fs::remove_file(images.join(".filesystem-complete"))
        .map_err(|e| io::Error::other(format!("checkpoint filesystem capture did not finish: {e}")))?;
    if !images.join("rootfs/layer.tar").is_file() { return Err(io::Error::other("checkpoint did not capture the writable layer")); }
    for mount in mounts {
        let archive = mount.archive_dir(images).join("data.tar");
        if !archive.is_file() {
            return Err(io::Error::other(format!("checkpoint did not capture volume {}", mount.name)));
        }
    }
    fs::remove_file(images.join(FILESYSTEM_HOOK))
}
