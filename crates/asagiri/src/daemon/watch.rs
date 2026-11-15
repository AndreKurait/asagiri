//! Changes made on the Mac to bind-mounted files, sent into the VM as
//! inotify events.
//!
//! The VM sees the Mac's files over virtiofs, and Apple's virtiofs server
//! tells the guest nothing when a file changes on the Mac: a file watcher in
//! a container (Vite, webpack, nodemon, `cargo watch`) never heard of a save
//! made in an editor on the Mac. Reads were always fresh; only the event was
//! missing.
//!
//! This watches the bind sources of running containers with FSEvents and
//! sends each change to the guest agent as `OP\tPATH` (`Msg::FsEvents`),
//! which raises the inotify event Linux would for a change made in the VM
//! (crates/asagiri-init/src/linux/fsnotify.rs, and the guest kernel's
//! /proc/asagiri-fsnotify). Nothing is written to the files.
//!
//! A change a container makes itself reaches the Mac too, and comes back as
//! a second event: watchers see it twice, which they take as one change.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Weak};
use std::time::Duration;

use super::engine::Engine;

/// Where the Mac's filesystem is mounted in the VM.
const MAC_ROOT: &str = "/mnt/mac";
/// How often the set of bind sources is looked at again.
const RESCAN: Duration = Duration::from_secs(1);
/// FSEvents' latency: changes within it arrive together.
const LATENCY: f64 = 0.05;
/// At most this many lines per message to the guest.
const BATCH: usize = 512;
/// The callback never waits for the guest or grows an unbounded queue.
const QUEUED_BATCHES: usize = 16;

pub fn start(engine: Weak<Engine>) {
    let _ = std::thread::Builder::new().name("asagiri-watch".into()).spawn(move || run(engine));
}

