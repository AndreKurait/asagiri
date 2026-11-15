//! Bringing the WSL daemon back must not change whether its engine is off.
//! Only an explicit Start wakes it. One startup check owns the copy/start
//! sequence, and background recovery retains at most one pending retry.

use std::io;
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Intent {
    Start,
    Restore,
}

impl Intent {
    pub(crate) fn run(self, ensure: impl FnOnce() -> io::Result<()>, wake: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        ensure()?;
        match self {
            Self::Start => wake(),
            Self::Restore => Ok(()),
        }
    }
}

/// Check inside the guard: a caller waiting for another startup should
/// observe that daemon, instead of copying and starting it a second time.
pub(crate) fn ensure(starting: &Mutex<()>, alive: impl FnOnce() -> bool, start: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    let _one = starting.lock().unwrap_or_else(PoisonError::into_inner);
    if alive() { Ok(()) } else { start() }
}

#[derive(Debug)]
pub(crate) struct Recovery {
    pending: mpsc::SyncSender<()>,
}

impl Recovery {
    /// Never wait on the window's command loop. A full channel already
    /// holds a retry, including while the worker is finishing an attempt.
    pub(crate) fn request(&self) -> io::Result<()> {
        match self.pending.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(())) => Err(io::Error::new(io::ErrorKind::NotConnected, "daemon recovery worker stopped")),
        }
    }
}

pub(crate) fn recovery(
    mut restore: impl FnMut() -> io::Result<()> + Send + 'static,
    mut finished: impl FnMut(io::Result<()>) + Send + 'static,
) -> io::Result<(Recovery, JoinHandle<()>)> {
    let (pending, requests) = mpsc::sync_channel(1);
    let worker = thread::Builder::new().name("asagiri-revive".into()).spawn(move || {
        while requests.recv().is_ok() {
            finished(restore());
        }
    })?;
    Ok((Recovery { pending }, worker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, atomic::{AtomicBool, AtomicUsize, Ordering}};
    use std::time::Duration;

    #[test]
    fn restoring_a_missing_daemon_preserves_the_stopped_engine() {
        let starting = Mutex::new(());
        let alive = AtomicBool::new(false);
        let stopped = AtomicBool::new(true);
        Intent::Restore.run(
            || ensure(&starting, || alive.load(Ordering::SeqCst), || {
                alive.store(true, Ordering::SeqCst);
                Ok(())
            }),
            || {
                stopped.store(false, Ordering::SeqCst);
                Ok(())
            },
        ).unwrap();
        assert!(alive.load(Ordering::SeqCst));
        assert!(stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn an_explicit_start_wakes_only_after_the_daemon_answers() {
        let alive = AtomicBool::new(false);
        let stopped = AtomicBool::new(true);
        Intent::Start.run(
            || {
                alive.store(true, Ordering::SeqCst);
                Ok(())
            },
            || {
                assert!(alive.load(Ordering::SeqCst));
                stopped.store(false, Ordering::SeqCst);
                Ok(())
            },
        ).unwrap();
        assert!(!stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn a_failed_daemon_start_never_wakes_the_engine() {
        for intent in [Intent::Start, Intent::Restore] {
            let woke = AtomicBool::new(false);
            let result = intent.run(
                || Err(io::Error::new(io::ErrorKind::TimedOut, "daemon unavailable")),
                || {
                    woke.store(true, Ordering::SeqCst);
                    Ok(())
                },
            );
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
            assert!(!woke.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn concurrent_recovery_and_settings_checks_start_one_daemon() {
        let starting = Mutex::new(());
        let alive = AtomicBool::new(false);
        let copies = AtomicUsize::new(0);
        let callers = Barrier::new(8);
        thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    callers.wait();
                    ensure(&starting, || alive.load(Ordering::SeqCst), || {
                        copies.fetch_add(1, Ordering::SeqCst);
                        thread::yield_now();
                        alive.store(true, Ordering::SeqCst);
                        Ok(())
                    }).unwrap();
                });
            }
        });
        assert_eq!(copies.load(Ordering::SeqCst), 1);
        assert!(alive.load(Ordering::SeqCst));
    }

    #[test]
    fn a_stop_during_background_recovery_stays_stopped() {
        let stopped = AtomicBool::new(false);
        let (entered, started) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        thread::scope(|scope| {
            let stopped = &stopped;
            let worker = scope.spawn(move || Intent::Restore.run(
                || {
                    entered.send(()).unwrap();
                    resume.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                },
                || {
                    stopped.store(false, Ordering::SeqCst);
                    Ok(())
                },
            ));
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            // Stop owns no startup lock and can complete while restoring
            // daemon availability is still pending.
            stopped.store(true, Ordering::SeqCst);
            release.send(()).unwrap();
            worker.join().unwrap().unwrap();
        });
        assert!(stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn a_failed_or_panicked_start_releases_the_startup_guard() {
        let starting = Mutex::new(());
        assert!(ensure(&starting, || false, || Err(io::Error::other("start failed"))).is_err());
        let panicked = std::panic::catch_unwind(|| {
            let _ = ensure(&starting, || false, || panic!("startup panicked"));
        });
        assert!(panicked.is_err());
        assert!(ensure(&starting, || false, || Ok(())).is_ok());
    }

    #[test]
    fn busy_recovery_keeps_one_retry_and_accepts_work_after_a_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let (entered, started) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let (finished, results) = mpsc::channel();
        let (recovery, worker) = recovery(
            move || {
                let call = observed.fetch_add(1, Ordering::SeqCst);
                entered.send(call).unwrap();
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
                if call == 0 { Err(io::Error::other("WSL not ready")) } else { Ok(()) }
            },
            move |result| finished.send(result.is_ok()).unwrap(),
        ).unwrap();
        recovery.request().unwrap();
        assert_eq!(started.recv_timeout(Duration::from_secs(5)).unwrap(), 0);
        for _ in 0..20 {
            recovery.request().unwrap();
        }
        release.send(()).unwrap();
        assert!(!results.recv_timeout(Duration::from_secs(5)).unwrap());
        assert_eq!(started.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
        drop(recovery);
        release.send(()).unwrap();
        assert!(results.recv_timeout(Duration::from_secs(5)).unwrap());
        worker.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_disconnected_recovery_worker_is_reported() {
        let (pending, requests) = mpsc::sync_channel(1);
        drop(requests);
        let recovery = Recovery { pending };
        assert_eq!(recovery.request().unwrap_err().kind(), io::ErrorKind::NotConnected);
    }
}
