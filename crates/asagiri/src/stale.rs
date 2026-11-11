//! An older `asagiri` that terminals run instead of this one: how an update
//! or reinstall seems not to have happened. Mac and Linux.
use std::path::PathBuf;
use std::process::Command;

/// The first `asagiri` on the user's PATH, when it isn't this Asagiri's: an older
/// copy (a manual build in ~/.local/bin, Homebrew, a file in /usr/local/bin)
/// that runs instead of this one in every terminal. Reported, never deleted.
/// The PATH is the login shell's: a window opened from Finder or the app
/// menu has only the system's, without ~/.local/bin and the like.
pub fn stale_asagiri() -> Option<PathBuf> {
    let ours = std::fs::canonicalize(our_asagiri()?).ok()?;
    let shell = std::env::var("SHELL").ok().filter(|s| s.starts_with('/')).unwrap_or_else(|| "/bin/sh".into());
    let out = Command::new(&shell)
        .args(["-ilc", "command -v asagiri"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let found = String::from_utf8_lossy(&out.stdout).lines().rev().map(str::trim).find(|l| l.starts_with('/'))?.to_owned();
    // The snap's own command: /snap/bin/asagiri is a link to snapd's launcher,
    // which runs this snap's usr/bin/asagiri.
    if found == "/snap/bin/asagiri" && ours.starts_with("/snap/asagiri/") {
        return None;
    }
    match std::fs::canonicalize(&found) {
        Ok(real) if real == ours => None,
        _ => Some(PathBuf::from(found)),
    }
}

/// This Asagiri's `asagiri`: the running binary on the Mac (the window is the same
/// binary), the package's /usr/bin/asagiri beside Linux's separate window.
fn our_asagiri() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if exe.file_name().is_some_and(|n| n == "asagiri") {
        return Some(exe);
    }
    let snap = std::env::var_os("SNAP").map(|s| PathBuf::from(s).join("usr/bin/asagiri"));
    [snap, Some(PathBuf::from("/usr/bin/asagiri"))].into_iter().flatten().find(|p| p.is_file())
}
