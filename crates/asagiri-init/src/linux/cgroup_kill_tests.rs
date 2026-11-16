use super::*;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

fn directory(parent: &Path) -> io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = parent.join(format!("asagiri-cgroup-stop-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::DirBuilder::new().mode(0o700).create(&path)?;
    Ok(path)
}

struct Sleeper {
    child: Child,
    fd: OwnedFd,
}

impl Sleeper {
    fn new() -> Self {
        let mut child = Command::new("/bin/sleep").arg("300")
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let fd = match pidfd(child.id() as libc::pid_t) {
            Ok(fd) => fd,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("open an owned child's pidfd: {error}");
            }
        };
        Self { child, fd }
    }

    fn pid(&self) -> libc::pid_t { self.child.id() as libc::pid_t }

    fn alive(&mut self) {
        assert!(self.child.try_wait().unwrap().is_none(), "an outside or retained child was killed");
    }

    fn killed(&mut self) {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.signal(), Some(libc::SIGKILL));
                return;
            }
            before(deadline).expect("the owned child was not killed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn stop(&mut self) {
        // Child retains the wait status after reaping. This cleanup only ever
        // addresses the child created here, independently of the code tested.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) { self.stop(); }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self { Self(directory(&std::env::temp_dir()).unwrap()) }
}
impl Drop for Scratch {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

mod unit {
    use super::*;

    #[test]
    fn ordinary_files_cannot_supply_process_ids_or_kill_controls() {
        let scratch = Scratch::new();
        let mut outside = Sleeper::new();
        fs::write(scratch.0.join("cgroup.procs"), outside.pid().to_string()).unwrap();
        fs::write(scratch.0.join("cgroup.kill"), "preserve").unwrap();
        assert_eq!(kill(&scratch.0).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read_to_string(scratch.0.join("cgroup.kill")).unwrap(), "preserve");
        outside.alive();
    }

    #[test]
    fn missing_groups_are_idempotent_and_symlinked_groups_are_refused() {
        let scratch = Scratch::new();
        let missing = scratch.0.join("missing");
        kill(&missing).unwrap();
        assert!(!missing.exists());
        let target = scratch.0.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("cgroup.kill"), "preserve").unwrap();
        symlink(&target, scratch.0.join("link")).unwrap();
        assert!(kill(&scratch.0.join("link")).is_err());
        assert_eq!(fs::read_to_string(target.join("cgroup.kill")).unwrap(), "preserve");
    }

    #[test]
    fn event_reads_fail_closed_on_missing_ambiguous_or_invalid_state() {
        assert!(!event("populated 0\nfrozen 1\n", "populated").unwrap());
        assert!(event("populated 0\nfrozen 1\n", "frozen").unwrap());
        for text in ["", "populated 2\n", "populated 1 extra\n", "populated 0\npopulated 1\n", "populated\n"] {
            assert_eq!(event(text, "populated").unwrap_err().kind(), io::ErrorKind::InvalidData, "{text:?}");
        }
    }

    #[test]
    fn only_unavailable_kernel_controls_permit_the_pidfd_fallback() {
        for error in [libc::ENOENT, libc::EOPNOTSUPP] {
            assert!(fallback_allowed(&io::Error::from_raw_os_error(error)));
        }
        for error in [libc::EACCES, libc::EPERM, libc::EROFS, libc::EINVAL, libc::EIO, libc::ENOTDIR] {
            assert!(!fallback_allowed(&io::Error::from_raw_os_error(error)), "{error}");
        }
    }

    #[test]
    fn a_reaped_pidfd_does_not_signal_another_owned_process() {
        let mut victim = Sleeper::new();
        let mut outside = Sleeper::new();
        signal(&victim.fd).unwrap();
        victim.killed();
        signal(&victim.fd).unwrap();
        outside.alive();
    }
}

struct Cgroups {
    root: PathBuf,
    children: Vec<Sleeper>,
    cleaned: bool,
}

fn remove_groups(path: &Path) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() { remove_groups(&entry.path())?; }
    }
    fs::remove_dir(path)
}

