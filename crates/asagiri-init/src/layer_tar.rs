//! OCI hard-link targets name paths in the layer, including when rooted.
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

pub(crate) fn unpack_hardlink<R: Read>(entry: &mut tar::Entry<'_, R>, dest: &Path) -> io::Result<bool> {
    let path = entry.path()?;
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => relative.push(name),
            Component::ParentDir => return Ok(false),
            _ => {}
        }
    }
    let name = file_name(&relative)?;
    let link = entry.link_name()?.ok_or_else(|| invalid("hard link has no source"))?;
    let source = target_path(dest, &link)?;
    let root = dest.canonicalize()?;

    let mut parent = root.clone();
    for directory in relative.parent().ok_or_else(|| invalid("hard link has no parent"))? {
        let next = parent.join(directory);
        parent = match checked_dir(&root, &next) {
            Ok(existing) => existing,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&next)?;
                next
            },
            Err(error) => return Err(error),
        };
    }
    let target = parent.join(name);

    // Link the source inode itself. A symlink can be rooted in the eventual
    // container or point to a path that has not been unpacked yet.
    match fs::hard_link(&source, &target) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if source != target {
                fs::remove_file(&target)?;
                fs::hard_link(&source, &target)?;
            }
        },
        result => result?,
    }
    Ok(true)
}

pub(crate) fn target_path(dest: &Path, path: &Path) -> io::Result<PathBuf> {
    let root = dest.canonicalize()?;
    let relative = path.strip_prefix("/").unwrap_or(path);
    let name = file_name(relative)?;
    let parent = relative.parent().ok_or_else(|| invalid("hard link has no parent"))?;
    Ok(checked_dir(&root, &root.join(parent))?.join(name))
}

fn checked_dir(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(root) {
        return Err(invalid("trying to unpack outside of destination path"));
    }
    Ok(canonical)
}

