//! Publish a captured plain directory volume under a new Docker volume name.
use std::fs::{self, DirBuilder, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

use super::{changeset, receive, volumes};
use super::super::{container, layers, sys};

#[derive(serde::Serialize, serde::Deserialize)]
struct Receipt { container: String, checkpoint: String, source: String, inode: u64 }

pub(super) fn restored_source(data: &Path, name: &str) -> io::Result<String> {
    let receipt: Receipt = receive::manifest(&data.with_file_name(".restore-receipt"))?;
    if receipt.checkpoint != name || receipt.inode != fs::symlink_metadata(data)?.ino()
        || !volumes::valid_name(&receipt.source) {
        return Err(io::Error::other("destination volume was not restored from this checkpoint"));
    }
    Ok(receipt.source)
}

/// An interrupted extraction may have left a receipt for an unpublished staging inode.
/// Only the published directory should suppress Docker's initial image copy-up.
pub(in super::super) fn installed_volume(data: &Path) -> io::Result<bool> {
    if !data.strip_prefix(container::VOLUMES).ok().is_some_and(|p| p.components().count() == 2 && p.ends_with("_data")) { return Ok(false); }
    let file = match File::open(data.with_file_name(".restore-receipt")) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let saved: Receipt = serde_json::from_reader(file)?;
    Ok(saved.inode == fs::symlink_metadata(data)?.ino())
}

/// The worker owns a lease on the receiver and only handles this one request.
/// Open the archive before entering the private staging tree. The only fixed
/// rename after extraction publishes under the already-open destination parent.
pub(in super::super) fn restore_volume(paths: &container::Paths, id: &str, name: &str, source: &str, destination: &str) -> io::Result<()> {
    container::valid_id(id)?;
    container::valid_id(name)?;
    if !volumes::valid_name(source) || !volumes::valid_name(destination) { return Err(io::Error::other("invalid volume name")); }
    let images = paths.containers.join(id).join("checkpoints").join(name);
    let mounts: Vec<volumes::Mount> = receive::manifest(&images.join(volumes::MANIFEST))?;
    let mount = mounts.iter().find(|m| m.name == source).ok_or_else(|| io::Error::other("notfound: source volume is absent from checkpoint inventory"))?;
    let mut input = File::open(mount.archive_dir(&images).join("data.tar"))?;
    let root = Path::new(container::VOLUMES);
    let parent = root.join(destination);
    DirBuilder::new().mode(0o700).recursive(true).create(&parent)?;
    File::open(root)?.sync_all()?;
    let directory = File::open(&parent)?;
    directory.lock()?;
    let data = parent.join("_data");
    let receipt = parent.join(".restore-receipt");
    if data.try_exists()? {
        if receipt.try_exists()? {
            let saved: Receipt = serde_json::from_reader(File::open(receipt)?)?;
            if saved.container == id && saved.checkpoint == name && saved.source == source && saved.inode == fs::symlink_metadata(&data)?.ino() {
                return directory.sync_all();
            }
        }
        return Err(io::Error::other("conflict: destination volume already has data"));
    }
    let stage = parent.join(".restore-data");
    if stage.try_exists()? { fs::remove_dir_all(&stage)?; }
    DirBuilder::new().mode(0o700).create(&stage)?;
    let pending = parent.join(".restore-receipt.partial");
    let file = File::create(&pending)?;
    serde_json::to_writer(&file, &Receipt { container: id.to_owned(), checkpoint: name.to_owned(), source: source.to_owned(), inode: fs::metadata(&stage)?.ino() })?;
    file.sync_all()?;
    fs::rename(pending, receipt)?;
    directory.sync_all()?;
    let path = sys::cstr(&stage)?;
    // SAFETY: the path names this worker's private stage. No other request shares its cwd or root.
    unsafe {
        sys::check(libc::chroot(path.as_ptr()) as _, "chroot into volume staging directory")?;
        sys::check(libc::chdir(c"/".as_ptr()) as _, "enter volume staging directory")?;
    }
    let result = (|| {
        unpack(&mut input)?;
        layers::sync_tree(Path::new("/"))?;
        // SAFETY: fixed child names of the still-open volume directory. Never replace an existing _data.
        sys::check(unsafe { libc::syscall(libc::SYS_renameat2, directory.as_raw_fd(), c".restore-data".as_ptr(), directory.as_raw_fd(), c"_data".as_ptr(), libc::RENAME_NOREPLACE) } as _, "publish restored volume")?;
        Ok(())
    })();
    if result.is_err() {
        // Nothing has been published. Reclaim file data even when this was the
        // only attempt. A retry also cleans a stage after an abrupt worker exit.
        for entry in fs::read_dir("/")?.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) { let _ = fs::remove_dir_all(entry.path()); }
            else { let _ = fs::remove_file(entry.path()); }
        }
        // SAFETY: remove only our fixed staging directory and pending receipt.
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), c".restore-data".as_ptr(), libc::AT_REMOVEDIR);
            libc::unlinkat(directory.as_raw_fd(), c".restore-receipt".as_ptr(), 0);
        }
    }
    result.and_then(|()| directory.sync_all())
}

fn unpack(input: &mut File) -> io::Result<()> {
    let root = Path::new("/");
    let mut found_root = false;
    changeset::visit(input, changeset::ArchiveKind::Volume, |path, header, fields, member| {
        let kind = header.entry_type();
        if path.file_name().is_none() {
            if found_root || !kind.is_dir() { return Err(io::Error::other("invalid checkpoint: invalid volume root")); }
            found_root = true;
        } else if kind.is_file() {
            fields.unpack_regular(path, header, member)?;
        } else {
            let mut archive = tar::Archive::new(member);
            archive.set_preserve_ownerships(true);
            archive.set_preserve_permissions(true);
            let mut entries = archive.entries()?;
            let mut entry = entries.next().ok_or_else(|| io::Error::other("invalid checkpoint: missing volume member"))??;
            if kind.is_character_special() || kind.is_block_special() || kind.is_fifo() {
                layers::unpack_special(&mut entry, root, path)?;
            } else if kind.is_hard_link() {
                if !crate::layer_tar::unpack_hardlink(&mut entry, root)? { return Err(io::Error::other("invalid checkpoint volume hard link")); }
            } else if !entry.unpack_in(root)? { return Err(io::Error::other("invalid checkpoint volume path")); }
        }
        Ok(())
    })?;
    if !found_root { return Err(io::Error::other("invalid checkpoint: missing volume root")); }
    changeset::visit(input, changeset::ArchiveKind::Volume, |path, header, fields, _| {
        if path.file_name().is_none() {
            std::os::unix::fs::chown(root, Some(header.uid()? as u32), Some(header.gid()? as u32))?;
            fs::set_permissions(root, fs::Permissions::from_mode(header.mode()? & 0o7777))?;
        }
        fields.apply_attributes_and_times(path, header)
    })
}
