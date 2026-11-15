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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::Weak;
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

pub fn start(engine: Weak<Engine>) {
    let _ = std::thread::Builder::new().name("asagiri-watch".into()).spawn(move || run(engine));
}

fn run(engine: Weak<Engine>) {
    let (tx, rx) = channel::<Vec<(String, u32)>>();
    let mut stream: Option<Stream> = None;
    let mut roots: Vec<String> = Vec::new();
    let mut known = Known::default();
    loop {
        let Some(e) = engine.upgrade() else { return };
        let now = e.mac_bind_sources();
        if now != roots {
            stream = None; // stops the old one
            roots = now;
            known = Known::default();
            if !roots.is_empty() {
                stream = Stream::start(&watch_dirs(&roots), roots.clone(), tx.clone()).inspect_err(|err| crate::diag::warn!("could not watch bind mounts for changes", error = err)).ok();
            }
        }
        drop(e);
        let first = match rx.recv_timeout(RESCAN) {
            Ok(events) => events,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
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

/// This batch and whatever else is already waiting, as guest lines: one
/// path once, with all its flags, however many times it changed.
fn lines(first: Vec<(String, u32)>, rx: &Receiver<Vec<(String, u32)>>, roots: &[String], known: &mut Known) -> Vec<String> {
    let mut order = Vec::new();
    let mut flags: HashMap<String, u32> = HashMap::new();
    for (path, f) in first.into_iter().chain(rx.try_iter().flatten()) {
        let Some(path) = under(&path, roots) else { continue };
        match flags.get_mut(&path) {
            Some(all) => *all |= f,
            None => {
                flags.insert(path.clone(), f);
                order.push(path);
            }
        }
    }
    let mut out = Vec::new();
    for path in order {
        let exists = std::fs::symlink_metadata(&path).is_ok();
        for op in ops(flags[&path], exists, known.seen(&path, exists)) {
            out.push(format!("{op}\t{MAC_ROOT}{path}"));
        }
    }
    out
}

/// `path` spelled as the bind source it is or is inside, or None for any
/// other path. Mac folders ignore case, and FSEvents may spell a path as
/// it is on disk rather than as the bind source was written.
fn under(path: &str, roots: &[String]) -> Option<String> {
    roots.iter().find_map(|r| {
        let head = path.get(..r.len())?;
        let rest = &path[r.len()..];
        (head.eq_ignore_ascii_case(r) && (rest.is_empty() || rest.starts_with('/'))).then(|| format!("{r}{rest}"))
    })
}

/// The paths seen to exist, so a file FSEvents still marks "created" is
/// reported as modified once it has been reported at all: FSEvents keeps a
/// path's flags from earlier changes in the same moment.
#[derive(Default)]
struct Known(HashSet<String>);

impl Known {
    /// Whether `path` was already known to exist, noting what it is now.
    fn seen(&mut self, path: &str, exists: bool) -> bool {
        if !exists {
            self.0.remove(path);
            return false;
        }
        if self.0.len() > 100_000 {
            self.0.clear();
        }
        !self.0.insert(path.to_owned())
    }
}

mod flag {
    pub const MUST_SCAN_SUB_DIRS: u32 = 0x1;
    pub const USER_DROPPED: u32 = 0x2;
    pub const KERNEL_DROPPED: u32 = 0x4;
    pub const CREATED: u32 = 0x100;
    pub const REMOVED: u32 = 0x200;
    pub const INODE_META_MOD: u32 = 0x400;
    pub const RENAMED: u32 = 0x800;
    pub const MODIFIED: u32 = 0x1000;
    pub const FINDER_INFO_MOD: u32 = 0x2000;
    pub const CHANGE_OWNER: u32 = 0x4000;
    pub const XATTR_MOD: u32 = 0x8000;
}

/// What a change means in the VM, from FSEvents' flags and whether the path
/// is there now: a path that's gone was deleted (or renamed away); one that
/// appeared was created (or renamed here); then modified and attribute
/// changes. `known`: it was there before, so "created" is a stale flag.
fn ops(flags: u32, exists: bool, known: bool) -> Vec<char> {
    use flag::*;
    if !exists {
        return if flags & (REMOVED | RENAMED | CREATED) != 0 { vec!['D'] } else { Vec::new() };
    }
    let mut v = Vec::new();
    let appeared = flags & (CREATED | RENAMED) != 0 && !known;
    if appeared {
        v.push('C');
    }
    // A file replaced by a rename onto it (how most editors save) changed,
    // whether or not it was reported before.
    if flags & (MODIFIED | RENAMED) != 0 {
        v.push('M');
    }
    if !appeared && v.is_empty() && flags & (INODE_META_MOD | CHANGE_OWNER | XATTR_MOD | FINDER_INFO_MOD) != 0 {
        v.push('A');
    }
    v
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
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
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
    tx: Sender<Vec<(String, u32)>>,
    roots: Vec<String>,
    warned: AtomicBool,
}

/// A running FSEvents stream; dropping it stops it.
struct Stream(FSEventStreamRef);

// SAFETY: the stream is only started and torn down from the thread that
// owns this, and FSEvents' calls are thread-safe.
unsafe impl Send for Stream {}

extern "C" fn on_events(_: FSEventStreamRef, info: *mut c_void, n: usize, paths: *mut c_void, flags: *const u32, _: *const u64) {
    // SAFETY: FSEvents passes `n` C strings and `n` flags, and `info` is the
    // Info this stream was made with, freed only by `release_info` once the
    // stream is gone.
    let (info, paths, flags) = unsafe { (&*(info as *const Info), std::slice::from_raw_parts(paths as *const *const c_char, n), std::slice::from_raw_parts(flags, n)) };
    let mut events = Vec::new();
    for (p, &f) in paths.iter().zip(flags) {
        if f & (flag::MUST_SCAN_SUB_DIRS | flag::USER_DROPPED | flag::KERNEL_DROPPED) != 0 && !info.warned.swap(true, Ordering::Relaxed) {
            crate::diag::warn!("FSEvents dropped changes to bind mounts; some file watchers in containers may miss them");
        }
        // SAFETY: a NUL-terminated path from FSEvents.
        let Ok(path) = unsafe { CStr::from_ptr(*p) }.to_str() else { continue };
        if under(path, &info.roots).is_some() {
            events.push((path.to_owned(), f));
        }
    }
    if !events.is_empty() {
        let _ = info.tx.send(events);
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
    fn start(dirs: &[String], roots: Vec<String>, tx: Sender<Vec<(String, u32)>>) -> std::io::Result<Stream> {
        let info = Box::into_raw(Box::new(Info { tx, roots, warned: AtomicBool::new(false) }));
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
            let stream = FSEventStreamCreate(std::ptr::null(), on_events, &ctx, array, SINCE_NOW, LATENCY, CREATE_FILE_EVENTS | CREATE_NO_DEFER);
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
            Ok(Stream(stream))
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
    use super::flag::*;
    use super::{Known, ops, under};

    #[test]
    fn what_each_change_means() {
        // A new file, then saved again: created once, modified after.
        assert_eq!(ops(CREATED | MODIFIED, true, false), ['C', 'M']);
        assert_eq!(ops(CREATED | MODIFIED, true, true), ['M']);
        // Saved by an editor that writes a copy and renames it over.
        assert_eq!(ops(RENAMED, true, true), ['M']);
        assert_eq!(ops(RENAMED, true, false), ['C', 'M']);
        // Gone, however it went.
        assert_eq!(ops(REMOVED, false, true), ['D']);
        assert_eq!(ops(RENAMED, false, true), ['D']);
        assert_eq!(ops(CREATED | REMOVED, false, false), ['D']);
        // chmod.
        assert_eq!(ops(INODE_META_MOD, true, true), ['A']);
        assert!(ops(0, true, true).is_empty());
    }

    #[test]
    fn known_paths() {
        let mut k = Known::default();
        assert!(!k.seen("/a", true));
        assert!(k.seen("/a", true));
        assert!(!k.seen("/a", false));
        assert!(!k.seen("/a", true));
    }

    #[test]
    fn only_inside_bind_sources() {
        let roots = ["/Users/a/app".to_owned(), "/Users/a/.env".to_owned()];
        assert_eq!(under("/Users/a/app/src/main.js", &roots).as_deref(), Some("/Users/a/app/src/main.js"));
        assert_eq!(under("/Users/a/.env", &roots).as_deref(), Some("/Users/a/.env"));
        assert_eq!(under("/users/A/App/src/main.js", &roots).as_deref(), Some("/Users/a/app/src/main.js"));
        assert_eq!(under("/Users/a/application/x", &roots), None);
        assert_eq!(under("/Users/a/.envrc", &roots), None);
    }
}