fn run(engine: Weak<Engine>) {
    let (tx, rx) = sync_channel::<Vec<(String, u32)>>(QUEUED_BATCHES);
    let mut stream: Option<Stream> = None;
    let mut roots: Vec<Root> = Vec::new();
    let mut known = Known::new();
    let mut resume = 0;
    let mut rescan = false;
    loop {
        let Some(e) = engine.upgrade() else { return };
        let now = e.mac_bind_sources();
        if !roots.iter().map(|r| &r.path).eq(now.iter()) || (stream.is_none() && !now.is_empty()) {
            // The new stream picks up where the old one stopped, so changes
            // made in between are not lost; Known is kept with it.
            if let Some(old) = stream.take() {
                let last = old.1.clone();
                let lost = old.2.clone();
                drop(old); // Finish callbacks before reading their last id.
                resume = last.load(Ordering::Relaxed);
                rescan |= lost.swap(false, Ordering::Relaxed);
            }
            // A source new to this stream has nothing from before now: the
            // resumed stream would replay its folder's whole history.
            let cur = current_event_id();
            roots = now.into_iter().map(|path| roots.iter().find(|r| r.path == path).cloned().unwrap_or_else(|| Root::new(path, cur))).collect();
            if roots.is_empty() {
                resume = 0;
                rescan = false;
                known = Known::new();
            } else if resume == 0 {
                // Keep this point even if starting the stream fails, so a
                // later retry can recover changes made while it was down.
                resume = cur;
            }
            let since = since_for(resume);
            known.sync(&roots);
            if !roots.is_empty() {
                let dirs = watch_dirs(&roots.iter().map(|r| r.path.clone()).collect::<Vec<_>>());
                stream = Stream::start(&dirs, roots.clone(), tx.clone(), since, cur).inspect_err(|err| crate::diag::warn!("could not watch bind mounts for changes", error = err)).ok();
            }
        }
        drop(e);
        let mut first = match rx.recv_timeout(RESCAN) {
            Ok(events) => events,
            Err(RecvTimeoutError::Timeout) => Vec::new(),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        rescan |= stream.as_ref().is_some_and(|s| s.2.swap(false, Ordering::Relaxed));
        if rescan {
            first.push(("/".to_owned(), flag::LOST));
            rescan = false;
        }
        let lines = lines(first, &rx, &roots, &mut known);
        if lines.is_empty() || stream.is_none() {
            continue;
        }
        let Some(e) = engine.upgrade() else { return };
        if let Some(vm) = e.awake_link() {
            for chunk in lines.chunks(BATCH) {
                let _ = vm.fs_events(chunk);
            }
        }
    }
}

/// The folders to give FSEvents: a bind-mounted folder itself, a bind-mounted
/// file's folder (FSEvents watches folders).
fn watch_dirs(roots: &[String]) -> Vec<String> {
    let mut dirs: Vec<String> = roots
        .iter()
        .map(|r| if std::path::Path::new(r).is_dir() { r.clone() } else { std::path::Path::new(r).parent().map_or_else(|| "/".to_owned(), |p| p.display().to_string()) })
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// This batch and whatever else is already waiting, as guest lines: each
/// path looked at once, however many times it changed.
fn lines(first: Vec<(String, u32)>, rx: &Receiver<Vec<(String, u32)>>, roots: &[Root], known: &mut Known) -> Vec<String> {
    let mut order = Vec::new();
    let mut flags: HashMap<String, u32> = HashMap::new();
    let mut rescan = false;
    // Leave a continually busy producer for the next turn so bind changes
    // and shutdown remain observable during a filesystem storm.
    for (path, f) in first.into_iter().chain(rx.try_iter().take(QUEUED_BATCHES).flatten()) {
        // FSEvents lost changes under `path`: look at the files themselves.
        if f & flag::LOST != 0 {
            rescan = true;
        }
        let Some(path) = under(&path, roots) else { continue };
        match flags.get_mut(&path) {
            Some(old) => *old |= f,
            None => {
                flags.insert(path.clone(), f);
                order.push(path);
            }
        }
    }
    if rescan {
        // One bounded scan for the whole batch, even when thousands of
        // lost-change notices name overlapping directories.
        for path in known.rescan("/", roots) {
            // Named by a rescan, not by FSEvents: nothing is known to have
            // happened to it.
            if !flags.contains_key(&path) {
                flags.insert(path.clone(), flag::LOST);
                order.push(path);
            }
        }
    }
    let mut out = Vec::new();
    for path in order {
        for op in known.check(&path, flags[&path]) {
            out.push(format!("{op}\t{MAC_ROOT}{path}"));
        }
    }
    out
}

/// Where a new stream starts: after the last event the old one delivered,
/// or now if it delivered none.
fn since_for(last: u64) -> u64 {
    if last == 0 { SINCE_NOW } else { last }
}

/// A bind source being watched.
#[derive(Clone, Debug)]
struct Root {
    path: String,
    /// Its volume tells apart names that differ only by case. Looked at once
    /// here, not per event.
    sensitive: bool,
    /// Events at or below this id happened before the source was added.
    start_id: u64,
}

impl Root {
    fn new(path: String, start_id: u64) -> Root {
        let sensitive = case_sensitive(&path);
        Root { path, sensitive, start_id }
    }
}

/// Whether the volume of `path` is case-sensitive. Assume it is when that
/// can't be told, to avoid matching another source.
fn case_sensitive(path: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(path) else { return true };
    // SAFETY: a NUL-terminated path.
    unsafe { libc::pathconf(c.as_ptr(), libc::_PC_CASE_SENSITIVE) != 0 }
}

/// The newest event id FSEvents has issued so far.
fn current_event_id() -> u64 {
    // SAFETY: takes no arguments.
    unsafe { FSEventsGetCurrentEventId() }
}

/// `path` spelled as `root` if it is or is inside it, else None.
fn spell(path: &str, root: &str, sensitive: bool) -> Option<String> {
    if !valid_path(path) || !valid_path(root) {
        return None;
    }
    let root = trimmed(root);
    if root == "/" {
        return Some(path.to_owned());
    }
    let head = path.get(..root.len())?;
    let rest = &path[root.len()..];
    let same = if sensitive { head == root } else { head.eq_ignore_ascii_case(root) };
    (same && (rest.is_empty() || rest.starts_with('/'))).then(|| format!("{root}{rest}"))
}

/// The bind source `path` is or is inside, and `path` spelled as that source
/// is written. Mac folders ignore case unless their volume says otherwise,
/// and FSEvents may spell a path as it is on disk rather than as the bind
/// source was written.
fn root_of<'a>(path: &str, roots: &'a [Root]) -> Option<(&'a Root, String)> {
    roots.iter().find_map(|r| {
        let spelling = spell(path, &r.path, r.sensitive)?;
        let root = trimmed(&r.path);
        let head = path.get(..root.len())?;
        // A case-insensitive bind volume does not imply all its ancestors
        // are case-insensitive. Verify the actual source identity too.
        (head == root || same_file(head, root) || deleted_alias(head, root, r.sensitive))
            .then_some((r, spelling))
    })
}

fn under(path: &str, roots: &[Root]) -> Option<String> {
    root_of(path, roots).map(|(_, s)| s)
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/') && !path.contains(['\n', '\0'])
        && !path.split('/').any(|part| part == "..")
        && MAC_ROOT.len() + path.len() < 4096
}

fn same_file(a: &str, b: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

fn deleted_alias(a: &str, b: &str, sensitive: bool) -> bool {
    if sensitive || ![a, b].iter().all(|p| std::fs::symlink_metadata(p).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)) {
        return false;
    }
    let (Some(a), Some(b)) = (std::path::Path::new(a).parent(), std::path::Path::new(b).parent()) else { return false };
    a.to_str().zip(b.to_str()).is_some_and(|(a, b)| same_file(a, b) && !case_sensitive(b))
}

/// Whether an event is to be ignored: the end of a history replay, or a
/// change under a source from before the source was added. Root-changed and
/// mount events carry no id and always count; a path under no source (a
/// lost-changes event for a folder above one) is left to the caller. Under
/// nested sources the oldest decides: a parent added later must not hide a
/// change the already-watched child is owed.
fn stale(path: &str, id: u64, flags: u32, roots: &[Root]) -> bool {
    if flags & flag::HISTORY_DONE != 0 {
        return true;
    }
    if flags & (flag::ROOT_CHANGED | flag::MOUNT | flag::UNMOUNT) != 0 {
        return false;
    }
    roots
        .iter()
        .filter(|r| spell(path, &r.path, r.sensitive).is_some())
        .map(|r| r.start_id)
        .min()
        .is_some_and(|start| id <= start)
}

/// `dir` without trailing slashes ("/" stays).
fn trimmed(dir: &str) -> &str {
    let t = dir.trim_end_matches('/');
    if t.is_empty() { "/" } else { t }
}

/// What a path was when last looked at: enough to tell what changed.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Meta {
    dir: bool,
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    mode: u32,
    owner: (u32, u32),
}

impl Meta {
    /// The path's Meta, and whether it was made since `since`.
    fn of(path: &str, since: std::time::SystemTime) -> Option<(Meta, bool)> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path).ok()?;
        let new = m.created().is_ok_and(|t| t >= since);
        Some((Meta { dir: m.is_dir(), dev: m.dev(), ino: m.ino(), size: m.size(), mtime: (m.mtime(), m.mtime_nsec()), ctime: (m.ctime(), m.ctime_nsec()), mode: m.mode(), owner: (m.uid(), m.gid()) }, new))
    }
}

