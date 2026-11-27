//! Files that CRIU records as external bind mounts. A process may have edited
//! them after container creation, so copy their actual bytes in the frozen hook.
use std::fs::File;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::super::inroot;

const ARCHIVE: &str = "network-files.tar";
const FILES: [(&str, &str); 3] = [("hostname", "asagiri-hostname"), ("hosts", "asagiri-hosts"), ("resolv.conf", "asagiri-resolv")];
const SOURCE: &str = "ASAGIRI_CHECKPOINT_NETWORK_FILES";

pub(super) fn prepare(command: &mut Command, source: &Path) {
    command.env(SOURCE, source);
    for (name, key) in FILES { command.arg("--external").arg(format!("mnt[/etc/{name}]:{key}")); }
}

pub(super) fn capture(root: BorrowedFd<'_>, images: &Path) -> io::Result<()> {
    let source = PathBuf::from(std::env::var_os(SOURCE).ok_or_else(|| io::Error::other("missing network file directory"))?);
    let mut tar = Command::new("/usr/bin/tar");
    tar.env_clear().env("PATH", super::TOOL_PATH).current_dir(&source)
        .args(["--create", "--file=-", "--numeric-owner", "--acls", "--xattrs", "--selinux", "--sparse", "--no-recursion", "--"]);
    for (name, _) in FILES {
        let expected = std::fs::symlink_metadata(source.join(name))?;
        let target = inroot::lookup(root, &Path::new("/etc").join(name))?;
        let actual = File::from(target.fd).metadata()?;
        if !expected.is_file() || (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
            return Err(io::Error::other(format!("/etc/{name} does not expose the engine-managed network file")));
        }
        tar.arg(name);
    }
    let status = tar.stdin(Stdio::null()).stdout(File::create_new(images.join(ARCHIVE))?).status()?;
    if !status.success() { return Err(io::Error::other(format!("tar of network files exited with {status}"))); }
    Ok(())
}
