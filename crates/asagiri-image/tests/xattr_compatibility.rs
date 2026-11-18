//! Exercise the filesystem dependency used by tar with data beyond its stack buffer.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use xattr::FileExt;

static NEXT: AtomicU64 = AtomicU64::new(0);
const ATTRIBUTE: &str = "user.asagiri.large-xattr";

struct Scratch { root: PathBuf, path: PathBuf, file: File }

impl Scratch {
    fn new() -> Self {
        let base = std::env::var_os("ASAGIRI_XATTR_TEST_DIR").map(PathBuf::from).unwrap_or_else(|| {
            // ext4 can limit an inode's xattrs to one block. tmpfs supplies
            // the large values and lists these regressions need on Linux.
            if cfg!(target_os = "linux") { PathBuf::from("/dev/shm") } else { std::env::temp_dir() }
        });
        let root = base.join(format!("asagiri-xattr-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();
        let path = root.join("data");
        let file = File::create(&path).unwrap();
        Self { root, path, file }
    }

    fn value(&self) -> Vec<u8> {
        let value: Vec<u8> = (0..8192).map(|i| (i % 251) as u8).collect();
        xattr::set(&self.path, ATTRIBUTE, &value).expect("test filesystem must support 8 KiB xattrs");
        value
    }

    fn names(&self) -> Vec<std::ffi::OsString> {
        let names: Vec<_> = (0..80).map(|n| std::ffi::OsString::from(format!("user.asagiri.{n:03}.{}", "x".repeat(64)))).collect();
        for name in &names {
            xattr::set(&self.path, name, b"x").expect("test filesystem must support a name list larger than 4 KiB");
        }
        names
    }
}

impl Drop for Scratch {
    fn drop(&mut self) { fs::remove_dir_all(&self.root).unwrap(); }
}

#[test]
fn large_value_by_path() {
    let scratch = Scratch::new();
    let expected = scratch.value();
    assert_eq!(xattr::get(&scratch.path, ATTRIBUTE).unwrap().unwrap().len(), expected.len());
    assert_eq!(xattr::get(&scratch.path, ATTRIBUTE).unwrap(), Some(expected));
}

#[test]
fn large_value_by_descriptor() {
    let scratch = Scratch::new();
    let expected = scratch.value();
    assert_eq!(scratch.file.get_xattr(ATTRIBUTE).unwrap().unwrap().len(), expected.len());
    assert_eq!(scratch.file.get_xattr(ATTRIBUTE).unwrap(), Some(expected));
}

#[test]
fn large_value_through_symlink() {
    let scratch = Scratch::new();
    let expected = scratch.value();
    let link = scratch.root.join("link");
    std::os::unix::fs::symlink("data", &link).unwrap();
    assert_eq!(xattr::get_deref(&link, ATTRIBUTE).unwrap().unwrap().len(), expected.len());
    assert_eq!(xattr::get_deref(&link, ATTRIBUTE).unwrap(), Some(expected));
}

fn same_names(actual: impl Iterator<Item = std::ffi::OsString>, expected: Vec<std::ffi::OsString>) {
    // macOS may also add com.apple.provenance to a freshly created file.
    // Require every fixture-owned name without treating OS metadata as
    // a dependency failure.
    let mut actual: Vec<_> = actual.filter(|name| name.as_encoded_bytes().starts_with(b"user.asagiri.")).collect();
    actual.sort();
    assert_eq!(actual.len(), expected.len());
    assert_eq!(actual, expected);
}

#[test]
fn large_name_list_by_path() {
    let scratch = Scratch::new();
    let expected = scratch.names();
    same_names(xattr::list(&scratch.path).unwrap(), expected);
}

#[test]
fn large_name_list_by_descriptor() {
    let scratch = Scratch::new();
    let expected = scratch.names();
    same_names(scratch.file.list_xattr().unwrap(), expected);
}

#[test]
fn large_name_list_through_symlink() {
    let scratch = Scratch::new();
    let expected = scratch.names();
    let link = scratch.root.join("link");
    std::os::unix::fs::symlink("data", &link).unwrap();
    same_names(xattr::list_deref(&link).unwrap(), expected);
}
