//! Serialize the same layer, while bounding work on independent layers.

use std::collections::HashSet;
use std::io;
use std::sync::{Condvar, Mutex};

#[derive(Default)]
struct State {
    by_digest: HashSet<String>,
    active: usize,
}

pub(super) struct LayerTransfers {
    state: Mutex<State>,
    available: Condvar,
    limit: usize,
}

impl Default for LayerTransfers {
    fn default() -> Self {
        Self::new(std::thread::available_parallelism().map_or(2, |n| n.get().min(4)))
    }
}

impl LayerTransfers {
    fn new(limit: usize) -> Self {
        assert!(limit > 0);
        Self { state: Mutex::default(), available: Condvar::new(), limit }
    }

    pub(super) fn run<T>(&self, digest: &str, cancelled: &dyn Fn() -> bool, operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        loop {
            // Call external cancellation code without holding our state lock.
            let cancelled = cancelled();
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // Digest ownership and capacity are claimed together. A waiter
            // owns neither, so a busy digest cannot fill every capacity slot.
            if let Some(active) = super::layer_admission::transition(
                state.active, self.limit, true, state.by_digest.contains(digest), cancelled,
            ) {
                state.by_digest.insert(digest.to_owned());
                state.active = active;
                let _permit = Permit { transfers: self, digest };
                drop(state);
                return operation();
            }
            if cancelled {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "image preparation cancelled"));
            }
            // Cancellation need not notify this condvar. Recheck it while
            // waiting for either a digest owner or an unrelated transfer.
            drop(self.available.wait_timeout(state, std::time::Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner()));
        }
    }
}

struct Permit<'a> {
    transfers: &'a LayerTransfers,
    digest: &'a str,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut state = self.transfers.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(state.by_digest.remove(self.digest), "layer permit lost its digest");
        state.active = super::layer_admission::transition(
            state.active, self.transfers.limit, false, false, false,
        ).expect("layer permit count is inconsistent");
        // A single wake can select a waiter for another still-busy digest.
        self.transfers.available.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, atomic::{AtomicBool, AtomicUsize, Ordering}};

    #[test]
    fn same_digest_is_initialized_once_under_contention() {
        let transfers = LayerTransfers::new(4);
        let start = Barrier::new(9);
        let ready = AtomicBool::new(false);
        let uploads = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    start.wait();
                    transfers.run("shared", &|| false, || {
                        if !ready.load(Ordering::SeqCst) {
                            uploads.fetch_add(1, Ordering::SeqCst);
                            std::thread::yield_now();
                            ready.store(true, Ordering::SeqCst);
                        }
                        Ok(())
                    }).unwrap();
                });
            }
            start.wait();
        });
        assert_eq!(uploads.load(Ordering::SeqCst), 1);
        assert_eq!(transfers.state.lock().unwrap().active, 0);
    }

    #[test]
    fn independent_layers_overlap_without_exceeding_capacity() {
        let transfers = LayerTransfers::new(2);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (entered, arrivals) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            for index in 0..6 {
                let entered = entered.clone();
                let release = release.clone();
                let (transfers, active, peak) = (&transfers, &active, &peak);
                scope.spawn(move || transfers.run(&index.to_string(), &|| false, || {
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(count, Ordering::SeqCst);
                    entered.send(()).unwrap();
                    let (lock, wake) = &*release;
                    let mut done = lock.lock().unwrap();
                    while !*done {
                        done = wake.wait(done).unwrap();
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                }).unwrap());
            }
            let first = arrivals.recv_timeout(std::time::Duration::from_secs(5));
            let second = arrivals.recv_timeout(std::time::Duration::from_secs(5));
            let extra = arrivals.try_recv();
            *release.0.lock().unwrap() = true;
            release.1.notify_all();
            assert!(first.is_ok() && second.is_ok());
            assert!(extra.is_err());
        });
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(transfers.state.lock().unwrap().active, 0);
    }

    #[test]
    fn panic_releases_capacity_and_the_digest_can_be_retried() {
        let transfers = LayerTransfers::new(1);
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: io::Result<()> = transfers.run("retry", &|| false, || panic!("interrupted transfer"));
        }));
        assert!(failed.is_err());
        assert_eq!(transfers.state.lock().unwrap().active, 0);
        assert_eq!(transfers.run("retry", &|| false, || Ok(42)).unwrap(), 42);
        assert_eq!(transfers.run("another", &|| false, || Ok(7)).unwrap(), 7);
    }

    #[test]
    fn cancelled_and_failed_operations_leave_no_digest_or_capacity() {
        let transfers = LayerTransfers::new(1);
        let cancelled: io::Result<()> = transfers.run("cancelled", &|| true, || panic!("cancelled work ran"));
        assert_eq!(cancelled.unwrap_err().kind(), io::ErrorKind::Interrupted);
        let failed: io::Result<()> = transfers.run("retry", &|| false, || Err(io::Error::other("transfer failed")));
        assert_eq!(failed.unwrap_err().to_string(), "transfer failed");
        assert_eq!(transfers.run("retry", &|| false, || Ok(7)).unwrap(), 7);
        let state = transfers.state.lock().unwrap();
        assert_eq!(state.active, 0);
        assert!(state.by_digest.is_empty());
    }

    fn cancelled_waiter(same_digest: bool) {
        use std::sync::mpsc;
        use std::time::Duration;

        let transfers = Arc::new(LayerTransfers::new(1));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (entered, running) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let owner = {
            let transfers = transfers.clone();
            std::thread::spawn(move || {
                transfers.run("held", &|| false, || {
                    entered.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                }).unwrap();
            })
        };
        running.recv_timeout(Duration::from_secs(5)).expect("owner started");
        let (attempt, attempted) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        let waiter = {
            let transfers = transfers.clone();
            let cancelled = cancelled.clone();
            std::thread::spawn(move || {
                attempt.send(()).unwrap();
                let value = transfers.run(
                    if same_digest { "held" } else { "other" },
                    &|| cancelled.load(Ordering::SeqCst), || Ok(()),
                );
                finished.send(value).unwrap();
            })
        };
        attempted.recv_timeout(Duration::from_secs(5)).unwrap();
        let before_cancel = result.recv_timeout(Duration::from_millis(30));
        cancelled.store(true, Ordering::SeqCst);
        let after_cancel = result.recv_timeout(Duration::from_secs(2));
        let active_before_release = transfers.state.lock().unwrap().active;
        // Cleanup must finish even if a regression made the waiter block.
        release.send(()).unwrap();
        owner.join().unwrap();
        waiter.join().unwrap();
        assert!(matches!(before_cancel, Err(mpsc::RecvTimeoutError::Timeout)));
        assert_eq!(active_before_release, 1, "a waiter cannot consume another permit");
        assert!(
            matches!(&after_cancel, Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted),
            "cancelled waiter remained blocked by another transfer: {after_cancel:?}"
        );
        assert_eq!(transfers.state.lock().unwrap().active, 0);
        transfers.run("held", &|| false, || Ok(())).unwrap();
        transfers.run("other", &|| false, || Ok(())).unwrap();
        let state = transfers.state.lock().unwrap();
        assert_eq!(state.active, 0);
        assert!(state.by_digest.is_empty());
    }

    #[test]
    fn cancellation_does_not_wait_for_a_digest_owner() {
        cancelled_waiter(true);
    }

    #[test]
    fn cancellation_does_not_wait_for_transfer_capacity() {
        cancelled_waiter(false);
    }
}
