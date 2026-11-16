//! Run the actual cgroup setup in a fresh, private cgroup namespace. The
//! standalone process must be its container's PID 1; Cargo only compiles it.

#[cfg(target_os = "linux")]
#[path = "../src/layer_tar.rs"]
#[allow(dead_code, unused_imports)]
mod layer_tar;

#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[expect(unused_imports, reason = "harness=false includes agent modules without running their libtest bodies")]
mod linux {
    include!("../src/linux/mod.rs");

    // Keep the same regression executable usable against the older helper,
    // whose unit return silently discarded all setup errors.
    trait SetupOutcome {
        fn error(&self) -> Option<String>;
    }

    impl SetupOutcome for () {
        fn error(&self) -> Option<String> { None }
    }

    impl SetupOutcome for std::io::Result<()> {
        fn error(&self) -> Option<String> { self.as_ref().err().map(ToString::to_string) }
    }

    pub(super) fn regression() {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::path::{Path, PathBuf};
        use std::time::{Duration, Instant};

        struct Child {
            pid: libc::pid_t,
            cgroup: PathBuf,
        }
        impl Drop for Child {
            fn drop(&mut self) {
                if self.pid > 0 {
                    let _ = std::fs::write(self.cgroup.join("cgroup.freeze"), "0");
                    // SAFETY: this unreaped child is exclusively owned here.
                    unsafe {
                        libc::kill(self.pid, libc::SIGKILL);
                        libc::waitpid(self.pid, std::ptr::null_mut(), 0);
                    }
                }
            }
        }

        fn controllers(path: &Path) -> Vec<String> {
            std::fs::read_to_string(path.join("cgroup.subtree_control")).unwrap()
                .split_whitespace().map(str::to_owned).collect()
        }

        fn required(path: &Path) {
            let enabled = controllers(path);
            for name in ["cpu", "memory", "pids"] {
                assert!(enabled.iter().any(|value| value == name), "{path:?}: {enabled:?}");
            }
        }

        assert_eq!(std::process::id(), 1, "use an isolated container, with this executable as PID 1");
        assert_eq!(std::fs::read_dir("/proc/self/task").unwrap().count(), 1);
        assert_eq!(std::fs::read_to_string("/proc/self/cgroup").unwrap().trim(), "0::/");
        let root = Path::new("/sys/fs/cgroup");
        assert_eq!(std::fs::read_to_string(root.join("cgroup.type")).unwrap().trim(), "domain",
            "the hierarchy's real root must never be used by this test");
        // Change only the flags of this container's existing cgroup mount.
        sys::mount(c"none", c"/", c"", libc::MS_REC | libc::MS_PRIVATE, None).unwrap();
        sys::mount(c"none", c"/sys/fs/cgroup", c"",
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None).unwrap();
        assert!(controllers(root).is_empty(), "the regression needs a fresh namespace root");
        assert_eq!(std::fs::read_to_string(root.join("cgroup.procs")).unwrap().trim(), "1");

        // A newly arriving exec can occupy this root after host_cgroup takes
        // its process snapshot. The kernel then rejects domain delegation.
        let blocked = root.join("occupied-parent");
        std::fs::create_dir(&blocked).unwrap_or_else(|error| {
            panic!("the private cgroup mount must be writable: {error}; {}",
                std::fs::read_to_string("/proc/self/mountinfo").unwrap().lines()
                    .filter(|line| line.contains(" - cgroup2 ")).collect::<Vec<_>>().join("\n"))
        });
        let outcome = containers_cgroup(&blocked);
        let setup_error = outcome.error();
        let missing: Vec<_> = ["cpu.max", "memory.max", "pids.max"].into_iter()
            .filter(|name| !blocked.join(name).exists()).collect();
        assert!(!missing.is_empty(), "the kernel unexpectedly delegated an occupied parent");
        let after_blocked_controllers = controllers(root);
        let after_blocked_type = std::fs::read_to_string(root.join("cgroup.type")).unwrap();
        eprintln!("occupied parent: error={setup_error:?}, missing={missing:?}, controllers={:?}, type={:?}",
            after_blocked_controllers, after_blocked_type);
        if setup_error.is_some() {
            assert!(after_blocked_controllers.is_empty(), "a failed delegation must be atomic");
            assert_eq!(after_blocked_type.trim(), "domain");
        }
        if blocked.exists() { std::fs::remove_dir(&blocked).unwrap(); }
        for controller in controllers(root) {
            std::fs::write(root.join("cgroup.subtree_control"), format!("-{controller}")).unwrap();
        }

        let group = host_cgroup().unwrap();
        assert_eq!(group, root.join("asagiri"));
        assert_eq!(std::fs::read_to_string("/proc/self/cgroup").unwrap().trim(), "0::/asagiri-host");
        assert!(std::fs::read_to_string(root.join("cgroup.procs")).unwrap().trim().is_empty());
        required(root);
        required(&group);
        assert_eq!(host_cgroup().unwrap(), group, "agent restart must reuse the delegated parent");
        assert_eq!(std::fs::read_to_string("/proc/self/cgroup").unwrap().trim(), "0::/asagiri-host");

        // Exercise the same aggregate parent, live limits and freezer that
        // Buildx needs after the daemon has restarted.
        let docker = group.join("docker");
        let aggregate = docker.join("buildx");
        let _ = containers_cgroup(&docker);
        let _ = containers_cgroup(&aggregate);
        required(&docker);
        required(&aggregate);
        let leaf = aggregate.join("owned-worker");
        std::fs::create_dir(&leaf).unwrap();
        let target = std::fs::File::open(&leaf).unwrap();
        let (ready_read, ready_write) = sys::pipe().unwrap();
        let (stop_read, stop_write) = sys::pipe().unwrap();
        let args = sys::CloneArgs {
            flags: sys::CLONE_INTO_CGROUP, exit_signal: libc::SIGCHLD as u64,
            cgroup: target.as_raw_fd() as u64, ..Default::default()
        };
        // SAFETY: this program is single-threaded and owns the target cgroup.
        let pid = unsafe { sys::clone3(&args) }.unwrap();
        if pid == 0 {
            // SAFETY: the child uses only local mappings and inherited pipes.
            unsafe {
                let bytes = 1024 * 1024;
                let memory = libc::mmap(std::ptr::null_mut(), bytes,
                    libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
                if memory == libc::MAP_FAILED { libc::_exit(2); }
                for at in (0..bytes).step_by(4096) {
                    std::ptr::write_volatile(memory.cast::<u8>().add(at), 7);
                }
                if libc::write(ready_write.as_raw_fd(), b"r".as_ptr().cast(), 1) != 1 { libc::_exit(3); }
                let mut byte = 0u8;
                if libc::read(stop_read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) != 1 { libc::_exit(4); }
                libc::_exit(0);
            }
        }
        let mut child = Child { pid, cgroup: leaf.clone() };
        drop(ready_write);
        drop(stop_read);
        let mut poll = libc::pollfd { fd: ready_read.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid descriptor and pollfd.
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 5000) }, 1, "worker did not become ready");
        let mut byte = [0u8];
        std::fs::File::from(ready_read).read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"r");
        let pids: u64 = std::fs::read_to_string(aggregate.join("pids.current")).unwrap().trim().parse().unwrap();
        let memory: u64 = std::fs::read_to_string(aggregate.join("memory.current")).unwrap().trim().parse().unwrap();
        assert!(pids > 0 && memory > 0, "aggregate accounting was not delegated");
        let limits = asagiri_proto::Limits { pids_max: 256, ..Default::default() };
        cgroup::update_limits(&leaf, &limits).unwrap();
        assert_eq!(std::fs::read_to_string(leaf.join("pids.max")).unwrap().trim(), "256");
        cgroup::freeze(&leaf, true).unwrap();
        cgroup::freeze(&leaf, false).unwrap();
        cgroup::update_limits(&leaf, &asagiri_proto::Limits::default()).unwrap();
        assert_eq!(std::fs::read_to_string(leaf.join("pids.max")).unwrap().trim(), "max");
        std::fs::File::from(stop_write).write_all(b"q").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut status = 0;
        loop {
            // SAFETY: only the owned worker is observed and reaped.
            let found = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if found == pid {
                child.pid = 0;
                break;
            }
            assert_eq!(found, 0);
            assert!(Instant::now() < deadline, "worker did not stop");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        std::fs::remove_dir(&leaf).unwrap();
        std::fs::remove_dir(&aggregate).unwrap();
        std::fs::remove_dir(&docker).unwrap();
        let report = serde_json::json!({
            "schema": "asagiri.cgroup-startup.v1",
            "source_commit": option_env!("ASAGIRI_CGROUP_STARTUP_COMMIT"),
            "compiled_architecture": std::env::consts::ARCH,
            "status": if setup_error.is_some() { "passed" } else { "failed" },
            "occupied_parent_error": setup_error,
            "occupied_parent_missing_files": missing,
            "occupied_parent_controllers_after_failure": after_blocked_controllers,
            "occupied_parent_type_after_failure": after_blocked_type.trim(),
            "namespace_root_drained": true,
            "agent_restart_reused_parent": true,
            "required_controllers_delegated": ["cpu", "memory", "pids"],
            "buildx_aggregate_pids": pids, "buildx_aggregate_memory_bytes": memory,
            "live_pid_limit_and_freezer": "passed", "owned_child_reaped": true,
            "owned_worker_cgroups_removed": true,
            "scope": "Actual Linux cgroup setup and accounting in this private namespace; full Buildx and Kubernetes run separately.",
        });
        println!("{report}");
        if let Ok(path) = std::env::var("ASAGIRI_CGROUP_STARTUP_REPORT") {
            std::fs::write(path, format!("{report}\n")).unwrap();
        }
        assert!(setup_error.is_some(), "an occupied parent was silently accepted without required controllers");
    }
}

#[cfg(target_os = "linux")]
fn main() {
    if std::env::var("ASAGIRI_REQUIRE_CGROUP_STARTUP_TEST").as_deref() != Ok("1") {
        println!("SKIP cgroup-startup: requires a fresh private container with this executable as PID 1");
        return;
    }
    linux::regression();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("SKIP cgroup-startup: Linux only");
}
