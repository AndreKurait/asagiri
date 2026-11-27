//! Files that CRIU records as external bind mounts. A process may have edited
//! them after container creation, so copy their actual bytes in the frozen hook.
use std::fs::{self, DirBuilder, File};
use std::io::{self, Write};
use std::os::fd::BorrowedFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::super::inroot;

const ARCHIVE: &str = "network-files.tar";
const FILES: [(&str, &str); 3] = [("hostname", "asagiri-hostname"), ("hosts", "asagiri-hosts"), ("resolv.conf", "asagiri-resolv")];
const SOURCE: &str = "ASAGIRI_CHECKPOINT_NETWORK_FILES";
pub(in super::super) const RESTORED_NETWORK_FILES: &str = "restored-network-files";

/// The worker is chrooted to one stopped receiver. Publish the set by one
/// rename. A retry can discard an incomplete private stage after worker loss.
pub(in super::super) fn restore_network_files(name: &str) -> io::Result<()> {
    let destination = Path::new(RESTORED_NETWORK_FILES);
    if destination.try_exists()? {
        if fs::read_to_string(destination.join(".checkpoint"))? == name { return File::open("/")?.sync_all(); }
        return Err(io::Error::other("conflict: a different checkpoint's network files are already installed"));
    }
    let mut archive = File::open(Path::new("checkpoints").join(name).join(ARCHIVE)).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound { io::Error::other(format!("notfound: checkpoint {name} has no network file archive")) } else { e }
    })?;
    let stage = Path::new(".restore-network-files");
    if stage.try_exists()? { fs::remove_dir_all(stage)?; }
    DirBuilder::new().mode(0o700).create(stage)?;
    let installed = (|| {
        let mut found = [false; FILES.len()];
        super::changeset::visit(&mut archive, super::changeset::ArchiveKind::NetworkFiles, |path, header, fields, member| {
            let index = FILES.iter().position(|(expected, _)| path == Path::new(expected))
                .ok_or_else(|| io::Error::other("invalid checkpoint: unexpected network file name"))?;
            if !header.entry_type().is_file() || std::mem::replace(&mut found[index], true) {
                return Err(io::Error::other("invalid checkpoint: expected three distinct regular network files"));
            }
            // The three destination names come from FILES. The archive cannot
            // choose another path or install a link into this private stage.
            let target = stage.join(FILES[index].0);
            fields.unpack_regular(&target, header, member)?.sync_all()
        })?;
        if found.contains(&false) { return Err(io::Error::other("invalid checkpoint: missing network file")); }
        let mut receipt = File::create_new(stage.join(".checkpoint"))?;
        receipt.write_all(name.as_bytes())?;
        receipt.sync_all()?;
        File::open(stage)?.sync_all()?;
        fs::rename(stage, destination)?;
        File::open("/")?.sync_all()
    })();
    if installed.is_err() { let _ = fs::remove_dir_all(stage); }
    installed
}

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
