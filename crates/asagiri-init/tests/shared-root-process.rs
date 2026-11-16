//! Exercise production roots and copy workers with actual overlay mounts,
//! sockets, forks and waitpid. No libtest thread may exist at a worker fork.

#[cfg(target_os = "linux")]
#[path = "../src/layer_tar.rs"]
#[allow(dead_code, unused_imports)] // The included agent also builds its layer extractor.
mod layer_tar;

#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[expect(unused_imports, reason = "harness=false includes cfg(test) modules without libtest bodies")]
mod linux {
    include!("../src/linux/mod.rs");

    #[warn(dead_code, unused_imports)]
    pub(super) fn regression() {
        use std::fs;
        use std::os::fd::{AsRawFd, OwnedFd};
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::path::PathBuf;
        use std::time::{Duration, Instant};
        use asagiri_proto::{Msg, copy_op};
        use container::Paths;
        use roots::Roots;

        if unsafe { libc::geteuid() } != 0 {
            assert!(std::env::var_os("ASAGIRI_REQUIRE_SHARED_ROOT_TEST").is_none(),
                "CI requires real privileged shared-root tests");
            println!("SKIP shared-root-process: requires root and CAP_SYS_ADMIN");
            return;
        }
        assert_eq!(fs::read_dir("/proc/self/task").unwrap().count(), 1);
        // SAFETY: this test is a single-threaded disposable process. Its
        // mounts and working directory must not affect the caller.
        sys::check(unsafe { libc::unshare(libc::CLONE_NEWNS) } as _, "test unshare").unwrap();
        sys::remount(c"/", libc::MS_REC | libc::MS_PRIVATE).unwrap();

        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                if let Ok(entries) = fs::read_dir(self.0.join("containers")) {
                    for entry in entries.flatten() { let _ = sys::umount_lazy(&entry.path().join("rootfs")); }
                }
                let _ = std::env::set_current_dir("/");
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let base = std::env::temp_dir().join(format!("asagiri-shared-root-process-{}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        let fixture = Fixture(base);
        let paths = Paths { layers: fixture.0.join("layers"), containers: fixture.0.join("containers"), cgroup: fixture.0.join("cgroup") };
        fs::create_dir_all(&paths.layers).unwrap();
        fs::create_dir(&paths.containers).unwrap();
        let digest = format!("sha256:{}", "1".repeat(64));
        let lower = paths.layers.join(&digest[7..]);
        fs::create_dir(&lower).unwrap();
        fs::write(lower.join("image"), b"image contents").unwrap();
        fs::write(paths.layers.join(format!("{}.done", &digest[7..])), b"").unwrap();
        std::env::set_current_dir(&paths.layers).unwrap();
        let socket = fixture.0.join("copy.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener: OwnedFd = listener.into();
        let roots = Roots::default();
        let mut copies = copy::Broker::new(&roots);

        fn acquire(roots: &Roots, paths: &Paths, id: &str, digest: &str) -> PathBuf {
            let mut frame = Vec::new();
            asagiri_proto::encode_copy(&mut frame, copy_op::STAT, id, "/", 0, &[digest], "", &[]);
            let Msg::Copy(spec) = asagiri_proto::decode(&frame).unwrap() else { panic!("copy frame") };
            roots.acquire(paths, id, spec.layers).unwrap()
        }

        fn busy(roots: &Roots, paths: &Paths, id: &str, digest: &str) {
            let mut frame = Vec::new();
            asagiri_proto::encode_copy(&mut frame, copy_op::STAT, id, "/", 0, &[digest], "", &[]);
            let Msg::Copy(spec) = asagiri_proto::decode(&frame).unwrap() else { panic!("copy frame") };
            assert_eq!(roots.acquire(paths, id, spec.layers).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        }

        fn step(roots: &Roots, copies: &mut copy::Broker<'_>, paths: &Paths, reap: bool, running: Option<(&str, libc::pid_t)>) {
            let view = |id: &str| match running {
                Some((name, pid)) if id == name => copy::View::Running(pid),
                _ => copy::View::Stopped,
            };
            roots.start(paths, Instant::now());
            copies.advance(paths, view);
            let mut fds = Vec::new();
            copies.interest(&mut fds);
            // SAFETY: a valid poll array; a short wait also gives children CPU.
            assert!(unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 5) } >= 0);
            for fd in fds.iter().filter(|fd| fd.revents != 0) { copies.readable(fd.fd); }
            if reap {
                loop {
                    let mut status = 0;
                    // SAFETY: all children belong to this single-threaded test.
                    let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                    if pid <= 0 { break; }
                    let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) };
                    assert!(copies.reaped(pid, code) || roots.reaped(pid, code, Instant::now()), "unowned child {pid}");
                }
            }
            copies.advance(paths, view);
        }

        fn until(roots: &Roots, copies: &mut copy::Broker<'_>, paths: &Paths, running: Option<(&str, libc::pid_t)>, done: impl Fn() -> bool) {
            let end = Instant::now() + Duration::from_secs(10);
            while !done() {
                assert!(Instant::now() < end, "shared-root test timed out");
                step(roots, copies, paths, true, running);
            }
        }

        fn request(socket: &std::path::Path, listener: &OwnedFd, copies: &mut copy::Broker<'_>, paths: &Paths, digest: &str, id: &str, op: u8, path: &str) -> UnixStream {
            let mut client = UnixStream::connect(socket).unwrap();
            client.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            copies.accept(listener, paths);
            let mut frame = Vec::new();
            asagiri_proto::encode_copy(&mut frame, op, id, path, 0, &[digest], "", &[]);
            client.write_all(&frame).unwrap();
            client
        }

        fn archive(name: &str) -> Vec<u8> {
            let data = vec![b'x'; 512 * 1024];
            let mut tar = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, data.as_slice()).unwrap();
            tar.into_inner().unwrap()
        }

