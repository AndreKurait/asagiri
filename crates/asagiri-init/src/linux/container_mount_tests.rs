use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

const CHILD: &str = "ASAGIRI_VOLUME_OWNER_TEST_CHILD";
const REQUIRED: &str = "ASAGIRI_REQUIRE_VOLUME_OWNER_TEST";

struct Fixture {
    base: PathBuf,
    paths: Paths,
    mounts: Vec<PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!("asagiri-volume-owner-{}", std::process::id()));
        fs::create_dir(&base).unwrap();
        let paths = Paths { layers: base.join("layers"), containers: base.join("containers"), cgroup: base.join("cgroup") };
        fs::create_dir(&paths.layers).unwrap();
        let lower = paths.layers.join(format!("{:064x}", 1));
        fs::create_dir(&lower).unwrap();
        for name in ["from-image", "empty-image", "no-copy", "host-scope/from-image"] {
            let dir = lower.join(name);
            fs::create_dir_all(&dir).unwrap();
            if name != "empty-image" { fs::write(dir.join("image.txt"), b"image contents").unwrap(); }
            let path = sys::cstr(&dir).unwrap();
            // SAFETY: only this fixture's new image directory is changed.
            assert_eq!(unsafe { libc::chown(path.as_ptr(), 10007, 10008) }, 0);
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o2751)).unwrap();
        }
        fs::write(paths.layers.join(format!("{:064x}.done", 1)), b"").unwrap();
        Self { base, paths, mounts: Vec::new() }
    }

    fn root(&mut self) -> PathBuf {
        std::env::set_current_dir(&self.paths.layers).unwrap();
        let digests = vec![format!("sha256:{:064x}", 1)];
        let mut frame = Vec::new();
        asagiri_proto::encode_copy(&mut frame, asagiri_proto::copy_op::OUT, "fixture", "/", 0, &digests, "", &[]);
        let asagiri_proto::Msg::Copy(spec) = asagiri_proto::decode(&frame).unwrap() else { panic!("copy spec"); };
        let root = mount_rootfs(&self.paths, "fixture", spec.layers).unwrap();
        self.mounts.push(root.clone());
        root
    }

    fn volume(&mut self, name: &str) -> PathBuf {
        let path = self.base.join(name);
        fs::create_dir(&path).unwrap();
        sys::mount(c"tmpfs", &sys::cstr(&path).unwrap(), c"tmpfs", 0,
            Some(c"size=1m,uid=10001,gid=10050,mode=2750")).unwrap();
        self.mounts.push(path.clone());
        path
    }

    fn apply(&mut self, root: BorrowedFd<'_>, image: BorrowedFd<'_>, root_path: &Path,
        source: &Path, target: &str, copy_up: bool) {
        let m = Mount {
            source: Some(sys::cstr(source).unwrap()), target: CString::new(target).unwrap(),
            ro: false, dir: true, copy_up, tmpfs_opts: None, tmpfs_flags: 0,
        };
        apply_mount(root, Some(image), &m).unwrap();
        self.mounts.push(root_path.join(target));
    }

    fn unmount_last(&mut self) {
        let path = self.mounts.last().unwrap();
        let name = sys::cstr(path).unwrap();
        // SAFETY: unmount only the most recent fixture mount, in its private namespace.
        assert_eq!(unsafe { libc::umount2(name.as_ptr(), 0) }, 0, "{}: {}", path.display(), io::Error::last_os_error());
        self.mounts.pop();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir("/");
        for path in self.mounts.iter().rev() { let _ = sys::umount_lazy(path); }
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn metadata(path: &Path) -> (u32, u32, u32) {
    let meta = fs::metadata(path).unwrap();
    (meta.uid(), meta.gid(), meta.mode() & 0o7777)
}

#[test]
fn tmpfs_ownership_survives_copyup_restart_and_nested_binds() {
    if std::env::var_os(CHILD).is_none() {
        // CI supplies a disposable privileged Linux compiler container.
        // SAFETY: geteuid has no arguments and cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            assert!(std::env::var_os(REQUIRED).is_none(), "CI must run the real volume mounts as root");
            eprintln!("SKIP real volume mounts: requires a private mount namespace");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "linux::container::volume_mount_tests::tmpfs_ownership_survives_copyup_restart_and_nested_binds",
                "--nocapture", "--test-threads=1"])
            .env(CHILD, "1").output().unwrap();
        assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("1 passed") && stdout.contains("PASS real volume mounts"), "real mount fixture was not executed");
        return;
    }
    // The selected test runs in a separate process; these operations cannot
    // change the parent test runner's mount namespace or working directory.
    // SAFETY: unshare only this disposable process's mount/filesystem context.
    assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNS) }, 0, "{}", io::Error::last_os_error());
    sys::remount(c"/", libc::MS_REC | libc::MS_PRIVATE).unwrap();
    let mut f = Fixture::new();
    let cases = [
        ("missing", "fresh/deep", true, false),
        ("real", "from-image", true, true),
        ("empty", "empty-image", true, true),
        ("nocopy", "no-copy", false, false),
        ("nested-missing", "host-scope/missing/deep", true, false),
        ("nested-real", "host-scope/from-image", true, true),
    ];
    let volumes: Vec<_> = cases.iter().map(|(name, _, _, _)| f.volume(name)).collect();
    let persistent = f.mounts.len();
    let host = f.base.join("host");
    fs::create_dir(&host).unwrap();
    fs::write(host.join("keep.txt"), b"host contents").unwrap();
    let host_metadata = metadata(&host);
    for round in 0..2 {
        let root_path = f.root();
        {
            std::env::set_current_dir(&root_path).unwrap();
            let root = inroot::open_root(&root_path).unwrap();
            let image = image_tree().expect("clone the image view without child mounts");
            f.apply(root.as_fd(), image.as_fd(), &root_path, &host, "host-scope", false);
            for ((name, target, copy_up, copied), volume) in cases.iter().zip(&volumes) {
                f.apply(root.as_fd(), image.as_fd(), &root_path, volume, target, *copy_up);
                let expected = if *copied { (10007, 10008, 0o2751) } else { (10001, 10050, 0o2750) };
                assert_eq!(metadata(&root_path.join(target)), expected, "{name} round {round}");
                assert_eq!(metadata(volume), expected, "{name} source round {round}");
                if *copied && *name != "empty" {
                    assert_eq!(fs::read(root_path.join(target).join("image.txt")).unwrap(), b"image contents");
                } else {
                    assert_eq!(fs::read_dir(volume).unwrap().count(), 0, "{name} stays empty");
                }
            }
            let generated = inroot::lookup(image.as_fd(), Path::new("fresh/deep")).unwrap();
            assert!(is_generated_mountpoint(&generated).unwrap(), "own mountpoint marker persists");
            assert!(inroot::lookup(image.as_fd(), Path::new("host-scope/missing")).is_err(),
                "looking beneath a host bind must not create directories in the image view");
            let host_root = inroot::open_root(&host).unwrap();
            for target in ["missing/deep", "from-image"] {
                let at = inroot::lookup(host_root.as_fd(), Path::new(target)).unwrap();
                assert!(!is_generated_mountpoint(&at).unwrap(), "host directories must never be marked");
                assert_eq!(metadata(&host.join(target)), (0, 0, 0o755));
                assert_eq!(fs::read_dir(host.join(target)).unwrap().count(), 0);
            }
            assert_eq!(metadata(&host), host_metadata);
            assert_eq!(fs::read(host.join("keep.txt")).unwrap(), b"host contents");
        }
        std::env::set_current_dir(&f.base).unwrap();
        while f.mounts.len() > persistent { f.unmount_last(); }
    }
    while !f.mounts.is_empty() { f.unmount_last(); }
    println!("PASS real volume mounts: explicit uid/gid/mode, two overlay mounts, real image copyup, nocopy, nested binds and host metadata");
}
