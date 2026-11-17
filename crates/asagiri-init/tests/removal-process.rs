//! The removal worker must fork from a single-threaded agent. This target has
//! no libtest harness; even libtest's --test-threads=1 uses a second thread.
//! Exercise the actual agent modules without starting an agent or container.

#[cfg(target_os = "linux")]
#[path = "../src/layer_tar.rs"]
#[allow(dead_code, unused_imports)] // This harness only runs removal, but includes the agent's layer code.
mod layer_tar;

#[cfg(target_os = "linux")]
#[allow(dead_code)] // Other included agent entrypoints are not run by this test.
#[expect(unused_imports, reason = "harness=false keeps cfg(test) modules but omits their libtest bodies")]
mod linux {
    include!("../src/linux/mod.rs");

    // Restore normal warnings for the standalone regression itself.
    #[warn(dead_code, unused_imports)]
    pub(super) fn removal_process_regression() {
        use std::os::unix::fs::DirBuilderExt;
        use std::time::{Duration, Instant};
        use container::{Paths, begin_root_cleanup};
        use removal::Removals;

        fn single_thread() {
            assert_eq!(std::fs::read_dir("/proc/self/task").unwrap().count(), 1,
                "the production removal worker must fork from a single-threaded process");
        }

        fn run_worker(removals: &mut Removals, paths: &Paths, now: Instant) -> i32 {
            let mut worker = None;
            removals.start(now, |id| {
                single_thread();
                let pid = begin_root_cleanup(paths, id, true)?;
                worker = Some(pid);
                Ok(pid)
            });
            let worker = worker.expect("removal worker was not started");
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let mut status = 0;
                // SAFETY: observe only the disposable worker owned by removals.
                let observed = unsafe { libc::waitpid(worker, &mut status, libc::WNOHANG) };
                if observed == worker {
                    let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) }
                        else { 128 + libc::WTERMSIG(status) };
                    // Clear ownership before assertions: this PID was reaped
                    // and must never be signalled by failure cleanup.
                    assert!(removals.reaped(worker, code, now));
                    assert!(libc::WIFEXITED(status), "removal worker was signalled");
                    eprintln!("removal worker exit={code} reserved={}", removals.ids().count());
                    return code;
                }
                if observed < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ECHILD) {
                        assert!(removals.reaped(worker, 1, now));
                    }
                    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
                }
                // Removals owns failure cleanup too: a timed-out worker is
                // killed and reaped before the fixture removes its files.
                assert!(Instant::now() < deadline, "removal worker timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
        }

        single_thread();
        // There is no competing reaper, and waitpid must retain child
        // ownership until this test consumes its status.
        let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        // SAFETY: query the disposition into a valid output without changing it.
        assert_eq!(unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr()) }, 0);
        // SAFETY: successful sigaction initialized the value.
        let action = unsafe { action.assume_init() };
        assert_eq!(action.sa_sigaction, libc::SIG_DFL);
        assert_eq!(action.sa_flags & libc::SA_NOCLDWAIT, 0);

        let root = std::env::temp_dir().join(format!("asagiri-removal-process-{}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let fixture = Fixture(root);
        let paths = Paths {
            layers: fixture.0.join("layers"),
            containers: fixture.0.join("containers"),
            cgroup: fixture.0.join("cgroup"),
        };
        std::fs::create_dir(&paths.containers).unwrap();
        let base = paths.containers.join("victim");
        std::fs::write(&base, b"preserve this failed removal").unwrap();
        // Drop order keeps the private tree alive until all workers are joined.
        let mut removals = Removals::default();
        let now = Instant::now();
        removals.enqueue("victim");
        assert_eq!(run_worker(&mut removals, &paths, now), 1);
        assert!(removals.contains("victim"));
        assert_eq!(std::fs::read(&base).unwrap(), b"preserve this failed removal");
        std::fs::remove_file(&base).unwrap();
        std::fs::create_dir_all(base.join("rootfs")).unwrap();
        std::fs::write(base.join("payload"), b"remove this repaired tree").unwrap();
        assert_eq!(run_worker(&mut removals, &paths, now + Duration::from_secs(1)), 0);
        assert!(removals.is_empty());
        assert!(!base.exists());
        // Reclaim a real volume tree in the worker while immediately reusing
        // its public name. Only the detached old tree may disappear.
        let volume_root = fixture.0.join("volumes");
        std::fs::create_dir_all(volume_root.join("reuse/_data")).unwrap();
        for i in 0..1000 {
            std::fs::write(volume_root.join(format!("reuse/_data/{i}")), b"old").unwrap();
        }
        let mut volumes = volumes::Volumes::new(&volume_root).unwrap();
        volumes.remove("reuse").unwrap();
        std::fs::create_dir_all(volume_root.join("reuse/_data")).unwrap();
        std::fs::write(volume_root.join("reuse/_data/new"), b"new").unwrap();
        volumes.start(Instant::now());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut status = 0;
            // SAFETY: the root worker above was reaped; this is the only child.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid > 0 {
                let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 1 };
                assert!(volumes.reaped(pid, code, Instant::now()));
                assert_eq!(code, 0);
                break;
            }
            assert!(pid >= 0, "volume worker disappeared");
            assert!(Instant::now() < deadline, "volume worker timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(std::fs::read(volume_root.join("reuse/_data/new")).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&volume_root).unwrap().count(), 1);
        println!("PASS volume-process: asynchronous reclamation preserves a recreated volume");
        single_thread();
        let mut status = 0;
        // SAFETY: this process owns every child; unlike proc's children
        // file, ECHILD is available on the packaged guest kernel too.
        assert_eq!(unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
        println!("PASS removal-process: single-threaded worker reports deletion failure, reserves its ID, and completes repaired retry");
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux::removal_process_regression();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("SKIP removal process regression: Linux only");
}
