//! OCI hard-link targets name paths in the layer, including when rooted.
use std::io::{self, Read};
use std::path::Path;

pub(crate) fn unpack_hardlink<R: Read>(entry: &mut tar::Entry<'_, R>, dest: &Path) -> io::Result<bool> {
    let Some(link) = entry.link_name()?.map(|p| p.into_owned()) else {
        return entry.unpack_in(dest);
    };
    let Ok(relative) = link.strip_prefix("/") else {
        return entry.unpack_in(dest);
    };
    // Entry exposes resolved GNU/PAX names, but does not allow editing them.
    // Re-encode this one link and retain tar-rs's canonical destination and
    // source checks. Do not use Entry::unpack, which omits those checks.
    // Hard-link headers do not change the existing inode's attributes.
    let mut header = entry.header().clone();
    header.set_size(0);
    let mut builder = tar::Builder::new(Vec::new());
    builder.preserve_absolute(true);
    builder.append_link(&mut header, entry.path()?, relative)?;
    let bytes = builder.into_inner()?;
    let mut archive = tar::Archive::new(bytes.as_slice());
    let mut entries = archive.entries()?;
    let mut link = entries.next().ok_or_else(|| io::Error::other("normalized hard link is missing"))??;
    link.unpack_in(dest)
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
        assert_eq!(linked.mode() & 0o7777, 0o640, "link metadata must not change its source inode");
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