        fn response(client: &mut UnixStream, roots: &Roots, copies: &mut copy::Broker<'_>, paths: &Paths) -> Vec<u8> {
            client.set_nonblocking(true).unwrap();
            let end = Instant::now() + Duration::from_secs(10);
            let mut bytes = Vec::new();
            loop {
                step(roots, copies, paths, true, None);
                let mut buf = [0u8; 4096];
                match client.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => bytes.extend_from_slice(&buf[..n]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("copy response: {error}"),
                }
                assert!(Instant::now() < end, "copy response timed out");
            }
            bytes
        }

        fn no_reply(client: &mut UnixStream) {
            client.set_nonblocking(true).unwrap();
            let error = client.read(&mut [0u8; 1]).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock, "reply/EOF escaped before waitpid");
            client.set_nonblocking(false).unwrap();
        }

        struct Running(Option<libc::pid_t>);
        impl Running {
            fn stop(&mut self) {
                let Some(pid) = self.0.take() else { return };
                let mut status = 0;
                // SAFETY: prove this is still our unreaped child before
                // signalling; it may already have exited during a failure.
                let observed = loop {
                    let observed = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                    if observed >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted { break observed; }
                };
                if observed != 0 { return; }
                unsafe { libc::kill(pid, libc::SIGKILL); }
                loop {
                    // SAFETY: reap that owned child before its lease ends.
                    if unsafe { libc::waitpid(pid, &mut status, 0) } >= 0
                        || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted { break; }
                }
            }
        }
        impl Drop for Running {
            fn drop(&mut self) { self.stop(); }
        }

        fn running(root: &std::path::Path) -> Running {
            let (read, write) = sys::pipe().unwrap();
            let root = sys::cstr(root).unwrap();
            // SAFETY: the harness is single-threaded. The child owns only a
            // private mount namespace, one ready pipe, and its test root.
            let parent = unsafe { libc::getpid() };
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                drop(read);
                let fd = write.as_raw_fd();
                let okay = unsafe {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) == 0
                        && libc::getppid() == parent
                        && libc::unshare(libc::CLONE_NEWNS) == 0
                        && sys::remount(c"/", libc::MS_REC | libc::MS_PRIVATE).is_ok()
                        && libc::chdir(root.as_ptr()) == 0
                        && libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) == 0
                        && libc::umount2(c".".as_ptr(), libc::MNT_DETACH) == 0
                        && libc::chdir(c"/".as_ptr()) == 0
                };
                // SAFETY: close only this child's inherited descriptors.
                unsafe {
                    if fd > 3 { libc::syscall(libc::SYS_close_range, 3u32, fd as u32 - 1, 0); }
                    libc::syscall(libc::SYS_close_range, fd as u32 + 1, u32::MAX, 0);
                    libc::write(fd, if okay { b"r" } else { b"e" }.as_ptr().cast(), 1);
                    libc::close(fd);
                    if !okay { libc::_exit(1); }
                    loop { libc::pause(); }
                }
            }
            let owned = Running(Some(pid));
            drop(write);
            let mut ready = [0u8; 1];
            File::from(read).read_exact(&mut ready).unwrap();
            assert_eq!(ready, *b"r");
            owned
        }

        // A created container has no writable directory yet. Diff must
        // return an empty successful stream without mounting an overlay.
        let fresh = paths.containers.join("never-started");
        assert!(!fresh.exists());
        let mut empty_diff = request(&socket, &listener, &mut copies, &paths, &digest, "never-started", copy_op::DIFF, "/");
        assert!(matches!(asagiri_proto::decode(&response(&mut empty_diff, &roots, &mut copies, &paths)), Ok(Msg::CopyDone)));
        assert!(fresh.join("upper").is_dir());
        assert_eq!(fs::read_dir(fresh.join("upper")).unwrap().count(), 0);
        assert!(!fresh.join("rootfs").exists());
        assert!(!roots.unfinished().iter().any(|id| id == "never-started"));
        println!("PASS shared-root: never-started diff initializes an empty upper without an overlay or remaining lease");

        // A completed archive must permit the next start to acquire its root.
        // Reap the actual copy child while deliberately delaying root-worker
        // admission. This is the same ordering as a slow overlay unmount,
        // without relying on host I/O load or changing a production timeout.
        for (id, op, path) in [
            ("stat-completion", copy_op::STAT, "/image"),
            ("upload-completion", copy_op::IN, "/"),
        ] {
            let mut client = request(&socket, &listener, &mut copies, &paths, &digest, id, op, path);
            if op == copy_op::IN {
                let body = archive("uploaded");
                client.write_all(&body[..64 * 1024]).unwrap();
                until(&roots, &mut copies, &paths, None, || paths.containers.join(id).join("upper/uploaded").exists());
                client.write_all(&body[64 * 1024..]).unwrap();
                client.shutdown(std::net::Shutdown::Write).unwrap();
            }
            let end = Instant::now() + Duration::from_secs(10);
            let mut reaped = false;
            while !reaped {
                copies.advance(&paths, |_| copy::View::Stopped);
                let mut fds = Vec::new();
                copies.interest(&mut fds);
                // SAFETY: valid descriptors owned by this test's copy broker.
                assert!(unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 5) } >= 0);
                for fd in fds.iter().filter(|fd| fd.revents != 0) { copies.readable(fd.fd); }
                let mut status = 0;
                // No root worker was admitted for this ID. The sole child is
                // the actual copy worker, owned and reaped by this fixture.
                let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                if pid > 0 {
                    let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) };
                    assert!(copies.reaped(pid, code), "unexpected child {pid}");
                    reaped = true;
                }
                assert!(Instant::now() < end, "copy worker did not finish");
            }
            let copied_device = fs::metadata(paths.containers.join(id).join("rootfs")).unwrap().dev();
            copies.advance(&paths, |_| copy::View::Stopped);
            client.set_nonblocking(true).unwrap();
            let mut prefix = [0u8; 1];
            let mut reply = Vec::new();
            match client.read(&mut prefix) {
                Ok(0) => panic!("archive EOF escaped without its final reply"),
                Ok(n) => {
                    assert!(!roots.cleaning(id), "archive reply escaped while the next start's root was still being cleaned up");
                    reply.extend_from_slice(&prefix[..n]);
                }
                Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock),
            }
            reply.extend(response(&mut client, &roots, &mut copies, &paths));
            assert!(match asagiri_proto::decode(&reply) {
                Ok(Msg::CopyStat { .. }) => op == copy_op::STAT,
                Ok(Msg::CopyDone) => op == copy_op::IN,
                _ => false,
            });
            assert!(!roots.cleaning(id), "completed archive left the next start waiting on cleanup");
            let ready = acquire(&roots, &paths, id, &digest);
            assert_eq!(fs::metadata(&ready).unwrap().dev(), copied_device,
                "the completed copy's overlay must be reused without an unmount/writeback");
            assert_eq!(fs::read(ready.join("image")).unwrap(), b"image contents");
            if op == copy_op::IN {
                assert_eq!(fs::read(ready.join("uploaded")).unwrap(), vec![b'x'; 512 * 1024]);
            }
            roots.release(id);
            until(&roots, &mut copies, &paths, None, || !roots.cleaning(id));
        }
        println!("PASS shared-root: archive metadata and upload completion reuse the same overlay immediately without cleanup");

        // Ordinary process reap must leave a stopped container ready for
        // restart, even when no copy still holds a lease. Exercise the same
        // release_root path as the agent after reaping actual namespace
        // children; delay cleanup admission to expose any queued unmount.
        let stopped = acquire(&roots, &paths, "restart-idle", &digest);
        let device = fs::metadata(&stopped).unwrap().dev();
        fs::write(stopped.join("persisted"), b"across restarts").unwrap();
        for _ in 0..3 {
            let mut child = running(&stopped);
            let process = container::Process::test_container("restart-idle", child.0.unwrap(), &paths, false);
            child.stop();
            container::release_root(&process, &roots);
            assert!(!roots.cleaning("restart-idle"), "process reap left the next restart waiting on an unmount");
            let restarted = acquire(&roots, &paths, "restart-idle", &digest);
            assert_eq!(fs::metadata(&restarted).unwrap().dev(), device);
            assert_eq!(fs::read(restarted.join("persisted")).unwrap(), b"across restarts");
        }
        roots.release_idle("restart-idle");
        roots.remove("restart-idle");
        assert!(roots.removing("restart-idle"));
        until(&roots, &mut copies, &paths, None, || !roots.cleaning("restart-idle"));
        assert!(!paths.containers.join("restart-idle").exists());

        let automatic = acquire(&roots, &paths, "auto-remove", &digest);
        let mut child = running(&automatic);
        let process = container::Process::test_container("auto-remove", child.0.unwrap(), &paths, true);
        child.stop();
        container::release_root(&process, &roots);
        assert!(roots.removing("auto-remove"), "auto-remove must override idle-root reuse");
        until(&roots, &mut copies, &paths, None, || !roots.cleaning("auto-remove"));
        assert!(!paths.containers.join("auto-remove").exists());
        println!("PASS shared-root: reaped processes restart on the same overlay; explicit and automatic removal reclaim it");

        // A stopped copy and a start share a single actual overlay, including
        // its device identity. A long copy must not retain unrelated roots.
        let other = acquire(&roots, &paths, "other", &digest);
        let mut client = request(&socket, &listener, &mut copies, &paths, &digest, "shared", copy_op::IN, "/");
        let body = archive("copy");
        client.write_all(&body[..64 * 1024]).unwrap();
        let base = paths.containers.join("shared");
        until(&roots, &mut copies, &paths, None, || base.join("upper/copy").exists());
        let mounted = fs::metadata(base.join("rootfs")).unwrap().dev();
        let started = acquire(&roots, &paths, "shared", &digest);
        assert_eq!(fs::metadata(&started).unwrap().dev(), mounted);
        roots.release("other");
        until(&roots, &mut copies, &paths, None, || !roots.cleaning("other"));
        let other_again = acquire(&roots, &paths, "other", &digest);
        assert_eq!(other, other_again);
        assert_eq!(fs::read(other_again.join("image")).unwrap(), b"image contents");
        roots.release("other");
        roots.release("shared");
        assert!(!roots.cleaning("shared"), "the copy still owns this overlay");
        client.write_all(&body[64 * 1024..]).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        for _ in 0..20 { step(&roots, &mut copies, &paths, false, None); }
        no_reply(&mut client);
        assert!(matches!(asagiri_proto::decode(&response(&mut client, &roots, &mut copies, &paths)), Ok(Msg::CopyDone)));
        until(&roots, &mut copies, &paths, None, || !roots.cleaning("shared"));
        assert_eq!(fs::read(base.join("upper/copy")).unwrap(), vec![b'x'; 512 * 1024]);
        println!("PASS shared-root: stopped copy/start share one overlay; unrelated roots retire during the copy; ACK/EOF wait for reap");

        // A copied namespace can still hold the old overlay after its agent
        // unmount worker exits. Reuse waits for the namespace-ready event.
        acquire(&roots, &paths, "deferred", &digest);
        roots.begin_copy();
        roots.release("deferred");
        for _ in 0..20 { step(&roots, &mut copies, &paths, true, None); }
        busy(&roots, &paths, "deferred", &digest);
        assert!(roots.cleaning("deferred"));
        roots.copy_ready();
        acquire(&roots, &paths, "deferred", &digest);
        roots.release("deferred");
        until(&roots, &mut copies, &paths, None, || !roots.cleaning("deferred"));
        println!("PASS shared-root: reaped unmount waits for inherited namespace references");

        // The descriptor granted from a live process keeps that exact mount
        // view after the process exits. The restarted owner shares its root.
        let root = acquire(&roots, &paths, "running", &digest);
        let device = fs::metadata(&root).unwrap().dev();
        let mut process = running(&root);
        let pid = process.0.unwrap();
        let mut client = request(&socket, &listener, &mut copies, &paths, &digest, "running", copy_op::IN, "/");
        let running_body = archive("running-copy");
        client.write_all(&running_body[..64 * 1024]).unwrap();
        until(&roots, &mut copies, &paths, Some(("running", pid)), || root.join("running-copy").exists());
        process.stop();
        roots.release("running");
        assert!(!roots.cleaning("running"), "the copy still holds the departed process's root");
        let restarted = acquire(&roots, &paths, "running", &digest);
        assert_eq!(fs::metadata(&restarted).unwrap().dev(), device);
        roots.release("running");
        client.write_all(&running_body[64 * 1024..]).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(matches!(asagiri_proto::decode(&response(&mut client, &roots, &mut copies, &paths)), Ok(Msg::CopyDone)));
        until(&roots, &mut copies, &paths, None, || !roots.cleaning("running"));
        assert_eq!(fs::read(paths.containers.join("running/upper/running-copy")).unwrap(), vec![b'x'; 512 * 1024]);
        println!("PASS shared-root: a pinned running view survives process exit and shares its overlay with restart");

        // Metadata is bounded and sent only after the worker is reaped.
        let mut stat = request(&socket, &listener, &mut copies, &paths, &digest, "shared", copy_op::STAT, "/image");
        for _ in 0..20 { step(&roots, &mut copies, &paths, false, None); }
        no_reply(&mut stat);
        assert!(matches!(asagiri_proto::decode(&response(&mut stat, &roots, &mut copies, &paths)), Ok(Msg::CopyStat { .. })));
        until(&roots, &mut copies, &paths, None, || !roots.cleaning("shared"));
        println!("PASS shared-root: stat reply is held through actual worker reap");

        // Download bytes may stream before reap; the end of the stream may
        // not. Drain the complete tar while deliberately withholding waitpid.
        let mut download = request(&socket, &listener, &mut copies, &paths, &digest, "shared", copy_op::OUT, "/image");
        download.set_nonblocking(true).unwrap();
        let mut streamed = Vec::new();
        for _ in 0..30 {
            step(&roots, &mut copies, &paths, false, None);
            loop {
                let mut buf = [0u8; 4096];
                match download.read(&mut buf) {
                    Ok(0) => panic!("download EOF escaped before worker reap"),
                    Ok(n) => streamed.extend_from_slice(&buf[..n]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("download: {error}"),
                }
            }
        }
        assert!(streamed.len() > 4);
        let metadata = asagiri_proto::frame_len(streamed[..4].try_into().unwrap()).unwrap();
        assert!(matches!(asagiri_proto::decode(&streamed[..metadata]), Ok(Msg::CopyStat { .. })));
        let mut downloaded = tar::Archive::new(&streamed[metadata..]);
        let mut contents = Vec::new();
        downloaded.entries().unwrap().next().unwrap().unwrap().read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"image contents");
        assert!(response(&mut download, &roots, &mut copies, &paths).is_empty());
        until(&roots, &mut copies, &paths, None, || !roots.cleaning("shared"));
        println!("PASS shared-root: download streams bytes but retains EOF until its worker is reaped");

        // Removal cannot delete the upper while a copy is still writing.
        let mut client = request(&socket, &listener, &mut copies, &paths, &digest, "removed", copy_op::IN, "/");
        client.write_all(&body[..64 * 1024]).unwrap();
        let removed = paths.containers.join("removed");
        until(&roots, &mut copies, &paths, None, || removed.join("upper/copy").exists());
        roots.remove("removed");
        for _ in 0..10 { step(&roots, &mut copies, &paths, true, None); }
        assert!(removed.join("upper/copy").exists());
        assert!(roots.removing("removed"));
        client.write_all(&body[64 * 1024..]).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(matches!(asagiri_proto::decode(&response(&mut client, &roots, &mut copies, &paths)), Ok(Msg::CopyDone)));
        until(&roots, &mut copies, &paths, None, || !roots.cleaning("removed"));
        assert!(!removed.exists());
        println!("PASS shared-root: removal waits for the last writer and its reaped namespace");

        // An idle successful copy still belongs to its container. Explicit
        // removal and host reconciliation must reclaim it, but retain keeps
        // named containers and every active lease protected.
        roots.remove("shared");
        until(&roots, &mut copies, &paths, None, || !roots.cleaning("shared"));
        assert!(!paths.containers.join("shared").exists());
        acquire(&roots, &paths, "retained", &digest);
        roots.release_idle("retained");
        let active = acquire(&roots, &paths, "active", &digest);
        let mut active_child = running(&active);
        let active_process = container::Process::test_container("active", active_child.0.unwrap(), &paths, false);
        let mut active_copy = request(&socket, &listener, &mut copies, &paths, &digest, "active-copy", copy_op::IN, "/");
        let active_body = archive("copied");
        active_copy.write_all(&active_body[..64 * 1024]).unwrap();
        until(&roots, &mut copies, &paths, None, || paths.containers.join("active-copy/upper/copied").exists());
        let mut frame = Vec::new();
        asagiri_proto::encode_retain(&mut frame, &["retained"]);
        let Msg::Retain(keep) = asagiri_proto::decode(&frame).unwrap() else { panic!("retain frame") };
        roots.retain(keep);
        until(&roots, &mut copies, &paths, None, || !roots.cleaning("running"));
        assert!(!paths.containers.join("running").exists());
        assert!(paths.containers.join("retained/rootfs/image").is_file());
        assert!(paths.containers.join("active/rootfs/image").is_file());
        assert!(!roots.removing("retained"));
        assert!(!roots.removing("active"));
        assert!(!roots.removing("active-copy"));
        active_child.stop();
        container::release_root(&active_process, &roots);
        assert!(roots.removing("active"), "Retain omission must retire a process root when its last lease ends");
        until(&roots, &mut copies, &paths, None, || !roots.cleaning("active"));
        assert!(!paths.containers.join("active").exists());
        active_copy.write_all(&active_body[64 * 1024..]).unwrap();
        active_copy.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(matches!(asagiri_proto::decode(&response(&mut active_copy, &roots, &mut copies, &paths)), Ok(Msg::CopyDone)));
        assert!(!paths.containers.join("active-copy").exists(), "Retain omission must retire a completed copy root");

        // A later ownership snapshot can retain a still-active root again,
        // but cannot revoke an explicit Remove already requested for it.
        for (id, explicit) in [("re-retained", false), ("explicit-active", true)] {
            let root = acquire(&roots, &paths, id, &digest);
            let mut child = running(&root);
            let process = container::Process::test_container(id, child.0.unwrap(), &paths, false);
            roots.retain(keep);
            if explicit { roots.remove(id); }
            let mut renewed = Vec::new();
            asagiri_proto::encode_retain(&mut renewed, &["retained", "re-retained", "explicit-active"]);
            let Msg::Retain(renewed) = asagiri_proto::decode(&renewed).unwrap() else { panic!("retain frame") };
            roots.retain(renewed);
            child.stop();
            container::release_root(&process, &roots);
            assert_eq!(roots.removing(id), explicit);
            until(&roots, &mut copies, &paths, None, || !roots.cleaning(id));
            assert_eq!(root.join("image").exists(), !explicit);
        }
        println!("PASS shared-root: reconciliation protects active leases, retires omitted roots after completion, and respects renewed ownership and explicit removal");

        // A malformed archive reports its real error and releases its root;
        // it must not be mistaken for a successful idle copy.
        let mut malformed = request(&socket, &listener, &mut copies, &paths, &digest, "malformed", copy_op::IN, "/");
        malformed.write_all(&[b'!'; 512]).unwrap();
        malformed.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(matches!(asagiri_proto::decode(&response(&mut malformed, &roots, &mut copies, &paths)), Ok(Msg::Error(_))));
        assert!(!roots.unfinished().iter().any(|id| id == "malformed"));
        println!("PASS shared-root: malformed upload returns an error and releases its root");

        // An abandoned upload releases its lease; shutdown joins workers
        // before root teardown and leaves no child able to touch these paths.
        let mut abandoned = request(&socket, &listener, &mut copies, &paths, &digest, "abandoned", copy_op::IN, "/");
        abandoned.write_all(&body[..64 * 1024]).unwrap();
        until(&roots, &mut copies, &paths, None, || paths.containers.join("abandoned/upper/copy").exists());
        drop(abandoned);
        until(&roots, &mut copies, &paths, None, || !roots.unfinished().iter().any(|id| id == "abandoned"));
        let mut held = request(&socket, &listener, &mut copies, &paths, &digest, "shutdown", copy_op::IN, "/");
        held.write_all(&body[..64 * 1024]).unwrap();
        until(&roots, &mut copies, &paths, None, || paths.containers.join("shutdown/upper/copy").exists());
        copies.shutdown();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !roots.unfinished().is_empty() {
            step(&roots, &mut copies, &paths, true, None);
            roots.poll_shutdown(&paths, Instant::now(), deadline);
            assert!(Instant::now() < deadline, "shutdown did not reclaim idle copy roots");
        }
        assert!(!fs::read_to_string("/proc/self/mountinfo").unwrap().lines()
            .any(|line| line.contains(&paths.containers.display().to_string())));
        let mut status = 0;
        // SAFETY: this test owns every child. ECHILD also works on guest
        // kernels that omit the optional /proc/PID/task/TID/children file.
        assert_eq!(unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
        assert_eq!(fs::read_dir("/proc/self/task").unwrap().count(), 1);
        println!("PASS shared-root: disconnect and shutdown reap owned workers and release every lease");
    }
}

#[cfg(target_os = "linux")]
fn main() { linux::regression(); }

#[cfg(not(target_os = "linux"))]
fn main() { println!("SKIP shared-root-process: Linux only"); }