impl Cgroups {
    fn new() -> Option<Self> {
        let result = directory(Path::new("/sys/fs/cgroup")).and_then(|root| {
            if let Err(error) = Group::open(&root) {
                let _ = fs::remove_dir(&root);
                return Err(error);
            }
            Ok(Self { root, children: Vec::new(), cleaned: false })
        });
        match result {
            Ok(fixture) => Some(fixture),
            Err(error) => {
                assert!(std::env::var_os("ASAGIRI_REQUIRE_CGROUP_KILL_TEST").is_none(),
                    "required real cgroup stop test needs a writable cgroup-v2 mount: {error}");
                eprintln!("real cgroup stop test unavailable: {error}");
                None
            }
        }
    }

    fn group(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir(&path).unwrap();
        path
    }

    fn spawn(&mut self, group: &Path) -> usize {
        let index = self.children.len();
        self.children.push(Sleeper::new());
        fs::write(group.join("cgroup.procs"), self.children[index].pid().to_string()).unwrap();
        index
    }

    fn outside(&mut self) -> usize {
        let path = self.group("outside");
        self.spawn(&path)
    }

    fn finish(mut self) {
        for child in &mut self.children { child.stop(); }
        kill(&self.root).unwrap();
        remove_groups(&self.root).unwrap();
        assert!(!self.root.exists());
        self.cleaned = true;
    }
}

impl Drop for Cgroups {
    fn drop(&mut self) {
        if self.cleaned { return; }
        for child in &mut self.children { child.stop(); }
        let _ = kill(&self.root);
        if let Err(error) = remove_groups(&self.root) {
            eprintln!("owned cgroup fixture cleanup {}: {error}", self.root.display());
        }
    }
}

struct Overmount(CString);

impl Overmount {
    fn new(source: &Path, target: &Path) -> Self {
        let source = CString::new(source.as_os_str().as_bytes()).unwrap();
        let target = CString::new(target.as_os_str().as_bytes()).unwrap();
        // SAFETY: both paths belong to this disposable fixture. The test runs
        // only in the isolated privileged CI container's mount namespace.
        assert_eq!(unsafe {
            libc::mount(source.as_ptr(), target.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null())
        }, 0, "overmount an owned test control: {}", io::Error::last_os_error());
        Self(target)
    }
}

impl Drop for Overmount {
    fn drop(&mut self) {
        // SAFETY: detach only the bind mount made by this guard.
        if unsafe { libc::umount2(self.0.as_ptr(), libc::MNT_DETACH) } != 0 {
            eprintln!("remove test control overmount: {}", io::Error::last_os_error());
        }
    }
}

mod live {
    use super::*;

    #[test]
    fn overmounted_regular_and_fifo_controls_are_refused_without_signals_or_writes() {
        let Some(mut f) = Cgroups::new() else { return };
        let scratch = Scratch::new();
        let outside = f.outside();
        for (name, control, fifo, force_pidfds) in [
            ("kill-file", "cgroup.kill", false, false),
            ("procs-file", "cgroup.procs", false, true),
            ("procs-fifo", "cgroup.procs", true, true),
        ] {
            let path = f.group(name);
            let victim = f.spawn(&path);
            let source = scratch.0.join(name);
            let contents = f.children[outside].pid().to_string();
            if fifo {
                let source_c = CString::new(source.as_os_str().as_bytes()).unwrap();
                // SAFETY: creates a FIFO only inside this test's scratch directory.
                assert_eq!(unsafe { libc::mkfifo(source_c.as_ptr(), 0o600) }, 0);
            } else {
                fs::write(&source, &contents).unwrap();
            }
            let mounted = Overmount::new(&source, &path.join(control));
            assert_eq!(kill_mode(&path, force_pidfds).unwrap_err().kind(), io::ErrorKind::InvalidInput);
            f.children[victim].alive();
            f.children[outside].alive();
            assert_eq!(Group::open(&path).unwrap().read(c"cgroup.freeze").unwrap().trim(), "0");
            if !fifo { assert_eq!(fs::read_to_string(&source).unwrap(), contents); }
            drop(mounted);
        }
        f.finish();
    }

    #[test]
    fn modern_kernel_kill_empties_nested_groups_and_preserves_an_outside_child() {
        let Some(mut f) = Cgroups::new() else { return };
        let group = f.group("target");
        if !group.join("cgroup.kill").exists() {
            assert!(std::env::var_os("ASAGIRI_REQUIRE_CGROUP_KILL_TEST").is_none(),
                "required modern cgroup.kill control is missing");
            f.finish();
            return;
        }
        let inner = f.group("target/inner");
        let deepest = f.group("target/inner/deepest");
        let outside = f.outside();
        let victims = [f.spawn(&group), f.spawn(&inner), f.spawn(&deepest)];
        kill(&group).unwrap();
        for index in victims { f.children[index].killed(); }
        assert!(!Group::open(&group).unwrap().event("populated").unwrap());
        f.children[outside].alive();
        f.finish();
    }

