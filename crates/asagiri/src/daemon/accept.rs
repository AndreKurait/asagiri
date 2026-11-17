//! Avoid spinning or flooding the log when the API listener cannot accept.
use std::io;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

const RETRY_DELAY: Duration = Duration::from_millis(100);
const WARNING_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(super) struct Retry {
    cancelled: Receiver<()>,
    delay: Duration,
    last_warning: Option<Instant>,
    failures: u64,
}

impl Retry {
    pub(super) fn new(cancelled: Receiver<()>) -> Self {
        Self { cancelled, delay: RETRY_DELAY, last_warning: None, failures: 0 }
    }

    pub(super) fn next<T>(&mut self, mut accept: impl FnMut() -> io::Result<T>) -> Option<T> {
        loop {
            match self.cancelled.try_recv() {
                Err(TryRecvError::Empty) => {}
                _ => return None,
            }
            match accept() {
                Ok(connection) => return Some(connection),
                Err(error) => {
                    self.failures = self.failures.saturating_add(1);
                    let now = Instant::now();
                    if self.last_warning.is_none_or(|last| now.duration_since(last) >= WARNING_INTERVAL) {
                        crate::diag::warn!("cannot accept an API connection; retrying",
                            error = error, retry_ms = self.delay.as_millis(), failures = self.failures);
                        self.last_warning = Some(now);
                        self.failures = 0;
                    }
                    // SIGINT/SIGTERM interrupt the wait instead of waiting for
                    // the retry interval while the signal thread shuts down.
                    if !matches!(self.cancelled.recv_timeout(self.delay), Err(RecvTimeoutError::Timeout)) {
                        return None;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn persistent_accept_errors_wait_between_attempts_and_recover() {
        let (_cancel, cancelled) = mpsc::channel();
        let mut retry = Retry::new(cancelled);
        let mut attempts = Vec::new();
        let connection = retry.next(|| {
            attempts.push(Instant::now());
            if attempts.len() <= 3 {
                Err(io::Error::from_raw_os_error(libc::EMFILE))
            } else {
                Ok(42)
            }
        });
        assert_eq!(connection, Some(42));
        assert_eq!(attempts.len(), 4);
        for times in attempts.windows(2) {
            assert!(times[1].duration_since(times[0]) >= RETRY_DELAY);
        }
        // The later failures were suppressed after the first visible warning.
        assert_eq!(retry.failures, 2);
    }

    #[test]
    fn cancellation_interrupts_a_pending_retry_without_another_accept() {
        let (cancel, cancelled) = mpsc::channel();
        let (attempted, attempt) = mpsc::channel();
        let (completed, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut retry = Retry::new(cancelled);
            // A long pending delay makes this a cancellation test, independent
            // of scheduler precision around the ordinary 100ms retry interval.
            retry.delay = Duration::from_secs(30);
            let result = retry.next::<()>(|| {
                attempted.send(()).unwrap();
                Err(io::Error::from_raw_os_error(libc::EMFILE))
            });
            completed.send(result).unwrap();
        });
        attempt.recv_timeout(Duration::from_secs(5)).unwrap();
        cancel.send(()).unwrap();
        assert_eq!(completion.recv_timeout(Duration::from_secs(1)).unwrap(), None);
        worker.join().unwrap();
        assert!(attempt.try_recv().is_err());
    }

    #[test]
    fn cancellation_before_accept_does_not_call_the_listener() {
        let (cancel, cancelled) = mpsc::channel();
        cancel.send(()).unwrap();
        assert_eq!(Retry::new(cancelled).next::<()>(|| panic!("accept after shutdown")), None);
    }
}
