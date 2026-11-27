//! Preflight update members using macOS libarchive, the parser behind bsdtar.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};
use std::ptr::{NonNull, null_mut};

#[link(name = "archive")]
unsafe extern "C" {
    fn archive_read_new() -> *mut c_void;
    fn archive_read_support_filter_gzip(reader: *mut c_void) -> c_int;
    fn archive_read_support_format_tar(reader: *mut c_void) -> c_int;
    fn archive_read_open_filename(
        reader: *mut c_void,
        path: *const c_char,
        block_size: usize,
    ) -> c_int;
    fn archive_read_next_header(reader: *mut c_void, entry: *mut *mut c_void) -> c_int;
    fn archive_read_free(reader: *mut c_void) -> c_int;
    fn archive_error_string(reader: *mut c_void) -> *const c_char;
    fn archive_entry_pathname(entry: *mut c_void) -> *const c_char;
    fn archive_entry_hardlink(entry: *mut c_void) -> *const c_char;
    fn archive_entry_symlink(entry: *mut c_void) -> *const c_char;
    fn archive_entry_filetype(entry: *mut c_void) -> libc::mode_t;
}

struct Reader(NonNull<c_void>);

impl Drop for Reader {
    fn drop(&mut self) {
        // SAFETY: This reader owns the allocation from archive_read_new.
        unsafe { archive_read_free(self.0.as_ptr()) };
    }
}

fn invalid(reason: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid update archive: {reason}"),
    )
}

impl Reader {
    fn check(&self, status: c_int) -> io::Result<()> {
        if status == 0 {
            return Ok(());
        }
        // SAFETY: The error string belongs to the live reader. Copy it before
        // the next libarchive call can invalidate it.
        let detail = unsafe {
            let ptr = archive_error_string(self.0.as_ptr());
            (!ptr.is_null()).then(|| CStr::from_ptr(ptr).to_string_lossy().into_owned())
        };
        Err(invalid(
            detail.unwrap_or_else(|| format!("libarchive status {status}")),
        ))
    }
}

fn relative_components(path: &Path) -> Option<impl Iterator<Item = Component<'_>>> {
    let parts = path.components().filter(|part| *part != Component::CurDir);
    (!path.as_os_str().is_empty()
        && parts
            .clone()
            .all(|part| matches!(part, Component::Normal(_))))
    .then_some(parts)
}

// Adapted from the preflight proposed in Kiri PR #419 (01b9860344d05ffa54a80655a6137d48670c8d84).
// libarchive supplies effective names after handling tar extension records.
pub(super) fn check(path: &Path) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(invalid)?;
    // SAFETY: The reader is owned until this function returns. A successful
    // next_header borrows an entry from it until the next reader operation.
    unsafe {
        let reader = Reader(
            NonNull::new(archive_read_new())
                .ok_or_else(|| io::Error::other("could not allocate archive reader"))?,
        );
        let raw = reader.0.as_ptr();
        reader.check(archive_read_support_filter_gzip(raw))?;
        reader.check(archive_read_support_format_tar(raw))?;
        reader.check(archive_read_open_filename(raw, path.as_ptr(), 64 * 1024))?;
        let mut found_bundle = false;
        loop {
            let mut entry = null_mut();
            let status = archive_read_next_header(raw, &mut entry);
            if status == 1 {
                // ARCHIVE_EOF
                break;
            }
            reader.check(status)?;
            let name = archive_entry_pathname(entry);
            if name.is_null() {
                return Err(invalid("member has no name"));
            }
            let name = Path::new(std::ffi::OsStr::from_bytes(CStr::from_ptr(name).to_bytes()));
            let kind = archive_entry_filetype(entry);
            let hardlink = archive_entry_hardlink(entry);
            let symlink = archive_entry_symlink(entry);
            let mut parts = relative_components(name).ok_or_else(|| invalid(name.display()))?;
            let root = parts.next().map(|part| part.as_os_str());
            let at_root = parts.next().is_none();
            if root == Some("._Asagiri.app".as_ref())
                && at_root
                && kind == libc::S_IFREG
                && hardlink.is_null()
                && symlink.is_null()
            {
                continue;
            }
            if root != Some("Asagiri.app".as_ref()) {
                return Err(invalid(format_args!(
                    "member outside Asagiri.app: {}",
                    name.display()
                )));
            }
            if at_root {
                if kind != libc::S_IFDIR || !hardlink.is_null() || !symlink.is_null() {
                    return Err(invalid("Asagiri.app must be a directory"));
                }
                found_bundle = true;
            } else if !hardlink.is_null() {
                let target = Path::new(std::ffi::OsStr::from_bytes(
                    CStr::from_ptr(hardlink).to_bytes(),
                ));
                let mut parts =
                    relative_components(target).ok_or_else(|| invalid(name.display()))?;
                if parts.next().map(|part| part.as_os_str()) != Some("Asagiri.app".as_ref())
                    || parts.next().is_none()
                {
                    return Err(invalid(format_args!(
                        "hardlink outside the bundle: {}",
                        name.display()
                    )));
                }
            } else if !symlink.is_null() {
                let target = Path::new(std::ffi::OsStr::from_bytes(
                    CStr::from_ptr(symlink).to_bytes(),
                ));
                // Framework links can point down and across within the same directory.
                // Parent components can resolve differently through another symlink.
                if relative_components(target).is_none() {
                    return Err(invalid(format_args!(
                        "unsupported symlink target at {}",
                        name.display()
                    )));
                }
            } else if !matches!(kind, libc::S_IFREG | libc::S_IFDIR) {
                return Err(invalid(format_args!(
                    "unsupported member type at {}",
                    name.display()
                )));
            }
        }
        if !found_bundle {
            return Err(invalid("missing Asagiri.app directory"));
        }
    }
    Ok(())
}
