use super::*;
use super::super::container::{self, Paths};
use std::path::PathBuf;

const CHILD: &str = "ASAGIRI_OVERLAY_TEST_CHILD";
const REQUIRED: &str = "ASAGIRI_REQUIRE_MANY_LAYER_TEST";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("asagiri-overlay-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Only paths created in this process's private mount namespace.
        for root in ["containers/many/rootfs", "short/root", "long/root"] {
            let _ = sys::umount_lazy(&self.0.join(root));
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn child(name: &str) -> bool {
    if std::env::var(CHILD).as_deref() == Ok(name) {
        // CLONE_NEWNS also gives this thread its own filesystem context.
        // SAFETY: this is a disposable test process; no host mounts change.
        assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNS) }, 0, "{}", io::Error::last_os_error());
        sys::remount(c"/", libc::MS_REC | libc::MS_PRIVATE).unwrap();
        return true;
    }
    if unsafe { libc::geteuid() } != 0 {
        assert!(std::env::var_os(REQUIRED).is_none(), "CI must run the real overlay mount tests as root");
        eprintln!("SKIP real overlay mounts: requires a private mount namespace");
        return false;
    }
    let selected = format!("linux::overlay::tests::{name}");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &selected, "--nocapture", "--test-threads=1"])
        .env(CHILD, name).output().unwrap();
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "the real test was not selected");
    if std::env::var_os(REQUIRED).is_some() {
        assert!(String::from_utf8_lossy(&output.stdout).contains("PASS real overlay"), "CI did not execute the required mount checks");
    }
    false
}

#[test]
fn many_layers_use_one_mount_and_keep_copyup_data() {
    if !child("many_layers_use_one_mount_and_keep_copyup_data") { return; }
    let scratch = Scratch::new();
    let paths = Paths { layers: scratch.0.join("layers"), containers: scratch.0.join("containers"), cgroup: scratch.0.join("cgroup") };
    fs::create_dir_all(&paths.layers).unwrap();
    let mut digests = Vec::new();
    for number in 0..120 {
        let hex = format!("{number:064x}");
        let lower = paths.layers.join(&hex);
        fs::create_dir(&lower).unwrap();
        fs::write(lower.join(format!("layer-{number}")), number.to_string()).unwrap();
        fs::write(lower.join("winner"), number.to_string()).unwrap();
        fs::write(paths.layers.join(format!("{hex}.done")), b"").unwrap();
        digests.push(format!("sha256:{hex}"));
    }
    // Exercise the production layer validation, deduplication and ordering.
    digests.push(digests[0].clone());
    std::env::set_current_dir(&paths.layers).unwrap();
    let mut frame = Vec::new();
    asagiri_proto::encode_copy(&mut frame, asagiri_proto::copy_op::OUT, "many", "/", 0, &digests, "", &[]);
    let asagiri_proto::Msg::Copy(spec) = asagiri_proto::decode(&frame).unwrap() else { panic!("copy spec"); };
    let root = match container::mount_rootfs(&paths, "many", spec.layers) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::Unsupported && std::env::var_os(REQUIRED).is_none() => {
            eprintln!("SKIP 120-layer overlay on this older kernel: {error}");
            return;
        }
        Err(error) => panic!("120-layer production mount: {error}"),
    };
    assert_eq!(fs::read_to_string(root.join("winner")).unwrap(), "0", "first lower remains topmost");
    for number in 0..120 {
        assert_eq!(fs::read_to_string(root.join(format!("layer-{number}"))).unwrap(), number.to_string());
    }
    fs::write(root.join("layer-119"), "copied-up").unwrap();
    let original = paths.layers.join(format!("{:064x}", 119)).join("layer-119");
    assert_eq!(fs::read_to_string(original).unwrap(), "119");
    assert_eq!(fs::read_to_string(paths.containers.join("many/upper/layer-119")).unwrap(), "copied-up");
    let mountinfo = fs::read_to_string("/proc/thread-self/mountinfo").unwrap();
    let rows: Vec<_> = mountinfo.lines().filter(|line| line.split_whitespace().nth(4) == root.to_str()).collect();
    assert_eq!(rows.len(), 1);
    // The kernel may disable index when the scratch filesystem cannot
    // export file handles. This implementation leaves that policy to it.
    assert!(!rows[0].contains("metacopy=on") && !rows[0].contains("redirect_dir=on"));
    let croot = sys::cstr(&root).unwrap();
    // SAFETY: this is our own private overlay, and every file was closed.
    assert_eq!(unsafe { libc::umount2(croot.as_ptr(), 0) }, 0, "{}", io::Error::last_os_error());
    let root = container::mount_rootfs(&paths, "many", spec.layers).unwrap();
    assert_eq!(fs::read_to_string(root.join("layer-119")).unwrap(), "copied-up");
    assert_eq!(unsafe { libc::umount2(croot.as_ptr(), 0) }, 0, "{}", io::Error::last_os_error());
    println!("PASS real overlay: 120 unique layers, duplicate layer, precedence, full-data copyup and remount");
}

#[test]
fn legacy_mount_remains_usable_and_rejects_a_truncated_option_list() {
    if !child("legacy_mount_remains_usable_and_rejects_a_truncated_option_list") { return; }
    let scratch = Scratch::new();
    let base = scratch.0.join("short");
    for name in ["lower", "upper", "work", "root"] { fs::create_dir_all(base.join(name)).unwrap(); }
    fs::write(base.join("lower/kept"), "legacy").unwrap();
    std::env::set_current_dir(&base).unwrap();
    legacy(&["lower"], &base.join("upper"), &base.join("work"), &base.join("root")).unwrap();
    assert_eq!(fs::read_to_string(base.join("root/kept")).unwrap(), "legacy");
    let target = sys::cstr(base.join("root")).unwrap();
    assert_eq!(unsafe { libc::umount2(target.as_ptr(), 0) }, 0, "{}", io::Error::last_os_error());

    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let name = "a".repeat(64);
    let too_many = vec![name.as_str(); page / 65 + 1];
    let error = legacy(&too_many, &base.join("upper"), &base.join("work"), &base.join("root")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(error.to_string().contains("byte limit"));
    assert!(!unsupported(&io::Error::from_raw_os_error(libc::EPERM)));
    assert!(!unsupported(&io::Error::from_raw_os_error(libc::ENOENT)));
    // Long data directories exceed fsconfig's string limit even for one
    // layer. Use directory FDs where available, or the working legacy path.
    let deep = scratch.0.join("long").join("a".repeat(90)).join("b".repeat(90)).join("c".repeat(90));
    let target = scratch.0.join("long/root");
    for path in [deep.join("upper"), deep.join("work"), target.clone()] { fs::create_dir_all(path).unwrap(); }
    assert!(sys::cstr(deep.join("upper")).unwrap().as_bytes().len() >= 256);
    mount(&["lower"], &deep.join("upper"), &deep.join("work"), &target).unwrap();
    assert_eq!(fs::read_to_string(target.join("kept")).unwrap(), "legacy");
    let info = fs::read_to_string("/proc/thread-self/mountinfo").unwrap();
    assert!(info.lines().any(|line| line.split_whitespace().nth(4) == target.to_str() && line.contains(deep.join("upper").to_str().unwrap())));
    let target = sys::cstr(target).unwrap();
    assert_eq!(unsafe { libc::umount2(target.as_ptr(), 0) }, 0, "{}", io::Error::last_os_error());
    println!("PASS real overlay: legacy mount, oversized-options rejection and long data directory");
}
