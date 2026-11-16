//! Explicit container removals run one at a time outside the control loop.
//! IDs stay reserved until the worker exits successfully, including retries.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

const RETRY: Duration = Duration::from_millis(250);

#[derive(Default)]
pub(super) struct Removals {
    pending: VecDeque<String>,
    active: Option<(String, libc::pid_t)>,
    retry_at: Option<Instant>,
}

impl Removals {
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.pending.iter().map(String::as_str).chain(self.active.iter().map(|(id, _)| id.as_str()))
    }

    pub fn contains(&self, id: &str) -> bool { self.ids().any(|queued| queued == id) }

    pub fn enqueue(&mut self, id: &str) {
        if !self.contains(id) { self.pending.push_back(id.to_owned()); }
    }

    pub fn is_empty(&self) -> bool { self.pending.is_empty() && self.active.is_none() }

    pub fn needs_retry_poll(&self) -> bool { self.active.is_none() && !self.pending.is_empty() }

    pub fn start(&mut self, now: Instant, spawn: impl FnOnce(&str) -> io::Result<libc::pid_t>) {
        if self.active.is_some() || self.retry_at.is_some_and(|at| now < at) { return; }
        let Some(id) = self.pending.front() else { return; };
        match spawn(id) {
            Ok(pid) => {
                self.active = Some((self.pending.pop_front().expect("front exists"), pid));
                self.retry_at = None;
            }
            Err(error) => {
                eprintln!("asagiri-init: remove {id}: {error}; retrying");
                self.retry_at = Some(now + RETRY);
            }
        }
    }

    /// Admit shutdown retries only before the deadline. An active worker must
    /// still be reaped after it, before its paths or mounts can be reused.
    pub fn poll_shutdown(&mut self, now: Instant, deadline: Instant, spawn: impl FnOnce(&str) -> io::Result<libc::pid_t>) -> bool {
        let retry = now < deadline;
        if retry { self.start(now, spawn); }
        self.active.is_some() || (retry && !self.is_empty())
    }

    pub fn reaped(&mut self, pid: libc::pid_t, code: i32, now: Instant) -> bool {
        if !self.active.as_ref().is_some_and(|(_, worker)| *worker == pid) { return false; }
        let (id, _) = self.active.take().expect("worker matched");
        if code != 0 {
            eprintln!("asagiri-init: remove {id} exited {code}; retrying");
            self.pending.push_front(id);
            self.retry_at = Some(now + RETRY);
        }
        true
    }
}