/// What changed, from what the path was and is. FSEvents' flags only say
/// which paths to look at: they keep a path's earlier changes (a chmod
/// after a write still says "modified"), and can be lost in a storm.
/// `new`: the path was made since watching began, so unknown means created.
/// `flags`: FSEvents' flags for the path, used only for a path not known
/// before, where there is nothing to compare with: a created or renamed
/// path that is there is created, a removed or renamed one that is gone is
/// deleted. A rescan (`flags` says changes were lost, with no item flag)
/// only records a path it hasn't seen.
fn ops(was: Option<Meta>, now: Option<Meta>, new: bool, flags: u32) -> Vec<char> {
    let quiet = flags & flag::LOST != 0 && flags & flag::ITEM == 0;
    match (was, now) {
        (None, None) => if flags & (flag::REMOVED | flag::RENAMED) != 0 { vec!['D'] } else { Vec::new() },
        (Some(_), None) => vec!['D'],
        // A file made with content: created, then written, as in the VM.
        (None, Some(n)) if new => if n.dir { vec!['C'] } else { vec!['C', 'M'] },
        // Here before watching began and not seen since: the flags say what
        // happened (a folder gets nothing but its creation: children say more).
        (None, Some(n)) if flags & (flag::CREATED | flag::RENAMED) != 0 => if n.dir { vec!['C'] } else { vec!['C', 'M'] },
        (None, Some(_)) if quiet => Vec::new(),
        (None, Some(n)) => if n.dir { Vec::new() } else { vec!['M'] },
        // Folders: a child's change moves a folder's times, which Linux
        // doesn't report on the folder; only its own mode or owner count.
        (Some(w), Some(n)) if n.dir => {
            if !w.dir {
                vec!['D', 'C']
            } else if (w.mode, w.owner) != (n.mode, n.owner) {
                vec!['A']
            } else {
                Vec::new()
            }
        }
        (Some(w), Some(n)) => {
            if w.dir {
                vec!['D', 'C', 'M']
            } else if (w.dev, w.ino) != (n.dev, n.ino) || w.size != n.size || w.mtime != n.mtime {
                // Written, or replaced by a rename onto it (how editors save).
                vec!['M']
            } else if w.ctime != n.ctime || (w.mode, w.owner) != (n.mode, n.owner) {
                vec!['A']
            } else {
                Vec::new()
            }
        }
    }
}

/// Every path looked at since watching began, as it was then.
struct Known {
    paths: HashMap<String, Meta>,
    /// Used for a path under no source in `starts`.
    since: std::time::SystemTime,
    /// When each source began to be watched: a file made before that isn't new.
    starts: Vec<(String, std::time::SystemTime)>,
}

/// Paths remembered at most; past it, cached knowledge is dropped (a
/// path then counts as "here before watching began").
const MAX_KNOWN: usize = 200_000;
/// Paths a rescan looks at at most.
const MAX_RESCAN: usize = 100_000;

impl Known {
    fn new() -> Known {
        // A second back: a file made just before the stream started is new.
        Known { paths: HashMap::new(), since: std::time::SystemTime::now() - Duration::from_secs(1), starts: Vec::new() }
    }

    /// Follows the set of sources: a source new here starts being watched
    /// now, one gone has its paths forgotten.
    fn sync(&mut self, roots: &[Root]) {
        self.starts.retain(|(p, _)| roots.iter().any(|r| r.path == *p));
        for r in roots {
            if !self.starts.iter().any(|(p, _)| *p == r.path) {
                // A second back: a file made just before it was added is new.
                self.starts.push((r.path.clone(), std::time::SystemTime::now() - Duration::from_secs(1)));
            }
        }
        self.paths.retain(|p, _| under(p, roots).is_some());
    }

    /// The time before which a file under the source of `path` isn't new.
    fn since_of(&self, path: &str) -> std::time::SystemTime {
        // `path` is spelled as its source, so an exact match is right.
        self.starts.iter().filter(|(r, _)| spell(path, r, true).is_some()).map(|(_, t)| *t).min().unwrap_or(self.since)
    }

    /// What changed at `path` since it was last looked at, remembering now.
    fn check(&mut self, path: &str, flags: u32) -> Vec<char> {
        let now = Meta::of(path, self.since_of(path));
        let was = match now {
            Some((m, _)) => {
                if self.paths.len() >= MAX_KNOWN && !self.paths.contains_key(path) {
                    self.paths.clear();
                }
                self.paths.insert(path.to_owned(), m)
            }
            None => self.paths.remove(path),
        };
        ops(was, now.map(|n| n.0), now.is_some_and(|n| n.1), flags)
    }

    /// The paths to look at after FSEvents lost changes under `dir`: what's
    /// there now and what was known there, inside the bind sources only.
    fn rescan(&self, dir: &str, roots: &[Root]) -> Vec<String> {
        self.rescan_limit(dir, roots, MAX_RESCAN)
    }

    fn rescan_limit(&self, dir: &str, roots: &[Root], limit: usize) -> Vec<String> {
        let dir = trimmed(dir);
        if limit == 0 || !valid_path(dir) {
            return Vec::new();
        }
        // Every source `dir` is, or is above ("/" is above all of them) ...
        let mut tops: Vec<String> = roots.iter().filter(|r| valid_path(&r.path) && (dir == "/" || spell(&r.path, dir, r.sensitive).is_some())).take(limit).map(|r| trimmed(&r.path).to_owned()).collect();
        // ... and `dir` itself when it is inside one.
        if let Some(d) = under(dir, roots) {
            tops.push(d);
        }
        tops.sort();
        tops.dedup();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        // Remember vanished paths too, so a lost deletion can be reported.
        for p in self.paths.keys().filter(|p| tops.iter().any(|t| spell(p, t, true).is_some())).take(limit) {
            seen.insert(p.clone());
            out.push(p.clone());
        }
        let mut stack = tops;
        let mut visited = HashSet::new();
        let mut examined = 0;
        while let Some(p) = stack.pop() {
            if out.len() >= limit || examined >= limit {
                break;
            }
            if !visited.insert(p.clone()) {
                continue;
            }
            examined += 1;
            if seen.insert(p.clone()) {
                out.push(p.clone());
            }
            // Do not walk symlinks, including a symlink used as a top.
            if !std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&p) {
                for entry in entries {
                    // Check inside the directory too: one directory can
                    // contain more entries than the entire scan budget.
                    if out.len() >= limit || examined >= limit {
                        break;
                    }
                    examined += 1;
                    let Ok(entry) = entry else { continue };
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    let child = format!("{}/{name}", p.trim_end_matches('/'));
                    let Some(child) = under(&child, roots) else { continue };
                    if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        stack.push(child.clone());
                    }
                    if seen.insert(child.clone()) {
                        out.push(child);
                    }
                }
            }
        }
        out
    }
}

