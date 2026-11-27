//! Install the checkpoint's OCI changeset as the receiver's actual overlay upper.
use super::super::{layers, sys};
use std::fs::{self, DirBuilder, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

#[derive(serde::Serialize, serde::Deserialize)]
struct Receipt { checkpoint: String, inode: u64 }

/// This copy worker starts chrooted to the receiver's storage directory. The
/// broker holds an exclusive lease and has unmounted any cached overlay.
pub(in super::super) fn restore_rootfs(name: &str, layers: asagiri_proto::StrList<'_>) -> io::Result<()> {
    let base = File::open("/")?;
    let upper = Path::new("/upper");
    let stage = Path::new("/.restore-upper");
    let receipt = Path::new("/.restore-receipt");
    let source = Path::new("/checkpoints").join(name).join("rootfs");
    super::receive::check_writable(&source, layers)?;
    if receipt.try_exists()? {
        let saved: Receipt = serde_json::from_reader(File::open(receipt)?)?;
        if saved.inode == fs::metadata(upper)?.ino() {
            if saved.checkpoint == name { return base.sync_all(); }
            return Err(io::Error::other("conflict: a different checkpoint writable layer is already installed"));
        }
    }
    if fs::read_dir(upper)?.next().transpose()?.is_some() {
        return Err(io::Error::other("conflict: receiver writable layer is not empty"));
    }
    if stage.try_exists()? { fs::remove_dir_all(stage)?; }
    DirBuilder::new().mode(0o700).create(stage)?;
    let mut input = File::open(source.join("layer.tar"))?;

    // Persist the intent before publication. Only the completed rename can
    // make the receipt's inode match upper, including across machine failure.
    let pending = File::create("/.restore-receipt.partial")?;
    serde_json::to_writer(&pending, &Receipt { checkpoint: name.to_owned(), inode: fs::metadata(stage)?.ino() })?;
    pending.sync_all()?;
    fs::rename("/.restore-receipt.partial", receipt)?;
    base.sync_all()?;

    // Only trusted code uses base after extraction, for the fixed renameat.
    // Members and hardlink targets resolve exclusively within the staging tree.
    let path = sys::cstr(stage)?;
    // SAFETY: NUL-terminated paths; this process serves just this copy request.
    unsafe {
        sys::check(libc::chroot(path.as_ptr()) as _, "chroot into writable layer")?;
        sys::check(libc::chdir(c"/".as_ptr()) as _, "enter writable layer")?;
    }
    let root = Path::new("/");
    super::changeset::visit(&mut input, super::changeset::ArchiveKind::WritableLayer, |_, _, _, member| layers::untar(member, root).map(drop))?;
    restore_metadata(&mut input)?;
    layers::sync_tree(root)?;
    // SAFETY: both names are fixed components of the still-open receiver directory.
    sys::check(unsafe { libc::renameat(base.as_raw_fd(), c".restore-upper".as_ptr(), base.as_raw_fd(), c"upper".as_ptr()) } as _, "install writable layer")?;
    base.sync_all()
}

/// GNU tar writes Linux ACLs and other extended attributes as PAX records.
/// Apply them and subsecond times after all entries exist, including ".".
fn restore_metadata(input: &mut File) -> io::Result<()> {
    let mut found_root = false;
    super::changeset::visit(input, super::changeset::ArchiveKind::WritableLayer, |path, header, fields, _| {
        if path.file_name().is_some_and(|n| n.as_encoded_bytes().starts_with(b".wh.")) { return Ok(()); }
        if path.file_name().is_none() {
            if found_root || !header.entry_type().is_dir() { return Err(io::Error::other("invalid checkpoint: unexpected writable root")); }
            let uid = u32::try_from(header.uid()?).map_err(io::Error::other)?;
            let gid = u32::try_from(header.gid()?).map_err(io::Error::other)?;
            if uid == u32::MAX || gid == u32::MAX { return Err(io::Error::other("invalid checkpoint: writable root owner")); }
            // The OCI extractor skips ".". Ownership precedes modes, ACLs and capabilities.
            // SAFETY: the root of this worker is the newly unpacked directory.
            sys::check(unsafe { libc::chown(c"/".as_ptr(), uid, gid) } as _, "set writable root owner")?;
            fs::set_permissions("/", fs::Permissions::from_mode(header.mode()? & 0o7777))?;
            found_root = true;
        }
        fields.apply_attributes_and_times(path, header)
    })?;
    if !found_root { return Err(io::Error::other("invalid checkpoint: missing writable root directory")); }
    Ok(())
}
