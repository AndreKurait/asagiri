//! Overlay mounts without putting every layer in mount(2)'s one-page buffer.

use std::ffi::{CStr, CString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

use super::sys;

pub fn mount(lowers: &[&str], upper: &Path, work: &Path, target: &Path) -> io::Result<()> {
    if individual_layers(lowers, upper, work, target)? {
        return Ok(());
    }
    legacy(lowers, upper, work, target)
}

fn unsupported(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EOPNOTSUPP | libc::EINVAL))
}

fn configure(fd: &OwnedFd, key: &CStr, value: &CStr) -> io::Result<()> {
    // SAFETY: the context is owned and both strings outlive the syscall.
    let ret = unsafe { libc::syscall(libc::SYS_fsconfig, fd.as_raw_fd(), libc::FSCONFIG_SET_STRING, key.as_ptr(), value.as_ptr(), 0) };
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn directory(fd: &OwnedFd, key: &CStr, path: &Path) -> io::Result<()> {
    let value = sys::cstr(path)?;
    if value.as_bytes_with_nul().len() <= 256 {
        return configure(fd, key, &value);
    }
    // fsconfig strings have a 256-byte limit. Newer kernels also accept a
    // directory FD, retaining the actual path in mountinfo. Older ones fall
    // back to mount(2), which can still fit a few layers with long data paths.
    let dir = fs::File::open(path)?;
    // SAFETY: both owned descriptors and the key outlive the syscall.
    let ret = unsafe { libc::syscall(libc::SYS_fsconfig, fd.as_raw_fd(), libc::FSCONFIG_SET_FD, key.as_ptr(), std::ptr::null::<libc::c_void>(), dir.as_raw_fd()) };
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// False only when this kernel cannot configure the individual layer paths.
/// Permission, I/O and missing-path errors are returned without retrying them.
fn individual_layers(lowers: &[&str], upper: &Path, work: &Path, target: &Path) -> io::Result<bool> {
    // SAFETY: a constant filesystem name and documented UAPI flag.
    let raw = unsafe { libc::syscall(libc::SYS_fsopen, c"overlay".as_ptr(), libc::FSOPEN_CLOEXEC) };
    if raw < 0 {
        let error = io::Error::last_os_error();
        return if unsupported(&error) { Ok(false) } else { Err(error) };
    }
    // SAFETY: a fresh descriptor returned by fsopen.
    let context = unsafe { OwnedFd::from_raw_fd(raw as _) };
    for (index, lower) in lowers.iter().enumerate() {
        let value = CString::new(*lower).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "layer path contains NUL"))?;
        if let Err(error) = configure(&context, c"lowerdir+", &value) {
            return if index == 0 && unsupported(&error) { Ok(false) } else { Err(error) };
        }
    }
    for (key, path) in [(c"upperdir", upper), (c"workdir", work)] {
        if let Err(error) = directory(&context, key, path) {
            return if unsupported(&error) { Ok(false) } else { Err(error) };
        }
    }
    // Keep complete writable files for diff/commit and the kernel's index
    // protection against two overlays using the same upper directory.
    configure(&context, c"metacopy", c"off")?;
    configure(&context, c"redirect_dir", c"off")?;
    // SAFETY: create the configured filesystem; this command takes no data.
    let ret = unsafe { libc::syscall(libc::SYS_fsconfig, context.as_raw_fd(), libc::FSCONFIG_CMD_CREATE, std::ptr::null::<libc::c_char>(), std::ptr::null::<libc::c_void>(), 0) };
    if ret < 0 {
        let error = io::Error::last_os_error();
        // Legacy filesystem contexts can defer rejecting lowerdir+ until
        // creation, even though the earlier configuration calls succeeded.
        return if unsupported(&error) { Ok(false) } else { Err(error) };
    }
    // SAFETY: the configured filesystem remains owned through this call.
    let raw = unsafe { libc::syscall(libc::SYS_fsmount, context.as_raw_fd(), libc::FSMOUNT_CLOEXEC, 0) };
    if raw < 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: a fresh detached-mount descriptor returned by fsmount.
    let mounted = unsafe { OwnedFd::from_raw_fd(raw as _) };
    let target = sys::cstr(target)?;
    // SAFETY: move our detached mount onto the existing target directory.
    sys::check(unsafe { libc::syscall(libc::SYS_move_mount, mounted.as_raw_fd(), c"".as_ptr(), libc::AT_FDCWD, target.as_ptr(), libc::MOVE_MOUNT_F_EMPTY_PATH) }, "attach overlay mount")?;
    Ok(true)
}

fn legacy(lowers: &[&str], upper: &Path, work: &Path, target: &Path) -> io::Result<()> {
    let options = CString::new(format!("lowerdir={},upperdir={},workdir={},metacopy=off,redirect_dir=off", lowers.join(":"), upper.display(), work.display()))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "overlay path contains NUL"))?;
    // SAFETY: reading the current kernel's page size does not change state.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 { return Err(io::Error::other("cannot determine the overlay mount option limit")); }
    if options.as_bytes_with_nul().len() > page as usize {
        return Err(io::Error::new(io::ErrorKind::Unsupported, format!(
            "legacy overlay mount options for {} layers need {} bytes, exceeding this kernel's {page}-byte limit; individual layer configuration is required",
            lowers.len(), options.as_bytes_with_nul().len()
        )));
    }
    sys::mount(c"overlay", &sys::cstr(target)?, c"overlay", 0, Some(&options))
}

#[cfg(test)]
#[path = "overlay_tests.rs"]
mod tests;