mod flag {
    pub const MUST_SCAN_SUB_DIRS: u32 = 0x1;
    pub const USER_DROPPED: u32 = 0x2;
    pub const KERNEL_DROPPED: u32 = 0x4;
    /// Changes under the path were lost.
    pub const LOST: u32 = MUST_SCAN_SUB_DIRS | USER_DROPPED | KERNEL_DROPPED;
    pub const CREATED: u32 = 0x100;
    pub const REMOVED: u32 = 0x200;
    pub const RENAMED: u32 = 0x800;
    pub const HISTORY_DONE: u32 = 0x10;
    pub const ROOT_CHANGED: u32 = 0x20;
    pub const MOUNT: u32 = 0x40;
    pub const UNMOUNT: u32 = 0x80;
    /// Every flag that says what happened to an item.
    pub const ITEM: u32 = 0xff00;
}

// FSEvents (CoreServices), CoreFoundation and libdispatch, as much as this uses.
type FSEventStreamRef = *mut c_void;
type CFRef = *const c_void;

#[repr(C)]
struct FSEventStreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type Callback = extern "C" fn(FSEventStreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(alloc: CFRef, cb: Callback, ctx: *const FSEventStreamContext, paths: CFRef, since: u64, latency: f64, flags: u32) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: *mut c_void);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    #[cfg(test)]
    fn FSEventStreamFlushSync(stream: FSEventStreamRef);
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
    fn FSEventsGetCurrentEventId() -> u64;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: u8;
    fn CFStringCreateWithBytes(alloc: CFRef, bytes: *const u8, len: isize, encoding: u32, external: u8) -> CFRef;
    fn CFArrayCreate(alloc: CFRef, values: *const CFRef, count: isize, callbacks: *const u8) -> CFRef;
    fn CFRelease(cf: CFRef);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_sync_f(queue: *mut c_void, ctx: *mut c_void, work: extern "C" fn(*mut c_void));
}

const UTF8: u32 = 0x0800_0100;
const SINCE_NOW: u64 = u64::MAX;
const CREATE_NO_DEFER: u32 = 0x2;
const CREATE_FILE_EVENTS: u32 = 0x10;

/// What the callback has: where to send changes, and the bind sources, so
/// only their changes are copied out of FSEvents (a bind-mounted file is
/// watched through its whole folder, often the home folder).
struct Info {
    tx: SyncSender<Vec<(String, u32)>>,
    roots: Vec<Root>,
    warned: AtomicBool,
    /// The highest event id delivered so far.
    last: Arc<AtomicU64>,
    /// A dropped callback batch is recovered by a bounded source rescan.
    lost: Arc<AtomicBool>,
}

/// A running FSEvents stream; dropping it stops it.
struct Stream(FSEventStreamRef, Arc<AtomicU64>, Arc<AtomicBool>);

impl Stream {
    /// The last event id the stream delivered, 0 if none.
    #[cfg(test)]
    fn last_id(&self) -> u64 {
        self.1.load(Ordering::Relaxed)
    }
}

// SAFETY: the stream is only started and torn down from the thread that
// owns this, and FSEvents' calls are thread-safe.
unsafe impl Send for Stream {}

extern "C" fn on_events(_: FSEventStreamRef, info: *mut c_void, n: usize, paths: *mut c_void, flags: *const u32, ids: *const u64) {
    if n == 0 {
        return;
    }
    // SAFETY: FSEvents passes `n` C strings and `n` flags, and `info` is the
    // Info this stream was made with, freed only by `release_info` once the
    // stream is gone.
    let (info, paths, flags) = unsafe { (&*(info as *const Info), std::slice::from_raw_parts(paths as *const *const c_char, n), std::slice::from_raw_parts(flags, n)) };
    // SAFETY: as above, `n` ids.
    let ids = unsafe { std::slice::from_raw_parts(ids, n) };
    if let Some(&max) = ids.iter().max() {
        info.last.fetch_max(max, Ordering::Relaxed);
    }
    let mut events = Vec::with_capacity(BATCH);
    for ((p, &f), &id) in paths.iter().zip(flags).zip(ids) {
        let lost = f & flag::LOST != 0;
        if lost {
            info.lost.store(true, Ordering::Relaxed);
        }
        if lost && !info.warned.swap(true, Ordering::Relaxed) {
            crate::diag::info!("FSEvents dropped changes to bind mounts; looking at the files again");
        }
        // SAFETY: a NUL-terminated path from FSEvents.
        let Ok(path) = unsafe { CStr::from_ptr(*p) }.to_str() else { continue };
        if stale(path, id, f, &info.roots) {
            continue;
        }
        // A lost-changes event may name a folder above a bind source.
        if valid_path(path) && (lost || under(path, &info.roots).is_some()) {
            events.push((path.to_owned(), f));
            if events.len() == BATCH && !deliver(info, std::mem::take(&mut events)) {
                return;
            }
        }
    }
    if !events.is_empty() {
        deliver(info, events);
    }
}