impl Drop for Removals {
    fn drop(&mut self) {
        let Some((_, pid)) = self.active.take() else { return; };
        // An I/O error can leave serve() before orderly shutdown. Stop and
        // reap our worker before its caller sweeps or reuses these mounts.
        // First establish that this remains our unreaped child, so a stale
        // PID can never receive the signal.
        let mut status = 0;
        let observed = loop {
            // SAFETY: waitpid examines the worker child without blocking.
            let observed = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if observed >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted { break observed; }
        };
        if observed != 0 { return; }
        // SAFETY: this is our still-unreaped worker, not a reusable PID.
        unsafe { libc::kill(pid, libc::SIGKILL); }
        loop {
            // SAFETY: reap only the worker established above.
            if unsafe { libc::waitpid(pid, &mut status, 0) } >= 0
                || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted { break; }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_worker_reserves_id_and_does_not_start_more_workers() {
        let mut removals = Removals::default();
        let now = Instant::now();
        removals.enqueue("slow");
        removals.start(now, |_| Ok(42));
        removals.enqueue("slow");
        removals.enqueue("next");
        removals.start(now, |_| panic!("a blocked worker must not spawn another"));
        assert!(removals.contains("slow"));
        assert!(removals.contains("next"));
        assert!(!removals.contains("unrelated"));
        assert_eq!(removals.ids().count(), 2);
        assert!(!removals.reaped(41, 0, now));
        assert!(removals.reaped(42, 0, now));
        assert!(!removals.contains("slow"));
        removals.start(now, |id| { assert_eq!(id, "next"); Ok(43) });
        assert!(removals.reaped(43, 0, now));
        assert!(removals.is_empty());
    }

    #[test]
    fn failed_fork_and_failed_worker_keep_identity_reserved_and_back_off() {
        let mut removals = Removals::default();
        let now = Instant::now();
        removals.enqueue("victim");
        removals.start(now, |_| Err(io::Error::from_raw_os_error(libc::EAGAIN)));
        assert!(removals.contains("victim"));
        assert!(removals.needs_retry_poll());
        removals.start(now, |_| panic!("fork failure must back off"));
        removals.start(now + RETRY, |_| Ok(42));
        assert!(removals.reaped(42, 137, now + RETRY));
        assert!(removals.contains("victim"));
        removals.start(now + RETRY, |_| panic!("worker failure must back off"));
        removals.start(now + RETRY * 2, |_| Ok(43));
        assert!(!removals.reaped(42, 0, now + RETRY * 2));
        assert!(removals.contains("victim"));
        assert!(removals.reaped(43, 0, now + RETRY * 2));
        assert!(removals.is_empty());
    }

    #[test]
    fn shutdown_stops_retrying_failed_forks_at_its_deadline() {
        let mut removals = Removals::default();
        let now = Instant::now();
        let deadline = now + RETRY * 2;
        removals.enqueue("victim");
        removals.enqueue("next");
        let mut attempts = 0;
        assert!(removals.poll_shutdown(now, deadline, |_| {
            attempts += 1;
            Err(io::Error::from_raw_os_error(libc::EAGAIN))
        }));
        assert!(removals.poll_shutdown(now + RETRY / 2, deadline, |_| panic!("fork failure must back off")));
        assert!(removals.poll_shutdown(now + RETRY, deadline, |_| {
            attempts += 1;
            Err(io::Error::from_raw_os_error(libc::EAGAIN))
        }));
        assert!(!removals.poll_shutdown(deadline, deadline, |_| panic!("the deadline must stop new workers")));
        assert!(!removals.poll_shutdown(deadline + RETRY, deadline, |_| panic!("shutdown must not retry later")));
        assert_eq!(attempts, 2);
        assert_eq!(removals.ids().collect::<Vec<_>>(), ["victim", "next"]);
    }

    #[test]
    fn shutdown_waits_for_an_active_worker_without_retrying_it_after_deadline() {
        let mut removals = Removals::default();
        let now = Instant::now();
        let deadline = now + RETRY;
        removals.enqueue("active");
        removals.enqueue("next");
        assert!(removals.poll_shutdown(now, deadline, |_| Ok(42)));
        assert!(removals.poll_shutdown(deadline, deadline, |_| panic!("the active worker must be joined")));
        assert!(!removals.reaped(41, 0, deadline));
        assert!(removals.poll_shutdown(deadline + RETRY, deadline, |_| panic!("only the owned worker ends the wait")));
        assert!(removals.reaped(42, 1, deadline + RETRY));
        assert!(!removals.poll_shutdown(deadline + RETRY * 2, deadline, |_| panic!("failed workers must not restart after the deadline")));
        assert_eq!(removals.ids().collect::<Vec<_>>(), ["active", "next"]);
    }

    #[test]
    fn leaving_control_loop_terminates_and_reaps_its_active_worker() {
        // The disposable child uses only async-signal-safe syscalls, so this
        // test needs no engine, namespaces, mounts, privileges, or shell.
        // SAFETY: the child never returns to the multithreaded test harness.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: wait until the parent's worker guard terminates us.
            unsafe { loop { libc::pause(); } }
        }
        let mut removals = Removals::default();
        removals.enqueue("disposable-test-worker");
        removals.start(Instant::now(), |_| Ok(pid));
        drop(removals);
        let mut status = 0;
        // SAFETY: query only the disposable child this test created.
        let observed = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        let error = io::Error::last_os_error();
        if observed == 0 {
            // SAFETY: failure cleanup must not leave this test's child alive.
            unsafe { libc::kill(pid, libc::SIGKILL); libc::waitpid(pid, &mut status, 0); }
        }
        assert_eq!(observed, -1, "worker was not reaped before the control loop returned");
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
    }
}
