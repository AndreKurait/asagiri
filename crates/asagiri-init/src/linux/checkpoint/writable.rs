//! Encode the frozen overlay upper as an OCI changeset. The mounted root also
//! contains the image and the container's mounts, which are inventoried separately.
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::super::{copy, sys};

pub(super) const MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar";

pub(super) fn prepare(images: &Path, top_first: &[String]) -> io::Result<()> {
    let root = images.join("rootfs");
    fs::create_dir(&root)?;
    let manifest = serde_json::json!({
        "baseLayers": top_first.iter().rev().collect::<Vec<_>>(),
        "mediaType": MEDIA_TYPE,
    });
    serde_json::to_writer(File::create_new(root.join("manifest.json"))?, &manifest)?;
    Ok(())
}

fn overlay_flag(path: &Path, name: &std::ffi::CStr, expected: Option<u8>) -> io::Result<bool> {
    let path = sys::cstr(path)?;
    let mut value = 0u8;
    // SAFETY: the path and name are NUL-terminated; the output holds one byte.
    let size = unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), (&mut value as *mut u8).cast(), 1) };
    if size >= 0 { return Ok(expected.is_none_or(|wanted| size == 1 && value == wanted)); }
    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::ENODATA | libc::ENOTSUP)) { Ok(false) } else { Err(error) }
}

fn marker(tar: &mut tar::Builder<File>, name: &Path) -> io::Result<()> {
    let mut header = tar::Header::new_ustar();
    header.set_size(0);
    header.set_mode(0o644);
    tar.append_data(&mut header, name, io::empty())
}

pub(super) fn capture(container: &Path, images: &Path) -> io::Result<()> {
    let root = images.join("rootfs").canonicalize()?;
    let upper = container.join("upper");
    let part = root.join("layer.partial");
    let list = root.join("paths.partial");
    let mut names = BufWriter::new(File::create_new(&list)?);
    let mut markers = tar::Builder::new(File::create_new(&part)?);
    let mut pending = vec![PathBuf::from(".")];
    while let Some(rel) = pending.pop() {
        let path = upper.join(&rel);
        let meta = fs::symlink_metadata(&path)?;
        copy::require_complete_upper(&path)?;
        if let Some(name) = rel.file_name() {
            if name.as_bytes().starts_with(b".wh.") {
                return Err(io::Error::other(format!("OCI reserves writable layer name {}", rel.display())));
            }
            if copy::is_whiteout(&meta) || (meta.is_file() && meta.len() == 0 && overlay_flag(&path, c"trusted.overlay.whiteout", None)?) {
                let mut whiteout = b".wh.".to_vec();
                whiteout.extend_from_slice(name.as_bytes());
                marker(&mut markers, &rel.with_file_name(std::ffi::OsString::from_vec(whiteout)))?;
                continue;
            }
        }
        // Socket endpoints belong to the process checkpoint; tar has no socket entry type.
        if meta.file_type().is_socket() { continue; }
        names.write_all(rel.as_os_str().as_bytes())?;
        names.write_all(&[0])?;
        if meta.is_dir() {
            if overlay_flag(&path, c"trusted.overlay.opaque", Some(b'y'))? {
                marker(&mut markers, &rel.join(".wh..wh..opq"))?;
            }
            let mut children = fs::read_dir(path)?.map(|entry| entry.map(|e| rel.join(e.file_name()))).collect::<io::Result<Vec<_>>>()?;
            children.sort_unstable();
            pending.extend(children.into_iter().rev());
        }
    }
    markers.finish()?;
    drop(markers);
    names.flush()?;
    drop(names);
    // Write the real entries in one tar invocation so links share one inode
    // table, including when their names fall in different directories.
    // GNU tar retains hard links, ownership, permissions, timestamps, ACLs and
    // application xattrs. Overlay bookkeeping is specific to the source mount.
    let status = Command::new("/usr/bin/tar").env_clear().env("PATH", super::TOOL_PATH)
        .current_dir(&upper).args(["--append", "--format=pax", "--numeric-owner", "--acls", "--xattrs", "--selinux",
            "--xattrs-exclude=trusted.overlay.*", "--no-recursion", "--null", "--verbatim-files-from", "--file"])
        .arg(&part).arg("--files-from").arg(&list).stdin(Stdio::null()).status()?;
    if !status.success() { return Err(io::Error::other(format!("tar of writable layer exited with {status}"))); }
    fs::remove_file(list)?;
    fs::rename(part, root.join("layer.tar"))
}
