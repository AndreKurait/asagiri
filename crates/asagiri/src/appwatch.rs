//! Noticing that Asagiri's own program is gone from where it started: the app
//! dragged to the Trash, a development build's folder deleted, or (on
//! Windows, where a running program can't be deleted) its folder renamed
//! or moved. What's left running can't do its job any more (on the Mac the
//! VM's kernel went with the app), yet it still holds what "Build with
//! Asagiri" took, buildx's builder for every context, and every `docker
//! build` fails on it. The daemon (daemon/mod.rs) and asagiri-win both watch,
//! hand the builder back and stop.
//!
//! The same file serves the Mac and Linux (crates/asagiri) and Windows
//! (asagiri-win includes it).

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often the program's path is looked at.
pub const EVERY: Duration = Duration::from_secs(30);

/// Calls `on_gone` once, on its own thread, when the program this process
/// started from has been missing from its path on two looks in a row (an
/// update swaps the app in well under that). The path is the one it started
/// with: on a Linux host a package upgrade puts a new file there, which
/// counts as still there.
pub fn watch(on_gone: impl FnOnce(&Path) + Send + 'static) {
    if let Ok(exe) = std::env::current_exe() {
        watch_path(exe, EVERY, on_gone);
    }
}

fn watch_path(path: PathBuf, every: Duration, on_gone: impl FnOnce(&Path) + Send + 'static) {
    let _ = std::thread::Builder::new().name("asa-app-watch".into()).spawn(move || {
        wait_until_gone(&path, || std::thread::sleep(every));
        on_gone(&path);
    });
}

fn wait_until_gone(path: &Path, mut wait: impl FnMut()) {
    let mut missing = 0;
    while missing < 2 {
        wait();
        missing = if path.exists() { 0 } else { missing + 1 };
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn a_program_moved_away_is_noticed_once_and_one_put_back_is_not() {
        let dir = std::env::temp_dir().join(format!("asagiri-appwatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app")).unwrap();
        let exe = dir.join("app").join("asagiri");
        std::fs::write(&exe, b"program").unwrap();
        let (tx, rx) = mpsc::channel();
        super::watch_path(exe.clone(), Duration::from_millis(20), move |p| tx.send(p.to_owned()).unwrap());
        // There all along: nothing.
        assert!(rx.recv_timeout(Duration::from_millis(150)).is_err());
        // Its folder renamed, as Windows allows for a running program.
        std::fs::rename(dir.join("app"), dir.join("moved")).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), exe);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_brief_swap_is_not_taken_for_gone() {
        let dir = std::env::temp_dir().join(format!("asagiri-appwatch-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("asagiri");
        std::fs::write(&exe, b"old").unwrap();
        // Observe the replacement between checks without relying on the
        // scheduler waking this test before the watcher's next deadline.
        // A later removal must still take two more checks to be reported.
        let mut polls = 0;
        super::wait_until_gone(&exe, || {
            match polls {
                0 | 2 => std::fs::remove_file(&exe).unwrap(),
                1 => std::fs::write(&exe, b"new").unwrap(),
                3 => {}
                _ => panic!("a missing program was not noticed"),
            }
            polls += 1;
        });
        assert_eq!(polls, 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
