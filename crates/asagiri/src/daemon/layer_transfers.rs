//! Serialize the same layer, while bounding work on independent layers.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex, Weak};

#[derive(Default)]
struct State {
    by_digest: HashMap<String, Weak<Mutex<()>>>,
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

    pub(super) fn run<T>(&self, digest: &str, operation: impl FnOnce() -> T) -> T {
        let layer = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // Completed work must not retain a lock object for every image
            // this daemon has ever seen.
            if state.by_digest.len() >= 64 {
                state.by_digest.retain(|_, lock| lock.strong_count() > 0);
            }
            match state.by_digest.get(digest).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = std::sync::Arc::new(Mutex::new(()));
                    state.by_digest.insert(digest.to_owned(), std::sync::Arc::downgrade(&lock));
                    lock
                }
            }
        };
        // Take the digest lock before a capacity permit: waiters for the
        // same layer must not occupy every slot and starve unrelated work.
        let _layer = layer.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.active == self.limit {
            state = self.available.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.active += 1;
        drop(state);
        let _permit = Permit(self);
        operation()
    }
}

struct Permit<'a>(&'a LayerTransfers);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active -= 1;
        self.0.available.notify_one();
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
                    transfers.run("shared", || {
                        if !ready.load(Ordering::SeqCst) {
                            uploads.fetch_add(1, Ordering::SeqCst);
                            std::thread::yield_now();
                            ready.store(true, Ordering::SeqCst);
                        }
                    });
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
                scope.spawn(move || transfers.run(&index.to_string(), || {
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(count, Ordering::SeqCst);
                    entered.send(()).unwrap();
                    let (lock, wake) = &*release;
                    let mut done = lock.lock().unwrap();
                    while !*done {
                        done = wake.wait(done).unwrap();
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                }));
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
            transfers.run("retry", || panic!("interrupted transfer"));
        }));
        assert!(failed.is_err());
        assert_eq!(transfers.state.lock().unwrap().active, 0);
        assert_eq!(transfers.run("retry", || 42), 42);
        assert_eq!(transfers.run("another", || 7), 7);
    }
}
