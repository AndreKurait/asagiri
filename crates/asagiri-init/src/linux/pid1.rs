//! `--init`: the container's initial process stays ours and runs the program as its
//! child, as tini does for Docker. Signals sent to the container go to the
//! program, orphans re-parented to PID 1 are reaped, and PID 1 exits with
//! the program's status. A program that ignores SIGTERM as PID 1 (the
//! kernel drops signals PID 1 has no handler for) now stops on it. In host
//! PID mode the supervisor is a subreaper instead of the namespace's PID 1.

use std::ffi::CStr;
use std::io;
use std::os::fd::RawFd;

use super::sys::child_fail;

/// Adopt only orphaned descendants of this process. The agent uses this on
/// Linux hosts, and --init uses it inside a shared PID namespace. The flag
/// is not inherited by forked children; each supervisor registers itself.
pub(super) fn adopt_orphans() -> io::Result<()> {
    // SAFETY: this changes only the calling process's child-reaper policy.
    unsafe {
        if libc::getpid() != 1 && libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Signals left to their default action in PID 1: the ones raised by its
/// own faults, and terminal job control.
const NOT_FORWARDED: [libc::c_int; 9] =
    [libc::SIGFPE, libc::SIGILL, libc::SIGSEGV, libc::SIGBUS, libc::SIGABRT, libc::SIGTRAP, libc::SIGSYS, libc::SIGTTIN, libc::SIGTTOU];

/// The supervisor does not exec, so it must close the descriptors that an
/// ordinary container releases through CLOEXEC. Called before user limits
/// and seccomp, with stdin/stdout/stderr already installed.
pub(super) fn close_inherited(keep: Option<RawFd>) -> io::Result<()> {
    // The private setup reply must survive until final user/command setup.
    // The child closes it as soon as the supervisor is ready.
    let keep = keep.filter(|fd| *fd >= 3).map(|fd| fd as u32);
    if let Some(fd) = keep {
        if fd > 3 { close_range(3, fd - 1)?; }
        close_range(fd + 1, u32::MAX)
    } else {
        close_range(3, u32::MAX)
    }
}

fn close_range(first: u32, last: u32) -> io::Result<()> {
    // SAFETY: this runs only in the cloned child's private descriptor table.
    if unsafe { libc::syscall(libc::SYS_close_range, first, last, 0) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOSYS) {
        return Err(error);
    }
    close_inherited_one_by_one(first, last)
}

/// Kernels before close_range still support the native backend. Use the
/// inherited hard limit before applying a possibly smaller container limit;
/// /proc inside the container can be replaced by a caller's bind mount.
fn close_inherited_one_by_one(first: u32, last: u32) -> io::Result<()> {
    // SAFETY: getrlimit initializes a plain local structure.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if limit.rlim_max == libc::RLIM_INFINITY {
        return Err(io::Error::other("cannot bound inherited descriptors"));
    }
    let end = limit.rlim_max.min(libc::c_int::MAX as libc::rlim_t).min((last as libc::rlim_t).saturating_add(1));
    for fd in (first as libc::rlim_t)..end {
        // SAFETY: only the child's inherited descriptor table is affected.
        if unsafe { libc::close(fd as libc::c_int) } != 0 {
            let error = io::Error::last_os_error();
            // Linux releases the descriptor even when close reports EINTR.
            if !matches!(error.raw_os_error(), Some(libc::EBADF | libc::EINTR)) {
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Runs `exe` as PID 1's child and supervises it. Called by the container's
/// first process in place of its exec, after everything else is set up
/// (user, capabilities, seccomp), so both run with the same rights.
/// `tty`: the program gets its own process group and the terminal's
/// foreground, so ^C reaches it once, not through PID 1 as well.
pub fn run(exe: &CStr, argv: *const *const libc::c_char, envp: *const *const libc::c_char, tty: bool) -> ! {
    // SAFETY: signal-mask and process calls on local sets, in the
    // single-threaded child; the child branch only execs or exits.
    unsafe {
        // Blocked signals queue even for PID 1, which the kernel would
        // otherwise drop without a handler; sigtimedwait takes them below.
        let mut forward: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut forward);
        for s in NOT_FORWARDED {
            libc::sigdelset(&mut forward, s);
        }
        libc::sigprocmask(libc::SIG_SETMASK, &forward, std::ptr::null_mut());
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);

        let child = libc::fork();
        if child < 0 {
            child_fail(2, "init", io::Error::last_os_error());
        }
        if child == 0 {
            if tty {
                libc::setpgid(0, 0);
                libc::tcsetpgrp(0, libc::getpgrp());
            }
            // The program starts as Docker would start it: nothing blocked,
            // default actions.
            libc::signal(libc::SIGTTIN, libc::SIG_DFL);
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            libc::execve(exe.as_ptr(), argv, envp);
            let err = io::Error::last_os_error();
            // tini runs the program with execvp, which hands a file that is
            // no program (a script without a shebang) to /bin/sh.
            if err.raw_os_error() == Some(libc::ENOEXEC) {
                let sh = c"/bin/sh";
                let mut args = vec![sh.as_ptr(), exe.as_ptr()];
                let mut i = 1;
                while !(*argv).is_null() && !(*argv.add(i)).is_null() {
                    args.push(*argv.add(i));
                    i += 1;
                }
                args.push(std::ptr::null());
                libc::execve(sh.as_ptr(), args.as_ptr(), envp);
            }
            super::sys::exec_fail(2, exe, err, true);
        }

        let wake = libc::timespec { tv_sec: 1, tv_nsec: 0 };
        loop {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let sig = libc::sigtimedwait(&forward, &mut info, &wake);
            if sig > 0 && sig != libc::SIGCHLD {
                libc::kill(child, sig);
            }
            if let Some(code) = reap(child) {
                libc::_exit(code);
            }
        }
    }
}

/// Reaps every exited child; the program's exit status once it has gone.
fn reap(child: libc::pid_t) -> Option<i32> {
    let mut code = None;
    loop {
        let mut status = 0;
        // SAFETY: plain waitpid on any child, without blocking.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            return code;
        }
        if pid == child {
            code = Some(exit_code(status));
        }
    }
}

/// A wait status as a shell reports it: the exit code, or 128 + the signal.
fn exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_pid_one_supervisor_adopts_and_reaps_its_orphan() {
        // SAFETY: the disposable child alone changes reaper policy and
        // forks; it exits directly without running the harness destructors.
        unsafe {
            let supervisor = libc::fork();
            assert!(supervisor >= 0);
            if supervisor == 0 {
                libc::alarm(5);
                if adopt_orphans().is_err() { libc::_exit(1); }
                let me = libc::getpid();
                let middle = libc::fork();
                if middle < 0 { libc::_exit(2); }
                if middle == 0 {
                    let orphan = libc::fork();
                    if orphan < 0 { libc::_exit(3); }
                    if orphan == 0 {
                        for _ in 0..1000 {
                            if libc::getppid() == me { libc::_exit(0); }
                            libc::usleep(1000);
                        }
                        libc::_exit(4);
                    }
                    libc::_exit(0);
                }
                let mut status = 0;
                if libc::waitpid(middle, &mut status, 0) != middle || status != 0 { libc::_exit(5); }
                let adopted = libc::waitpid(-1, &mut status, 0);
                if adopted <= 0 || adopted == middle || status != 0 { libc::_exit(6); }
                if libc::waitpid(-1, &mut status, libc::WNOHANG) != -1
                    || io::Error::last_os_error().raw_os_error() != Some(libc::ECHILD) { libc::_exit(7); }
                libc::_exit(0);
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(supervisor, &mut status, 0), supervisor);
            assert_eq!(status, 0);
        }
    }

    #[test]
    fn statuses_read_as_a_shell_reports_them() {
        assert_eq!(exit_code(3 << 8), 3);
        assert_eq!(exit_code(libc::SIGTERM), 128 + 15);
    }

    #[test]
    fn inherited_descriptors_close_without_losing_stdio() {
        for fallback in [false, true] {
            // SAFETY: the child changes only its own descriptor table and
            // exits directly, without running the test harness destructors.
            unsafe {
                let child = libc::fork();
                assert!(child >= 0);
                if child == 0 {
                    let limit = libc::rlimit { rlim_cur: 128, rlim_max: 128 };
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                        libc::_exit(1);
                    }
                    let fd = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 64);
                    if fd < 64 {
                        libc::_exit(2);
                    }
                    let result = if fallback { close_inherited_one_by_one(3, u32::MAX) } else { close_inherited(None) };
                    if result.is_err() || libc::fcntl(fd, libc::F_GETFD) != -1 {
                        libc::_exit(3);
                    }
                    if libc::fcntl(1, libc::F_GETFD) < 0 || libc::fcntl(2, libc::F_GETFD) < 0 {
                        libc::_exit(4);
                    }
                    libc::_exit(0);
                }
                let mut status = 0;
                assert_eq!(libc::waitpid(child, &mut status, 0), child);
                assert_eq!(status, 0, "fallback={fallback}");
            }
        }
    }

    #[test]
    fn only_the_setup_reply_survives_until_init_is_ready() {
        for fallback in [false, true] {
            // SAFETY: all descriptor changes happen in this test's child.
            unsafe {
                let child = libc::fork();
                assert!(child >= 0);
                if child == 0 {
                    let limit = libc::rlimit { rlim_cur: 128, rlim_max: 128 };
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 { libc::_exit(1); }
                    let keep = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 64);
                    let other = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, keep + 1);
                    if keep < 64 || other <= keep { libc::_exit(2); }
                    let result = if fallback {
                        close_inherited_one_by_one(3, keep as u32 - 1)
                            .and_then(|()| close_inherited_one_by_one(keep as u32 + 1, u32::MAX))
                    } else {
                        close_inherited(Some(keep))
                    };
                    if result.is_err() || libc::fcntl(keep, libc::F_GETFD) < 0 || libc::fcntl(other, libc::F_GETFD) != -1 {
                        libc::_exit(3);
                    }
                    libc::_exit(0);
                }
                let mut status = 0;
                assert_eq!(libc::waitpid(child, &mut status, 0), child);
                assert_eq!(status, 0, "fallback={fallback}");
            }
        }
    }
}