    #[test]
    fn forced_pidfd_fallback_handles_nested_groups_and_more_than_one_batch() {
        let Some(mut f) = Cgroups::new() else { return };
        let group = f.group("target");
        let inner = f.group("target/inner");
        let outside = f.outside();
        let mut victims = vec![f.spawn(&group)];
        for _ in 0..35 { victims.push(f.spawn(&inner)); }
        kill_mode(&group, true).unwrap();
        for index in victims { f.children[index].killed(); }
        let group = Group::open(&group).unwrap();
        assert!(!group.event("populated").unwrap());
        assert_eq!(group.read(c"cgroup.freeze").unwrap().trim(), "0");
        f.children[outside].alive();
        f.finish();
    }

    #[test]
    fn fallback_preserves_an_already_paused_groups_requested_state() {
        let Some(mut f) = Cgroups::new() else { return };
        let path = f.group("target");
        let outside = f.outside();
        let victim = f.spawn(&path);
        let group = Group::open(&path).unwrap();
        group.write(c"cgroup.freeze", b"1").unwrap();
        wait(&group, "frozen", true, Instant::now() + WAIT).unwrap();
        kill_mode(&path, true).unwrap();
        f.children[victim].killed();
        assert_eq!(group.read(c"cgroup.freeze").unwrap().trim(), "1");
        f.children[outside].alive();
        f.finish();
    }

    #[test]
    fn expired_stop_restores_the_freezer_and_retains_live_processes() {
        let Some(mut f) = Cgroups::new() else { return };
        let path = f.group("target");
        let victim = f.spawn(&path);
        let group = Group::open(&path).unwrap();
        let error = kill_open(&group, true, Instant::now() - Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(group.read(c"cgroup.freeze").unwrap().trim(), "0");
        f.children[victim].alive();
        f.finish();
    }

    #[test]
    fn membership_is_read_again_after_process_handles_are_pinned() {
        let Some(mut f) = Cgroups::new() else { return };
        let from = f.group("from");
        let to = f.group("to");
        let victim = f.spawn(&from);
        let pid = f.children[victim].pid();
        let group = Group::open(&from).unwrap();
        assert!(group.processes().unwrap().contains(&pid));
        let pinned = vec![(pid, pidfd(pid).unwrap())];
        fs::write(to.join("cgroup.procs"), pid.to_string()).unwrap();
        assert_eq!(signal_pinned_members(&group, pinned).unwrap(), 0);
        f.children[victim].alive();
        assert!(Group::open(&to).unwrap().processes().unwrap().contains(&pid));
        f.finish();
    }

    #[test]
    fn fallback_kills_a_threaded_domain_without_reading_child_process_lists() {
        let Some(mut f) = Cgroups::new() else { return };
        let path = f.group("domain");
        let threaded = f.group("domain/threaded");
        fs::write(threaded.join("cgroup.type"), "threaded").unwrap();
        let outside = f.outside();
        let victim = f.spawn(&path);
        fs::write(threaded.join("cgroup.threads"), f.children[victim].pid().to_string()).unwrap();
        let domain = Group::open(&path).unwrap();
        assert_eq!(domain.read(c"cgroup.type").unwrap().trim(), "domain threaded");
        assert_eq!(Group::open(&threaded).unwrap().processes().unwrap_err().raw_os_error(), Some(libc::EOPNOTSUPP));
        kill_mode(&path, true).unwrap();
        f.children[victim].killed();
        f.children[outside].alive();
        f.finish();
    }

    #[test]
    fn a_populated_threaded_child_is_refused_without_freezing_or_signaling() {
        let Some(mut f) = Cgroups::new() else { return };
        let domain = f.group("domain");
        let path = f.group("domain/threaded");
        fs::write(path.join("cgroup.type"), "threaded").unwrap();
        let victim = f.spawn(&domain);
        fs::write(path.join("cgroup.threads"), f.children[victim].pid().to_string()).unwrap();
        assert_eq!(kill_mode(&path, true).unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(Group::open(&path).unwrap().read(c"cgroup.freeze").unwrap().trim(), "0");
        f.children[victim].alive();
        f.finish();
    }
}