fn deliver(info: &Info, events: Vec<(String, u32)>) -> bool {
    match info.tx.try_send(events) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            info.lost.store(true, Ordering::Relaxed);
            if !info.warned.swap(true, Ordering::Relaxed) {
                crate::diag::warn!("bind-mount file events exceeded the pending limit; rescanning watched sources");
            }
            false
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// The context's release: FSEvents calls it once the stream is released and
/// no callback can run again.
extern "C" fn release_info(info: *const c_void) {
    // SAFETY: the Box made in Stream::start, released once.
    drop(unsafe { Box::from_raw(info as *mut Info) });
}

fn queue() -> *mut c_void {
    static QUEUE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    // SAFETY: a serial queue made once and kept for the process's life.
    *QUEUE.get_or_init(|| unsafe { dispatch_queue_create(c"dev.asagiri.watch".as_ptr(), std::ptr::null()) } as usize) as *mut c_void
}

impl Stream {
    fn start(dirs: &[String], roots: Vec<Root>, tx: SyncSender<Vec<(String, u32)>>, since: u64, cur: u64) -> std::io::Result<Stream> {
        // Until an event comes, a restart resumes from where this one began.
        let last = Arc::new(AtomicU64::new(if since == SINCE_NOW { cur } else { since }));
        let lost = Arc::new(AtomicBool::new(false));
        let info = Box::into_raw(Box::new(Info { tx, roots, warned: AtomicBool::new(false), last: last.clone(), lost: lost.clone() }));
        // SAFETY: CoreFoundation objects made and released here. FSEvents
        // owns `info` from FSEventStreamCreate on and frees it through
        // `release_info`; before that it is still ours.
        unsafe {
            let strings: Vec<CFRef> = dirs.iter().map(|d| CFStringCreateWithBytes(std::ptr::null(), d.as_ptr(), d.len() as isize, UTF8, 0)).filter(|s| !s.is_null()).collect();
            let array = CFArrayCreate(std::ptr::null(), strings.as_ptr(), strings.len() as isize, &raw const kCFTypeArrayCallBacks);
            for s in &strings {
                CFRelease(*s);
            }
            let ctx = FSEventStreamContext { version: 0, info: info.cast(), retain: std::ptr::null(), release: release_info as *const c_void, copy_description: std::ptr::null() };
            let stream = FSEventStreamCreate(std::ptr::null(), on_events, &ctx, array, since, LATENCY, CREATE_FILE_EVENTS | CREATE_NO_DEFER);
            CFRelease(array);
            if stream.is_null() {
                drop(Box::from_raw(info));
                return Err(std::io::Error::other("FSEventStreamCreate failed"));
            }
            FSEventStreamSetDispatchQueue(stream, queue());
            if FSEventStreamStart(stream) == 0 {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                return Err(std::io::Error::other("FSEventStreamStart failed"));
            }
            Ok(Stream(stream, last, lost))
        }
    }
}

extern "C" fn tear_down(stream: *mut c_void) {
    // SAFETY: a started stream, torn down once.
    unsafe {
        FSEventStreamStop(stream);
        FSEventStreamInvalidate(stream);
        FSEventStreamRelease(stream);
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // On the stream's own serial queue, so no callback is running while
        // it stops. Never called from that queue (only from asagiri-watch).
        // SAFETY: the queue lives for the process; the stream is ours.
        unsafe { dispatch_sync_f(queue(), self.0, tear_down) };
    }
}

#[cfg(test)]
mod tests {
    use super::{Known, Meta, Root, flag, ops, since_for, stale, trimmed, under, SINCE_NOW};

    fn root(path: &str, sensitive: bool, start_id: u64) -> Root {
        Root { path: path.to_owned(), sensitive, start_id }
    }

    fn file(size: u64, mtime: i64, ctime: i64, mode: u32) -> Option<Meta> {
        Some(Meta { dir: false, dev: 1, ino: 1, size, mtime: (mtime, 0), ctime: (ctime, 0), mode, owner: (501, 20) })
    }

    #[test]
    fn what_each_change_means() {
        // Made since watching began: created and written; a folder created.
        assert_eq!(ops(None, file(1, 1, 1, 0o644), true, 0), ['C', 'M']);
        assert_eq!(ops(None, Some(Meta { dir: true, ..file(0, 1, 1, 0o755).unwrap() }), true, 0), ['C']);
        // Written, or replaced by a rename (a new inode).
        assert_eq!(ops(file(1, 1, 1, 0o644), file(2, 2, 2, 0o644), false, 0), ['M']);
        assert_eq!(ops(file(1, 1, 1, 0o644), Some(Meta { ino: 2, ..file(1, 1, 1, 0o644).unwrap() }), false, 0), ['M']);
        // chmod after a write: only the attributes, whatever FSEvents' flags say.
        assert_eq!(ops(file(2, 2, 2, 0o644), file(2, 2, 3, 0o600), false, 0), ['A']);
        // Nothing changed (a stale event): nothing.
        assert!(ops(file(1, 1, 1, 0o644), file(1, 1, 1, 0o644), false, 0).is_empty());
        // Gone.
        assert_eq!(ops(file(1, 1, 1, 0o644), None, false, 0), ['D']);
        assert!(ops(None, None, false, 0).is_empty());
        // A folder's times move with its children: not reported.
        let d = Meta { dir: true, ..file(0, 1, 1, 0o755).unwrap() };
        assert!(ops(Some(d), Some(Meta { mtime: (9, 0), ctime: (9, 0), ..d }), false, 0).is_empty());
        // Here before watching began, first seen now.
        assert_eq!(ops(None, file(1, 1, 1, 0o644), false, 0), ['M']);
    }

    #[test]
    fn known_paths() {
        let dir = std::env::temp_dir().join(format!("asagiri-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f").display().to_string();
        let mut k = Known::new();
        std::fs::write(&f, "a").unwrap();
        assert_eq!(k.check(&f, 0), ['C', 'M']);
        assert!(k.check(&f, 0).is_empty());
        std::fs::write(&f, "bb").unwrap();
        assert_eq!(k.check(&f, 0), ['M']);
        std::fs::remove_file(&f).unwrap();
        assert_eq!(k.check(&f, 0), ['D']);
        // A lost-changes rescan finds a file FSEvents never named.
        std::fs::write(&f, "c").unwrap();
        let roots = [root(&dir.display().to_string(), false, 0)];
        assert!(k.rescan(&roots[0].path, &roots).contains(&f));
        // From "/", or a path with a trailing slash, too.
        assert!(k.rescan("/", &roots).contains(&f));
        assert!(k.rescan(&format!("{}/", roots[0].path), &roots).contains(&f));
        assert!(k.rescan(&format!("{}//", roots[0].path), &roots).contains(&f));
        assert_eq!(trimmed("///"), "/");
        assert_eq!(trimmed("/a/b//"), "/a/b");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_paths_by_flags() {
        let was_here = file(1, 1, 1, 0o644);
        let d = Some(Meta { dir: true, ..file(0, 1, 1, 0o755).unwrap() });
        // Gone and never looked at: removed or renamed says deleted.
        assert_eq!(ops(None, None, false, flag::REMOVED), ['D']);
        assert_eq!(ops(None, None, false, flag::RENAMED), ['D']);
        assert!(ops(None, None, false, flag::CREATED).is_empty());
        assert!(ops(None, None, false, 0).is_empty());
        // There and never looked at: created or renamed (the new name of a
        // rename, whose birth is old) is created; a plain write is modified.
        assert_eq!(ops(None, was_here, false, flag::RENAMED), ['C', 'M']);
        assert_eq!(ops(None, was_here, false, flag::CREATED), ['C', 'M']);
        assert_eq!(ops(None, d, false, flag::RENAMED), ['C']);
        assert_eq!(ops(None, was_here, false, 0x1000), ['M']);
    }

    #[test]
    fn deleting_and_renaming_what_was_never_looked_at() {
        let dir = std::env::temp_dir().join(format!("asagiri-watch-del-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let (a, b, c) = (dir.join("a").display().to_string(), dir.join("b").display().to_string(), dir.join("sub/c").display().to_string());
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&c, "x").unwrap();
        let mut k = Known::new();
        k.since += std::time::Duration::from_secs(3600); // everything here predates watching
        std::fs::rename(&a, &b).unwrap();
        assert_eq!(k.check(&a, flag::RENAMED), ['D']);
        assert_eq!(k.check(&b, flag::RENAMED), ['C', 'M']);
        // rm -rf: the file and then the folder, both reported removed.
        std::fs::remove_dir_all(dir.join("sub")).unwrap();
        assert_eq!(k.check(&c, flag::REMOVED), ['D']);
        assert_eq!(k.check(&dir.join("sub").display().to_string(), flag::REMOVED), ['D']);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rescan_stays_silent_about_files_it_never_saw() {
        let dir = std::env::temp_dir().join(format!("asagiri-watch-rescan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (old, young) = (dir.join("old").display().to_string(), dir.join("young").display().to_string());
        std::fs::write(&old, "x").unwrap();
        let mut k = Known::new();
        k.since += std::time::Duration::from_secs(3600);
        assert!(k.check(&old, flag::LOST).is_empty());
        // Remembered now, so a real change is seen.
        std::fs::write(&old, "yy").unwrap();
        assert_eq!(k.check(&old, 0), ['M']);
        // Born since watching began: created even in a rescan.
        k.since -= std::time::Duration::from_secs(7200);
        std::fs::write(&young, "x").unwrap();
        assert_eq!(k.check(&young, flag::LOST), ['C', 'M']);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_stream_starts_where_the_old_one_stopped() {
        assert_eq!(since_for(0), SINCE_NOW);
        assert_eq!(since_for(12345), 12345);
    }

    #[test]
    fn only_inside_bind_sources() {
        let roots = [root("/Users/a/app", false, 0), root("/Users/a/.env", false, 0)];
        assert_eq!(under("/Users/a/app/src/main.js", &roots).as_deref(), Some("/Users/a/app/src/main.js"));
        assert_eq!(under("/Users/a/.env", &roots).as_deref(), Some("/Users/a/.env"));
        assert_eq!(under("/users/A/App/src/main.js", &roots), None, "a case variant must resolve to the same source");
        assert_eq!(under("/Users/a/application/x", &roots), None);
        assert_eq!(under("/Users/a/.envrc", &roots), None);
        assert_eq!(under("/Users/a/app/../secret", &roots), None);
        assert_eq!(under("/Users/a/app/bad\nM\t/mnt/mac/outside", &roots), None);
        assert_eq!(under("/Users/a/app/bad\0suffix", &roots), None);
        assert_eq!(under("/Users/a/app/x", &[root("/", true, 0)]).as_deref(), Some("/Users/a/app/x"));
        assert_eq!(under("/Users/a/app/x", &[root("/Users/a/app/", true, 0)]).as_deref(), Some("/Users/a/app/x"));
    }

    #[test]
    fn case_is_kept_on_case_sensitive_volumes() {
        let roots = [root("/Volumes/cs/app", true, 0)];
        assert_eq!(under("/Volumes/cs/app/x", &roots).as_deref(), Some("/Volumes/cs/app/x"));
        assert_eq!(under("/Volumes/cs/App/x", &roots), None);
        assert_eq!(under("/volumes/cs/app", &roots), None);
        let roots = [root("/Volumes/cs/app", false, 0)];
        assert_eq!(under("/Volumes/cs/App/x", &roots), None, "volume policy alone must not match another source");
    }

    #[test]
    fn events_from_before_a_source_was_added_are_dropped() {
        let roots = [root("/a", false, 0), root("/b", false, 100)];
        // The old source loses nothing; the new one nothing at or before 100.
        assert!(!stale("/a/x", 5, flag::CREATED, &roots));
        assert!(stale("/b/x", 100, flag::CREATED, &roots));
        assert!(stale("/b", 50, flag::CREATED, &roots));
        assert!(!stale("/b/x", 101, flag::CREATED, &roots));
        // History done, whatever its path or id.
        assert!(stale("/a/x", 500, flag::HISTORY_DONE, &roots));
        // Root changed and mounts have no id and always count.
        assert!(!stale("/b", 0, flag::ROOT_CHANGED, &roots));
        assert!(!stale("/b", 0, flag::MOUNT, &roots));
        assert!(!stale("/b", 0, flag::UNMOUNT, &roots));
        // A folder above a source (changes lost) is not under one.
        assert!(!stale("/", 1, flag::MUST_SCAN_SUB_DIRS, &roots));
    }

    #[test]
    fn a_new_parent_source_does_not_hide_its_watched_childs_changes() {
        // /u/app was watched from id 5; a container then binds /u (from 100).
        let roots = [root("/u", false, 100), root("/u/app", false, 5)];
        assert!(!stale("/u/app/x", 50, flag::CREATED, &roots));
        assert!(stale("/u/other", 50, flag::CREATED, &roots));
        assert!(stale("/u/app/x", 5, flag::CREATED, &roots));
    }

    #[test]
    fn a_new_source_counts_only_files_made_after_it_was_added() {
        let dir = std::env::temp_dir().join(format!("asagiri-watch-new-{}", std::process::id()));
        let (old_root, new_root) = (dir.join("old"), dir.join("new"));
        std::fs::create_dir_all(&old_root).unwrap();
        std::fs::create_dir_all(&new_root).unwrap();
        let (fo, fnw) = (old_root.join("f").display().to_string(), new_root.join("f").display().to_string());
        std::fs::write(&fo, "x").unwrap();
        std::fs::write(&fnw, "x").unwrap();
        let mut k = Known::new();
        k.sync(&[root(&old_root.display().to_string(), false, 0)]);
        // The second source is added an hour from now, by the clock of its start.
        k.sync(&[root(&old_root.display().to_string(), false, 0), root(&new_root.display().to_string(), false, 9)]);
        for (_, t) in k.starts.iter_mut().filter(|(p, _)| p.ends_with("new")) {
            *t += std::time::Duration::from_secs(3600);
        }
        // Both files were made a moment ago: new in the first source, not in the second.
        assert!(k.since_of(&fo) < k.since_of(&fnw));
        assert_eq!(k.check(&fo, 0), ['C', 'M']);
        assert_eq!(k.check(&fnw, 0), ['M']);
        // Dropping a source forgets its paths and start.
        k.sync(&[root(&old_root.display().to_string(), false, 0)]);
        assert!(!k.paths.contains_key(&fnw) && k.paths.contains_key(&fo));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    struct Temp(std::path::PathBuf);

    impl Temp {
        fn new() -> Temp {
            let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!("asagiri-filewatch-{}-{nonce}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Temp(path.canonicalize().unwrap())
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn case_variants_only_match_the_same_real_source() {
        let temp = Temp::new();
        let root = temp.0.join("MixedCase");
        std::fs::create_dir(&root).unwrap();
        let other = temp.0.join("mixedcase");
        let roots = [Root::new(root.to_str().unwrap().to_owned(), 0)];
        if !other.exists() {
            std::fs::create_dir(&other).unwrap();
        }
        let expected = super::same_file(root.to_str().unwrap(), other.to_str().unwrap());
        assert_eq!(under(other.to_str().unwrap(), &roots).is_some(), expected);
        std::fs::remove_dir(&root).unwrap();
        if expected {
            assert!(under(other.to_str().unwrap(), &roots).is_some(), "deleting a case-insensitive source still reports its deletion");
        }
    }

    #[test]
    fn rescans_are_bounded_deduplicated_and_do_not_walk_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        let bind = temp.0.join("bind");
        let outside = temp.0.join("outside");
        std::fs::create_dir_all(bind.join("nested")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"outside").unwrap();
        symlink(&outside, bind.join("escape")).unwrap();
        for i in 0..40 {
            std::fs::write(bind.join(format!("file-{i}")), b"before").unwrap();
            std::fs::write(bind.join("nested").join(format!("child-{i}")), b"nested").unwrap();
        }
        std::fs::write(bind.join("bad\nname"), b"not representable in the protocol").unwrap();
        let roots = [Root::new(bind.to_str().unwrap().into(), 0), Root::new(bind.join("nested").to_str().unwrap().into(), 0)];
        let mut known = Known::new();
        let deleted = bind.join("file-0").to_str().unwrap().to_owned();
        known.check(&deleted, 0);
        std::fs::remove_file(&deleted).unwrap();
        let limited = known.rescan_limit("/", &roots, 8);
        assert!(limited.len() <= 8, "{limited:?}");
        let all = known.rescan_limit("/", &roots, 1000);
        assert_eq!(all.iter().collect::<super::HashSet<_>>().len(), all.len());
        assert!(all.contains(&deleted), "remembered deletions must survive a rescan");
        assert!(all.iter().all(|p| under(p, &roots).is_some() && !p.ends_with("/secret") && !p.contains('\n')));
        assert!(all.iter().any(|p| p.ends_with("/nested/child-39")));
        assert!(known.rescan_limit(outside.to_str().unwrap(), &roots, 1000).is_empty());
    }

    #[test]
    fn lost_event_rescan_recovers_real_file_changes() {
        use std::os::unix::fs::PermissionsExt;
        let temp = Temp::new();
        let roots = [Root::new(temp.0.to_str().unwrap().into(), 0)];
        let deleted = temp.0.join("deleted");
        let edited = temp.0.join("edited");
        let metadata = temp.0.join("metadata");
        for p in [&deleted, &edited, &metadata] {
            std::fs::write(p, b"old").unwrap();
        }
        std::fs::set_permissions(&metadata, std::fs::Permissions::from_mode(0o640)).unwrap();
        let mut known = Known::new();
        for p in [&deleted, &edited, &metadata] {
            known.check(p.to_str().unwrap(), 0);
        }
        std::fs::remove_file(&deleted).unwrap();
        std::fs::write(&edited, b"new contents").unwrap();
        std::fs::set_permissions(&metadata, std::fs::Permissions::from_mode(0o600)).unwrap();
        let created = temp.0.join("created");
        std::fs::write(&created, b"new file").unwrap();
        let (_tx, rx) = super::sync_channel(super::QUEUED_BATCHES);
        let changes = super::lines(vec![("/".into(), flag::LOST)], &rx, &roots, &mut known);
        for (op, path) in [('D', &deleted), ('M', &edited), ('A', &metadata), ('C', &created), ('M', &created)] {
            assert!(changes.contains(&format!("{op}\t{}{}", super::MAC_ROOT, path.display())), "{changes:?}");
        }
        let repeated = super::lines(vec![("/".into(), flag::LOST)], &rx, &roots, &mut known);
        assert!(repeated.is_empty(), "unchanged files must not trigger repeated rebuilds: {repeated:?}");
    }

    #[test]
    fn adding_a_parent_preserves_the_childs_original_start_time() {
        let mut known = Known::new();
        let old = known.since;
        known.starts = vec![
            ("/owned".into(), old + std::time::Duration::from_secs(3600)),
            ("/owned/app".into(), old),
        ];
        assert_eq!(known.since_of("/owned/app/file"), old);
        assert_eq!(known.since_of("/owned/other"), old + std::time::Duration::from_secs(3600));
    }

    #[test]
    fn native_stream_resume_recovers_a_write_between_streams() {
        use super::{FSEventStreamFlushSync, Stream};
        let temp = Temp::new();
        let file = temp.0.join("watched");
        std::fs::write(&file, b"initial").unwrap();
        let cur = super::current_event_id();
        let roots = vec![Root::new(file.to_str().unwrap().into(), cur)];
        let dirs = vec![temp.0.to_str().unwrap().to_owned()];
        let (tx, rx) = super::sync_channel(super::QUEUED_BATCHES);
        let stream = Stream::start(&dirs, roots.clone(), tx.clone(), SINCE_NOW, cur).unwrap();
        let mut known = Known::new();
        known.check(file.to_str().unwrap(), 0);
        // Flush the stream and its callback before simulating reconfiguration.
        // SAFETY: a live stream, called away from its serial callback queue.
        unsafe { FSEventStreamFlushSync(stream.0) };
        while rx.try_recv().is_ok() {}
        let last = stream.last_id();
        assert_ne!(last, 0);
        drop(stream);
        std::fs::write(&file, b"written while no stream existed").unwrap();
        let stream = Stream::start(&dirs, roots.clone(), tx, last, super::current_event_id()).unwrap();
        let expected = format!("M\t{}{}", super::MAC_ROOT, file.display());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let first = rx.recv_timeout(remaining).expect("the restarted native stream missed the gap edit");
            let changes = super::lines(first, &rx, &roots, &mut known);
            assert!(changes.iter().all(|s| s.ends_with(file.to_str().unwrap())), "{changes:?}");
            if changes.contains(&expected) {
                break;
            }
        }
        drop(stream);
    }

    #[test]
    fn native_file_bind_receives_writes_atomic_saves_and_deletion() {
        use super::{FSEventStreamFlushSync, Stream, lines, sync_channel};
        let temp = Temp::new();
        let file = temp.0.join("watched.txt");
        std::fs::write(&file, b"initial").unwrap();
        let cur = super::current_event_id();
        let roots = vec![Root::new(file.to_str().unwrap().to_owned(), cur)];
        let (tx, rx) = sync_channel(super::QUEUED_BATCHES);
        let stream = Stream::start(&[temp.0.to_str().unwrap().to_owned()], roots.clone(), tx, SINCE_NOW, cur).unwrap();
        let mut known = Known::new();
        for expected in ['M', 'M', 'D'] {
            // SAFETY: a live stream, flushed away from its callback queue.
            unsafe { FSEventStreamFlushSync(stream.0) };
            while rx.try_recv().is_ok() {}
            if expected == 'D' {
                std::fs::remove_file(&file).unwrap();
            } else if std::fs::read(&file).unwrap() == b"initial" {
                std::fs::write(&file, b"edited").unwrap();
            } else {
                let replacement = temp.0.join("save.tmp");
                std::fs::write(&replacement, b"atomic-save").unwrap();
                std::fs::rename(replacement, &file).unwrap();
            }
            std::fs::write(temp.0.join("unrelated.txt"), b"outside the file bind").unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let first = rx.recv_timeout(remaining).expect("native FSEvents did not report the edit");
                let changes = lines(first, &rx, &roots, &mut known);
                assert!(changes.iter().all(|line| line.ends_with(file.to_str().unwrap())), "{changes:?}");
                if changes.iter().any(|line| line.starts_with(expected)) {
                    break;
                }
            }
        }
        drop(stream);
    }

    #[test]
    fn callback_batches_are_bounded_and_never_wait_for_a_slow_guest() {
        use super::{Info, Ordering, on_events, sync_channel};
        use std::ffi::CString;
        use std::sync::atomic::AtomicBool;
        let (tx, rx) = sync_channel(2);
        let (done_tx, done_rx) = sync_channel(1);
        let worker = std::thread::spawn(move || {
            // The warning is already emitted; the test never writes user logs.
            let mut info = Info { tx, roots: vec![root("/owned", true, 0)], warned: AtomicBool::new(true), last: super::Arc::new(super::AtomicU64::new(0)), lost: super::Arc::new(AtomicBool::new(false)) };
            let paths: Vec<_> = (0..super::BATCH * 5).map(|i| CString::new(format!("/owned/{i}")).unwrap()).collect();
            let mut pointers: Vec<_> = paths.iter().map(|p| p.as_ptr()).collect();
            let flags = vec![0x1000; paths.len()];
            let ids: Vec<_> = (1..=paths.len() as u64).collect();
            on_events(std::ptr::null_mut(), (&raw mut info).cast(), paths.len(), pointers.as_mut_ptr().cast(), flags.as_ptr(), ids.as_ptr());
            assert!(info.lost.load(Ordering::Relaxed));
            assert_eq!(info.last.load(Ordering::Relaxed), paths.len() as u64);
            assert!(info.warned.load(Ordering::Relaxed));
            done_tx.send(()).unwrap();
        });
        let result = done_rx.recv_timeout(std::time::Duration::from_secs(2));
        if result.is_err() {
            drop(rx); // Release an incorrectly blocking callback before failing.
            worker.join().unwrap();
            panic!("the filesystem callback waited for its consumer");
        }
        let batches: Vec<_> = rx.try_iter().collect();
        assert_eq!(batches.len(), 2);
        assert!(batches.iter().all(|batch| batch.len() == super::BATCH));
        worker.join().unwrap();
    }
}
