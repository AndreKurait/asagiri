//! Resolve CRIU external mount references from a prepared receiver.
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::{network_files, receive, restore_rootfs, restore_volume, volumes};
use super::super::{container, inroot};

pub(super) fn external_mounts(containers: &Path, id: &str, pid: libc::pid_t, name: &str, destinations: asagiri_proto::StrList<'_>) -> io::Result<BTreeMap<String, PathBuf>> {
    container::valid_id(id)?;
    container::valid_id(name)?;
    let base = containers.join(id);
    let images = base.join("checkpoints").join(name);
    restore_rootfs::require_installed(&base, name)?;
    let root = inroot::open_root(Path::new(&format!("/proc/{pid}/root")))?;
    let network = base.join(network_files::RESTORED_NETWORK_FILES);
    if fs::read_to_string(network.join(".checkpoint"))? != name {
        return Err(io::Error::other("receiver has different checkpoint network files"));
    }
    let mut bindings = BTreeMap::new();
    for (file, key) in network_files::FILES {
        let backing = network.join(file);
        verify_target(root.as_fd(), &Path::new("/etc").join(file), &backing, false)?;
        bindings.insert(key.into(), backing);
    }
    let mut volumes_by_source = BTreeMap::new();
    for destination in destinations.iter() {
        if !volumes::valid_name(destination) {
            return Err(io::Error::other("invalid receiver volume name"));
        }
        let data = Path::new(container::VOLUMES).join(destination).join("_data");
        let source = restore_volume::restored_source(&data, name)?;
        if volumes_by_source.insert(source, data).is_some() {
            return Err(io::Error::other("more than one destination for a captured volume"));
        }
    }
    let mounts: Vec<volumes::Mount> = receive::manifest(&images.join(volumes::MANIFEST))?;
    for mount in mounts {
        mount.validate()?;
        let backing = volumes_by_source.get(&mount.name)
            .ok_or_else(|| io::Error::other(format!("no restored destination for volume {}", mount.name)))?;
        let target = Path::new(&mount.target);
        let mounted = verify_target(root.as_fd(), target, backing, true)?;
        verify_access(mounted.as_fd(), target, mount.read_only)?;
        bindings.insert(format!("asagiri-volume-{}", mount.name), backing.clone());
    }
    if bindings.len() != network_files::FILES.len() + volumes_by_source.len() {
        return Err(io::Error::other("destination volume does not belong to this checkpoint"));
    }
    Ok(bindings)
}

fn verify_target(root: BorrowedFd<'_>, target: &Path, backing: &Path, directory: bool) -> io::Result<File> {
    let mounted = File::from(inroot::lookup(root, target)?.fd);
    let actual = mounted.metadata()?;
    let expected = fs::symlink_metadata(backing)?;
    let expected_type = if directory { expected.is_dir() } else { expected.is_file() };
    if !expected_type || (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
        return Err(io::Error::other(format!("receiver target {} does not expose {}", target.display(), backing.display())));
    }
    Ok(mounted)
}

fn verify_access(mounted: BorrowedFd<'_>, target: &Path, read_only: bool) -> io::Result<()> {
    let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: mounted is live and fstatvfs initializes info on success.
    if unsafe { libc::fstatvfs(mounted.as_raw_fd(), info.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let actual_read_only = unsafe { info.assume_init() }.f_flag & libc::ST_RDONLY != 0;
    if actual_read_only != read_only {
        let expected = if read_only { "read-only" } else { "writable" };
        return Err(io::Error::other(format!("receiver target {} must be {expected}", target.display())));
    }
    Ok(())
}