fn file_name(path: &Path) -> io::Result<&std::ffi::OsStr> {
    path.file_name().ok_or_else(|| invalid("hard link has no file name"))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let root = std::env::temp_dir().join(format!("asagiri-hardlink-{}-{stamp}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn file(&self, name: impl AsRef<Path>, contents: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn header() -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        header.set_mode(0o7777);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(1);
        header
    }

    fn archive(path: impl AsRef<Path>, target: impl AsRef<Path>) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        builder.preserve_absolute(true);
        builder.append_link(&mut header(), path, target).unwrap();
        builder.into_inner().unwrap()
    }

    fn unpack(bytes: &[u8], dest: &Path) -> io::Result<bool> {
        let mut archive = tar::Archive::new(bytes);
        let mut entries = archive.entries()?;
        unpack_hardlink(&mut entries.next().unwrap()?, dest)
    }

    fn same_inode(source: &Path, linked: &Path) {
        let source = fs::metadata(source).unwrap();
        let linked = fs::metadata(linked).unwrap();
        assert_eq!((source.dev(), source.ino()), (linked.dev(), linked.ino()));
        assert_eq!((source.uid(), source.gid(), source.mode()), (linked.uid(), linked.gid(), linked.mode()));
        assert_eq!(linked.mode() & 0o7777, 0o640, "the link constructor leaves attributes to the layer extractor");
    }

    fn boundary_error(error: &io::Error) {
        use std::error::Error;
        let mut messages = vec![error.to_string()];
        let mut cause = error.source();
        while let Some(error) = cause {
            messages.push(error.to_string());
            cause = error.source();
        }
        assert!(messages.iter().any(|message| message.contains("trying to unpack outside of destination path")), "{messages:?}");
    }

    #[test]
    fn rooted_and_relative_targets_preserve_inode_and_permissions() {
        let fixture = Fixture::new();
        let source = fixture.file("bin/tool", b"inside");
        for (index, target) in ["bin/tool", "/bin/tool", "///bin/tool", "/bin/../bin/tool"].iter().enumerate() {
            let name = format!("/links/copy-{index}");
            assert!(unpack(&archive(&name, target), &fixture.0).unwrap());
            let linked = fixture.0.join(name.trim_start_matches('/'));
            assert_eq!(fs::read(&linked).unwrap(), b"inside");
            same_inode(&source, &linked);
        }
    }

    #[test]
    fn hardlinks_preserve_rooted_and_dangling_symlink_inodes() {
        let fixture = Fixture::new();
        fixture.file("bin/tool", b"inside");
        for (index, (source, contents)) in [
            ("bin/rooted", "/bin/tool"),
            ("/bin/dangling", "not-unpacked-yet"),
        ].into_iter().enumerate() {
            let linked = format!("links/copy-{index}");
            let source_path = fixture.0.join(source.trim_start_matches('/'));
            symlink(contents, &source_path).unwrap();
            assert!(unpack(&archive(&linked, source), &fixture.0).unwrap());
            let original = fs::symlink_metadata(source_path).unwrap();
            let copy = fs::symlink_metadata(fixture.0.join(&linked)).unwrap();
            assert!(copy.file_type().is_symlink());
            assert_eq!((original.dev(), original.ino()), (copy.dev(), copy.ino()));
            assert_eq!(fs::read_link(fixture.0.join(linked)).unwrap(), Path::new(contents));
        }
    }

    #[test]
    fn existing_destinations_are_unlinked_without_following_them() {
        let fixture = Fixture::new();
        let source = fixture.file("layer/bin/tool", b"new");
        let outside = fixture.file("outside", b"outside");
        let layer = fixture.0.join("layer");
        fixture.file("layer/bin/copy", b"old");
        assert!(unpack(&archive("bin/copy", "/bin/tool"), &layer).unwrap());
        same_inode(&source, &layer.join("bin/copy"));
        symlink(&outside, layer.join("bin/link")).unwrap();
        assert!(unpack(&archive("bin/link", "bin/tool"), &layer).unwrap());
        same_inode(&source, &layer.join("bin/link"));
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert_eq!(fs::metadata(&outside).unwrap().mode() & 0o7777, 0o640);
        fs::create_dir(layer.join("bin/directory")).unwrap();
        assert!(unpack(&archive("bin/directory", "/bin/tool"), &layer).is_err());
        assert!(layer.join("bin/directory").is_dir());
    }

    #[test]
    fn gnu_long_names_and_pax_overrides_are_resolved_before_rewriting() {
        let fixture = Fixture::new();
        let name = format!("bin/{}", "long-name-".repeat(20));
        let source = fixture.file(&name, b"long target");
        let dest = format!("links/{}", "long-copy-".repeat(20));
        assert!(unpack(&archive(&dest, format!("/{name}")), &fixture.0).unwrap());
        same_inode(&source, &fixture.0.join(&dest));

        let mut builder = tar::Builder::new(Vec::new());
        let target = format!("/{name}");
        builder.append_pax_extensions([("path", b"links/pax-copy".as_slice()), ("linkpath", target.as_bytes())]).unwrap();
        builder.append_link(&mut header(), "ignored-path", "ignored-target").unwrap();
        assert!(unpack(&builder.into_inner().unwrap(), &fixture.0).unwrap());
        same_inode(&source, &fixture.0.join("links/pax-copy"));
        assert!(!fixture.0.join("ignored-path").exists());
    }

    #[test]
    fn an_absolute_host_name_still_resolves_only_in_the_layer() {
        let fixture = Fixture::new();
        let outside = fixture.file("outside", b"outside");
        let shadow = fixture.file(Path::new("layer").join(outside.strip_prefix("/").unwrap()), b"inside");
        let layer = fixture.0.join("layer");
        assert!(unpack(&archive("copy", &outside), &layer).unwrap());
        assert_eq!(fs::read(layer.join("copy")).unwrap(), b"inside");
        same_inode(&shadow, &layer.join("copy"));
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert_eq!(fs::metadata(outside).unwrap().nlink(), 1);
    }

    #[test]
    fn parent_and_symlink_targets_cannot_leave_the_layer() {
        let fixture = Fixture::new();
        let outside = fixture.file("outside/guard", b"unchanged");
        let layer = fixture.0.join("layer");
        fs::create_dir(&layer).unwrap();
        symlink(fixture.0.join("outside"), layer.join("escape")).unwrap();
        for target in ["/../outside/guard", "../outside/guard", "/escape/guard"] {
            let error = unpack(&archive("copy", target), &layer).unwrap_err();
            boundary_error(&error);
            assert!(!layer.join("copy").exists());
            assert_eq!(fs::read(&outside).unwrap(), b"unchanged");
            assert_eq!(fs::metadata(&outside).unwrap().nlink(), 1);
        }
    }

    #[test]
    fn destination_symlinks_cannot_create_a_link_outside_the_layer() {
        let fixture = Fixture::new();
        let source = fixture.file("layer/bin/tool", b"inside");
        fs::create_dir(fixture.0.join("outside")).unwrap();
        let layer = fixture.0.join("layer");
        symlink(fixture.0.join("outside"), layer.join("escape")).unwrap();
        let error = unpack(&archive("escape/copy", "/bin/tool"), &layer).unwrap_err();
        boundary_error(&error);
        assert!(!fixture.0.join("outside/copy").exists());
        assert_eq!(fs::metadata(source).unwrap().nlink(), 1);
    }

    #[test]
    fn symlinks_whose_targets_stay_in_the_layer_remain_usable() {
        let fixture = Fixture::new();
        let source = fixture.file("bin/tool", b"inside");
        symlink("bin", fixture.0.join("alias")).unwrap();
        assert!(unpack(&archive("copy", "/alias/tool"), &fixture.0).unwrap());
        same_inode(&source, &fixture.0.join("copy"));
    }
}
