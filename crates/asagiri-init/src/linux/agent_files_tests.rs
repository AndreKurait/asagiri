use super::*;
use std::ffi::CString;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

struct Fixture {
    path: PathBuf,
    child: Option<Child>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if child.try_wait().ok().flatten().is_none() { let _ = child.kill(); }
            let _ = child.wait();
        }
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            eprintln!("mounted file-view fixture cleanup: {error}");
        }
    }
}

#[test]
fn running_view_sees_private_mounts_and_rejects_reaped_entries() {
    // CI runs Linux tests in its privileged disposable compiler container.
    // An ordinary unprivileged development checkout cannot mount this fixture.
    // SAFETY: geteuid has no arguments and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        assert!(std::env::var_os("ASAGIRI_REQUIRE_MOUNTED_FILE_VIEW_TEST").is_none(),
            "CI must execute the real mounted file-view fixture as root");
        eprintln!("SKIP mounted file view: requires the privileged Linux test container");
        return;
    }
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!("asagiri-files-view-{}-{nonce}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    let mut fixture = Fixture { path, child: None };
    let root = fixture.path.join("rootfs");
    let volume = fixture.path.join("volume");
    let bind = fixture.path.join("bind");
    for dir in [&root, &volume, &bind] { std::fs::create_dir(dir).unwrap(); }
    for dir in ["data", "bind", "plain"] { std::fs::create_dir(root.join(dir)).unwrap(); }
    std::fs::write(volume.join("volume.txt"), b"volume payload").unwrap();
    std::fs::write(bind.join("bind.txt"), b"bind payload").unwrap();
    let outside = fixture.path.join("host-proof");
    std::fs::write(&outside, b"outside stays private").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    std::os::unix::fs::symlink("/data/volume.txt", root.join("volume-link")).unwrap();

    // The child needs only a shell and sleep. Bind its existing runtime
    // directories read-only in its private namespace; no fixture mounts are
    // installed in the parent namespace.
    let mut mounts = Vec::new();
    for name in ["bin", "lib", "lib64", "usr"] {
        let source = PathBuf::from(format!("/{name}"));
        if !source.is_dir() { continue; }
        let target = root.join(name);
        std::fs::create_dir(&target).unwrap();
        mounts.push((sys::cstr(&source).unwrap(), sys::cstr(&target).unwrap(), true));
    }
    mounts.push((sys::cstr(&volume).unwrap(), sys::cstr(&root.join("data")).unwrap(), false));
    mounts.push((sys::cstr(&bind).unwrap(), sys::cstr(&root.join("bind")).unwrap(), true));
    let child_root = sys::cstr(&root).unwrap();
    let slash = CString::new("/").unwrap();
    let mut command = Command::new("/bin/sh");
    command.args(["-ec", "printf '%s' runtime-payload > /plain/runtime.txt; printf 'ready\\n'; exec /bin/sleep 60"])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // SAFETY: after fork the callback only performs Linux syscalls on
    // parent-prepared C strings; it allocates nothing and acquires no locks.
    unsafe {
        command.pre_exec(move || {
            if libc::unshare(libc::CLONE_NEWNS) != 0 { return Err(io::Error::last_os_error()); }
            if libc::mount(std::ptr::null(), slash.as_ptr(), std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) != 0 {
                return Err(io::Error::last_os_error());
            }
            for (source, target, readonly) in &mounts {
                if libc::mount(source.as_ptr(), target.as_ptr(), std::ptr::null(),
                    libc::MS_BIND, std::ptr::null()) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if *readonly && libc::mount(std::ptr::null(), target.as_ptr(), std::ptr::null(),
                    libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY, std::ptr::null()) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::chroot(child_root.as_ptr()) != 0 || libc::chdir(slash.as_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("create the private mounted root");
    let pid = child.id() as libc::pid_t;
    let output = child.stdout.take().unwrap();
    fixture.child = Some(child);
    let mut ready = libc::pollfd { fd: output.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: poll one owned output pipe for bounded fixture readiness.
    assert_eq!(unsafe { libc::poll(&mut ready, 1, 5000) }, 1, "fixture did not reach its private root");
    let mut line = String::new();
    BufReader::new(output).read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    assert_ne!(std::fs::read_link(format!("/proc/{pid}/ns/mnt")).unwrap(),
        std::fs::read_link("/proc/self/ns/mnt").unwrap());

    let mut procs = Procs::new();
    procs.insert("fixture".into(), new_proc(Process::test_process("fixture", pid), 0, 0));
    let view = running_rootfs(&procs, "fixture").unwrap();
    let mut bytes = Vec::new();
    for (directory, name, expected) in [
        ("/data", "volume.txt", b"volume payload".as_slice()),
        ("/bind", "bind.txt", b"bind payload".as_slice()),
        ("/plain", "runtime.txt", b"runtime-payload".as_slice()),
    ] {
        let entries = super::super::fsview::list(&view, directory).unwrap();
        assert_eq!(entries.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), [name]);
        let file = format!("{directory}/{name}");
        let meta = super::super::fsview::stat(&view, &file).unwrap();
        assert_eq!(meta.mode & libc::S_IFMT, libc::S_IFREG);
        assert_eq!(meta.size, expected.len() as u64);
        super::super::fsview::read(&view, &file, 0, 1024, &mut bytes).unwrap();
        assert_eq!(bytes, expected);
        super::super::fsview::read(&view, &file, 2, 4, &mut bytes).unwrap();
        assert_eq!(bytes, &expected[2..6]);
    }
    // Negative control: the agent's original root still sees empty mount
    // targets, demonstrating that the test requires the child's mounted view.
    for path in ["/data", "/bind"] {
        assert!(super::super::fsview::list(&root, path).unwrap().is_empty());
    }
    super::super::fsview::read(&view, "/volume-link", 0, 1024, &mut bytes).unwrap();
    assert_eq!(bytes, b"volume payload");
    assert!(super::super::fsview::read(&view, "/escape", 0, 1024, &mut bytes).is_err());
    assert!(super::super::fsview::read(&view, "/../../host-proof", 0, 1024, &mut bytes).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside stays private");

    let mut exec = Process::test_process("exec", pid);
    exec.container = Some("fixture".into());
    procs.insert("exec".into(), new_proc(exec, 0, 0));
    assert!(running_rootfs(&procs, "exec").is_none());
    assert!(running_rootfs(&procs, "absent").is_none());
    procs.get_mut("fixture").unwrap().exited = Some((0, 0));
    assert!(running_rootfs(&procs, "fixture").is_none(), "reaped entries must not expose another root");
    let path = fixture.path.clone();
    drop(fixture);
    assert!(!path.exists());
    eprintln!("PASS mounted file view: volume, readonly bind, runtime files, offsets, in-root symlinks, reaped/exec denial; original root shows empty mounts");
}
