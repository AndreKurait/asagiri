//! One agent-owned overlay per container, shared by its process and copies.
//! A lease ends only after its child is reaped. Completed copies and stopped
//! processes leave an idle overlay ready for the next copy or start, without
//! an intervening filesystem-wide flush. Removal, reconciliation and shutdown
//! reclaim it asynchronously; its ID stays reserved through worker reap.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::time::Instant;

use asagiri_proto::StrList;
use super::container::{self, Paths};
use super::removal::Removals;

#[derive(Default)]
struct Root {
    users: usize,
    restoring: bool,
    mounted: bool,
    remove: bool,
    discard_when_idle: bool,
    retired: Option<bool>,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Root>,
    workers: Removals,
    // Removal may be requested after an unmount worker has already started.
    // Remember what that worker actually did before deciding the next step.
    active: Option<(String, bool)>,
    events: Vec<(String, bool)>,
    preparing_copies: usize,
}

impl State {
    fn finish(&mut self, id: String, removed: bool) {
        if self.entries[&id].remove && !removed {
            self.workers.enqueue(&id);
        } else {
            self.entries.remove(&id);
            self.events.push((id, false));
        }
    }
}

#[derive(Default)]
pub(super) struct Roots(RefCell<State>);

impl Roots {
    pub fn acquire(&self, paths: &Paths, id: &str, layers: StrList<'_>) -> io::Result<PathBuf> {
        self.lease(paths, id, Some(layers))
    }

    /// Diff/commit read the upper directory, without requiring another mount.
    pub fn hold_upper(&self, paths: &Paths, id: &str) -> io::Result<PathBuf> {
        self.lease(paths, id, None)
    }

    /// Give one stopped-receiver restore worker exclusive access to its directory.
    /// Unmount any cached overlay first so rootfs restore can replace the upper.
    pub fn restore_directory(&self, paths: &Paths, id: &str) -> io::Result<Option<PathBuf>> {
        {
            let mut state = self.0.borrow_mut();
            if let Some(root) = state.entries.get(id) {
                if root.users != 0 { return Err(io::Error::other("conflict: receiver filesystem is busy")); }
                if root.mounted {
                    state.workers.enqueue(id);
                    return Ok(None);
                }
            }
        }
        self.hold_upper(paths, id)?;
        self.0.borrow_mut().entries.get_mut(id).unwrap().restoring = true;
        Ok(Some(paths.containers.join(id)))
    }

