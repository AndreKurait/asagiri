//! Receive an export in a copy worker chrooted to the destination checkpoint store.
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use super::{stdio, volumes, writable};

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("invalid checkpoint: {message}"))
}

pub(in super::super) fn receive<R: Read>(name: &str, layers: asagiri_proto::StrList<'_>, accept: impl FnOnce() -> io::Result<R>) -> io::Result<()> {
    let root = Path::new("/");
    super::super::container::valid_id(name)?;
    // The directory FD stays open until publication. A killed worker releases the
    // lock and the next import can recover its private staging directory.
    let lock = File::open(root)?;
    // SAFETY: lock owns the open directory for the entire call and import.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        return Err(if e.kind() == io::ErrorKind::WouldBlock { io::Error::other("conflict: another checkpoint import is in progress") } else { e });
    }
    let destination = root.join(name);
    if destination.try_exists()? { return Err(io::Error::other("conflict: checkpoint already exists")); }
    let stage = root.join(".import");
    match fs::remove_dir_all(&stage) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    DirBuilder::new().mode(0o700).create(&stage)?;
    let result = (|| {
        read_archive(accept()?, &stage)?;
        check_inventory(&stage, layers)?;
        // Source capture may publish the same name concurrently. Either operation
        // installs a nonempty directory by rename, which cannot replace its peer.
        fs::rename(&stage, destination).map_err(|e| {
            if matches!(e.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST)) { io::Error::other("conflict: checkpoint already exists") } else { e }
        })?;
        lock.sync_all()
    })();
    if result.is_err() { let _ = fs::remove_dir_all(&stage); }
    result
}

pub(super) fn manifest<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    let file = File::open(path).map_err(invalid)?;
    // The source builds these manifests from a single protocol frame.
    if file.metadata()?.len() > asagiri_proto::MAX_FRAME as u64 * 2 { return Err(invalid("oversized checkpoint manifest")); }
    serde_json::from_reader(io::BufReader::new(file)).map_err(invalid)
}

fn check_inventory(stage: &Path, layers: asagiri_proto::StrList<'_>) -> io::Result<()> {
    for required in ["inventory.img", "pstree.img"] {
        if !stage.join(required).is_file() { return Err(invalid(format!("missing {required}"))); }
    }
    if stage.join("rootfs").try_exists()? {
        check_writable(&stage.join("rootfs"), layers)?;
    }
    let streams = stage.join(stdio::MANIFEST);
    let input = stage.join("stdin.bin");
    if streams.try_exists()? {
        let stdio: stdio::CheckpointStdio = manifest(&streams)?;
        stdio.validate().map_err(invalid)?;
        if stdio.stdin.is_some() != input.try_exists()? || (stdio.stdin.is_some() && !input.is_file()) {
            return Err(invalid("inconsistent checkpoint input data"));
        }
    } else if input.try_exists()? { return Err(invalid("input data without stdio inventory")); }
    let inventory = stage.join(volumes::MANIFEST);
    if inventory.try_exists()? || stage.join("volumes").try_exists()? {
        let mounts: Vec<volumes::Mount> = manifest(&inventory)?;
        for mount in mounts {
            mount.validate().map_err(invalid)?;
            if !mount.archive_dir(stage).join("data.tar").is_file() { return Err(invalid(format!("missing archive for volume {}", mount.name))); }
        }
    }
    Ok(())
}

pub(super) fn check_writable(rootfs: &Path, layers: asagiri_proto::StrList<'_>) -> io::Result<()> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Manifest { base_layers: Vec<String>, media_type: String }
    let saved: Manifest = manifest(&rootfs.join("manifest.json"))?;
    if !rootfs.join("layer.tar").is_file() || saved.media_type != writable::MEDIA_TYPE {
        return Err(invalid("incomplete writable layer"));
    }
    if !saved.base_layers.iter().rev().map(String::as_str).eq(layers.iter()) {
        return Err(io::Error::other("conflict: destination image layers differ from checkpoint"));
    }
    Ok(())
}

fn read_archive(input: impl Read, stage: &Path) -> io::Result<()> {
    let mut tar = tar::Archive::new(input);
    let mut directories = Vec::new();
    let mut long_name = None;
    // The exporter uses GNU headers for names longer than the fixed path field.
    // Content inside a nested volume/layer tar is kept opaque during import.
    for entry in tar.entries()?.raw(true) {
        let mut entry = entry.map_err(invalid)?;
        let kind = entry.header().entry_type();
        if kind.is_gnu_longname() {
            let size = entry.size();
            if long_name.is_some() || size == 0 || size > libc::PATH_MAX as u64 { return Err(invalid("invalid extended path length")); }
            let mut bytes = Vec::with_capacity(size as usize);
            entry.read_to_end(&mut bytes)?;
            if bytes.len() != size as usize || bytes.pop() != Some(0) || bytes.contains(&0) { return Err(invalid("invalid extended path")); }
            long_name = Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)));
            continue;
        }
        let path = match long_name.take() { Some(path) => path, None => entry.path().map_err(invalid)?.into_owned() };
        if path.as_os_str().is_empty() || !path.components().all(|c| matches!(c, Component::Normal(n) if !n.as_encoded_bytes().starts_with(b"."))) {
            return Err(invalid("unexpected archive path"));
        }
        let target = stage.join(path);
        if kind.is_dir() && entry.size() == 0 {
            DirBuilder::new().mode(0o700).create(&target)?;
            directories.push(target);
        } else if kind.is_file() {
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(target)?;
            let size = entry.size();
            if io::copy(&mut entry, &mut file)? != size { return Err(invalid("truncated checkpoint member")); }
            file.sync_all()?;
        } else { return Err(invalid("unexpected archive member type")); }
    }
    if long_name.is_some() { return Err(invalid("missing member after extended path")); }
    // tar-rs consumes the first zero block, and also accepts EOF in its place.
    // Require the second end block and read through transport completion before publication.
    let mut rest = tar.into_inner();
    let mut buf = [0; 8192];
    let mut tail = 0u64;
    loop {
        let n = rest.read(&mut buf)?;
        if n == 0 { break; }
        if buf[..n].iter().any(|b| *b != 0) { return Err(invalid("data after end of archive")); }
        tail += n as u64;
    }
    if tail < 512 || !tail.is_multiple_of(512) { return Err(invalid("missing tar end blocks")); }
    for directory in directories.iter().rev() { File::open(directory)?.sync_all()?; }
    File::open(stage)?.sync_all()
}
