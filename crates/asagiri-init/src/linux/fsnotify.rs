//! Files the Mac changed under bind mounts, raised as inotify events in the
//! VM. Apple's virtiofs server tells the guest nothing when a file changes
//! on the Mac, so file watchers in containers (Vite, webpack, nodemon,
//! `cargo watch`) never heard of a save. The daemon watches the bind sources
//! with FSEvents and sends `OP\tPATH` lines (`Msg::FsEvents`); the guest
//! kernel's /proc/asagiri-fsnotify (guest/kernel/patches/0006) raises the
//! event for each, as a change made in the VM would. Nothing is written to
//! the files, so nothing goes back to the Mac.

use std::collections::{HashSet, VecDeque};
use std::io::Write;

/// Where the Mac's filesystem is mounted in the VM (mod.rs).
const MAC: &str = "/mnt/mac/";
const PROC: &str = "/proc/asagiri-fsnotify";
/// Lines waiting to be raised; past this, new ones are dropped (a watcher
/// that missed a change in a storm hears of the next one).
const MAX_PENDING: usize = 8192;
/// Lines raised per turn of the agent's loop, so a storm of changes on the
/// Mac doesn't hold up its other messages.
const PER_TURN: usize = 64;

/// Changes waiting to be raised. Raised from the agent's own loop, a few at
/// a time, not a thread: the agent must stay single-threaded (it starts
/// containers with clone3, and a thread breaks setuid in the child).
#[derive(Default)]
pub struct Pending(VecDeque<String>, HashSet<String>);

impl Pending {
    /// Queues the lines that name a file under the Mac's mount, once each.
    pub fn add<'a>(&mut self, lines: impl Iterator<Item = &'a str>) {
        for line in valid(lines) {
            if self.0.len() < MAX_PENDING && self.1.insert(line.clone()) {
                self.0.push_back(line);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Raises up to PER_TURN lines. A kernel without the interface (or a
    /// host-process engine) drops them all.
    pub fn raise_some(&mut self) {
        let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(PROC) else {
            self.0.clear();
            self.1.clear();
            return;
        };
        // One write per line: the kernel answers each with its own result,
        // and a file gone again by now (ENOENT) must not drop the rest.
        for line in self.0.drain(..PER_TURN.min(self.0.len())) {
            self.1.remove(&line);
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// The lines the kernel takes, each ending in a newline: `C`, `M`, `A` or
/// `D`, a tab, and a path under /mnt/mac with no `..` and no newline.
fn valid<'a>(lines: impl Iterator<Item = &'a str>) -> impl Iterator<Item = String> {
    lines.filter_map(|line| {
        let (op, path) = line.split_once('\t')?;
        let ok = matches!(op, "C" | "M" | "A" | "D")
            && path.starts_with(MAC)
            && path.len() > MAC.len()
            && path.len() < 4096
            && !path.contains('\n')
            && !path.contains('\0')
            && !path.split('/').any(|part| part == "..");
        ok.then(|| format!("{op}\t{path}\n"))
    })
}

#[cfg(test)]
mod tests {
    use super::{Pending, valid};

    #[test]
    fn only_paths_under_the_macs_mount() {
        let lines = ["M\t/mnt/mac/Users/a/app/main.js", "D\t/etc/passwd", "C\t/mnt/mac/../etc/x", "X\t/mnt/mac/Users/a", "M\t/mnt/mac/", "nonsense"];
        assert_eq!(valid(lines.into_iter()).collect::<Vec<_>>(), ["M\t/mnt/mac/Users/a/app/main.js\n"]);
    }

    #[test]
    fn a_change_waiting_is_queued_once() {
        let mut p = Pending::default();
        p.add(["M\t/mnt/mac/a", "M\t/mnt/mac/b"].into_iter());
        p.add(["M\t/mnt/mac/a"].into_iter());
        assert_eq!(p.0.len(), 2);
    }

    #[test]
    fn queued_changes_and_kernel_path_lengths_are_bounded() {
        let mut p = Pending::default();
        let lines: Vec<_> = (0..super::MAX_PENDING + 100).map(|i| format!("M\t/mnt/mac/{i}")).collect();
        p.add(lines.iter().map(String::as_str));
        assert_eq!(p.0.len(), super::MAX_PENDING);
        assert_eq!(p.1.len(), super::MAX_PENDING);
        let invalid = ["M\t/mnt/mac/a\0other".to_owned(), format!("M\t/mnt/mac/{}", "a".repeat(4096))];
        assert!(valid(invalid.iter().map(String::as_str)).next().is_none());
    }
}