    fn lease(&self, paths: &Paths, id: &str, layers: Option<StrList<'_>>) -> io::Result<PathBuf> {
        container::valid_id(id)?;
        let mut state = self.0.borrow_mut();
        if state.entries.get(id).is_some_and(|root| root.restoring) {
            return Err(io::Error::other("conflict: receiver checkpoint restore in progress"));
        }
        if state.entries.get(id).is_some_and(|root| root.remove) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "container is being removed"));
        }
        if state.workers.contains(id) || state.entries.get(id).is_some_and(|root| root.retired.is_some()) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, asagiri_proto::ROOT_CLEANUP_PENDING));
        }
        let mounted = state.entries.get(id).is_some_and(|root| root.mounted);
        if let Some(layers) = layers.filter(|_| !mounted) {
            // Do not count a failed mount as a lease. An existing upper-only
            // reader, if any, keeps its own lease unchanged.
            container::mount_rootfs(paths, id, layers)?;
        }
        if layers.is_none() {
            // A created container may be diffed or committed before its first
            // start or archive copy has initialized the writable directory.
            std::fs::create_dir_all(paths.containers.join(id).join("upper"))?;
        }
        let root = state.entries.entry(id.to_owned()).or_default();
        root.mounted |= layers.is_some();
        root.users += 1;
        Ok(paths.containers.join(id).join(if layers.is_some() { "rootfs" } else { "upper" }))
    }

    pub fn release(&self, id: &str) {
        self.release_lease(id, false);
    }

    /// A completed copy or stopped process may be followed immediately by a
    /// start. Keep its exact overlay until removal, reconciliation or shutdown.
    /// Failed/abandoned copies and failed starts use ordinary release.
    pub fn release_idle(&self, id: &str) {
        self.release_lease(id, true);
    }

    fn release_lease(&self, id: &str, keep_idle: bool) {
        let mut state = self.0.borrow_mut();
        let Some(root) = state.entries.get_mut(id) else { return };
        assert!(root.users > 0, "a filesystem lease must be released exactly once");
        root.restoring = false;
        root.users -= 1;
        if root.users == 0 {
            root.remove |= root.discard_when_idle;
            if keep_idle && root.mounted && !root.remove { return; }
            if root.mounted || root.remove {
                state.workers.enqueue(id);
                state.events.push((id.to_owned(), true));
            } else {
                state.entries.remove(id);
            }
        }
    }

    pub fn remove(&self, id: &str) {
        let mut state = self.0.borrow_mut();
        let root = state.entries.entry(id.to_owned()).or_default();
        root.remove = true;
        if root.users == 0 && root.retired.is_none() { state.workers.enqueue(id); }
        state.events.push((id.to_owned(), true));
    }

    pub fn removing(&self, id: &str) -> bool {
        self.0.borrow().entries.get(id).is_some_and(|root| root.remove)
    }

    /// Host reconciliation may forget a stopped container without a separate
    /// Remove message. Retire idle roots through the same owned cleanup path.
    /// Active processes and copies remain protected until their leases end,
    /// but remember an omitted ID so their last release cannot cache it again.
    /// A newer snapshot can retain an active root; explicit removal stays set.
    pub fn retain(&self, keep: StrList<'_>) {
        let mut state = self.0.borrow_mut();
        let mut idle = Vec::new();
        for (id, root) in &mut state.entries {
            root.discard_when_idle = !keep.iter().any(|kept| kept == id.as_str());
            if root.users == 0 && root.discard_when_idle { idle.push(id.clone()); }
        }
        drop(state);
        for id in idle { self.remove(&id); }
    }

    pub fn cleaning(&self, id: &str) -> bool {
        let state = self.0.borrow();
        state.workers.contains(id) || state.entries.get(id).is_some_and(|root| root.remove || root.retired.is_some())
    }

    pub fn needs_retry_poll(&self) -> bool { self.0.borrow().workers.needs_retry_poll() }

    pub fn start(&self, paths: &Paths, now: Instant) {
        let mut state = self.0.borrow_mut();
        let State { entries, workers, active, .. } = &mut *state;
        workers.start(now, |id| {
            let remove = entries[id].remove;
            let pid = container::begin_root_cleanup(paths, id, remove)?;
            *active = Some((id.to_owned(), remove));
            Ok(pid)
        });
    }

    pub fn reaped(&self, pid: libc::pid_t, code: i32, now: Instant) -> bool {
        let mut state = self.0.borrow_mut();
        if !state.workers.reaped(pid, code, now) { return false; }
        let (id, removed) = state.active.take().expect("an owned cleanup worker");
        if code == 0 {
            let root = state.entries.get_mut(&id).expect("ID retained through cleanup");
            assert_eq!(root.users, 0);
            root.mounted = false;
            if state.preparing_copies > 0 {
                state.entries.get_mut(&id).expect("retained root").retired = Some(removed);
            } else {
                state.finish(id, removed);
            }
        }
        true
    }

    pub fn events(&self) -> Vec<(String, bool)> {
        std::mem::take(&mut self.0.borrow_mut().events)
    }

    /// unshare initially clones every agent mount. A stopped-copy worker
    /// promptly pivots into its own root and detaches that old tree, then
    /// reports ready. Until then, even a reaped unmount worker may have left
    /// an overlay referenced by this short-lived namespace copy.
    pub fn begin_copy(&self) { self.0.borrow_mut().preparing_copies += 1; }

    pub fn copy_ready(&self) {
        let mut state = self.0.borrow_mut();
        assert!(state.preparing_copies > 0);
        state.preparing_copies -= 1;
        if state.preparing_copies == 0 {
            let ready: Vec<_> = state.entries.iter().filter_map(|(id, root)| root.retired.map(|removed| (id.clone(), removed))).collect();
            for (id, removed) in ready {
                state.entries.get_mut(&id).expect("retired root").retired = None;
                state.finish(id, removed);
            }
        }
    }

    /// Stop admitting retries at the deadline, but always join an active
    /// worker. Live leases are never unmounted by the shutdown fallback.
    pub fn poll_shutdown(&self, paths: &Paths, now: Instant, deadline: Instant) -> bool {
        let mut state = self.0.borrow_mut();
        let State { entries, workers, active, .. } = &mut *state;
        // Completed copies and stopped processes can leave mounted roots with
        // no live user. Reclaim them before shutdown through the same queue.
        for (id, root) in entries.iter() {
            if root.users == 0 && root.retired.is_none() { workers.enqueue(id); }
        }
        workers.poll_shutdown(now, deadline, |id| {
            let remove = entries[id].remove;
            let pid = container::begin_root_cleanup(paths, id, remove)?;
            *active = Some((id.to_owned(), remove));
            Ok(pid)
        })
    }

    pub fn unfinished(&self) -> Vec<String> {
        self.0.borrow().entries.keys().cloned().collect()
    }
}
