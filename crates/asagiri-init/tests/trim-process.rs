//! The trim worker must fork from a single-threaded agent. Run without the
//! threaded Rust test harness, and trim only /dev/null (always unsupported).

#[cfg(target_os = "linux")]
#[path = "../src/linux/trim.rs"]
mod trim;

#[cfg(target_os = "linux")]
fn main() {
    use std::fs::{self, File};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    struct Child(libc::pid_t);
    impl Drop for Child {
        fn drop(&mut self) {
            if self.0 > 0 {
                // SAFETY: this is our own, unreaped child; no PID can be reused.
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                    libc::waitpid(self.0, std::ptr::null_mut(), 0);
                }
            }
        }
    }

    let task = format!("/proc/self/task/{}", std::process::id());
    assert_eq!(fs::read_dir("/proc/self/task").unwrap().count(), 1);
    let inherited = File::open("/dev/null").unwrap();
    assert!(inherited.as_raw_fd() >= 3);
    trim::start("/dev/null");
    let children = fs::read_to_string(format!("{task}/children")).unwrap();
    let pids: Vec<libc::pid_t> = children.split_whitespace().map(|pid| pid.parse().unwrap()).collect();
    assert_eq!(pids.len(), 1, "trim must start exactly one child process");
    let mut child = Child(pids[0]);
    assert_eq!(fs::read_dir("/proc/self/task").unwrap().count(), 1, "the agent must stay single-threaded");

    // Wait for the child to close inherited descriptors. A trim worker must
    // never keep an agent's control channel, pipe or data file alive.
    let inherited_path = format!("/proc/{}/fd/{}", child.0, inherited.as_raw_fd());
    let name_path = format!("/proc/{}/comm", child.0);
    let deadline = Instant::now() + Duration::from_secs(3);
    while (fs::symlink_metadata(&inherited_path).is_ok()
        || fs::read_to_string(&name_path).unwrap_or_default().trim() != "asagiri-trim")
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(fs::symlink_metadata(inherited_path).is_err(), "trim retained an agent descriptor");
    assert_eq!(fs::read_to_string(name_path).unwrap().trim(), "asagiri-trim");

    // The initial FITRIM against /dev/null must stop this worker cleanly,
    // without affecting the agent or requiring a VM or a real data disk.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let mut status = 0;
        // SAFETY: wait only for our own child, writing to a valid status value.
        let waited = unsafe { libc::waitpid(child.0, &mut status, libc::WNOHANG) };
        assert!(waited >= 0, "waitpid: {}", std::io::Error::last_os_error());
        if waited == child.0 {
            child.0 = 0;
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
            break;
        }
        assert!(Instant::now() < deadline, "trim did not exit on an unsupported device");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fs::read_dir("/proc/self/task").unwrap().count(), 1);
    println!("PASS trim runs as a separate process, drops inherited descriptors and exits cleanly");
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("SKIP trim process regression: Linux only");
}
