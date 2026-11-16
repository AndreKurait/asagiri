//! Run real personality syscalls under the production default filter.
//! Only a disposable child changes its own flags; it never execs untrusted code.

#[cfg(target_os = "linux")]
#[path = "../src/linux/seccomp.rs"]
#[allow(dead_code, unused_imports)]
mod seccomp;

#[cfg(target_os = "linux")]
fn probe() {
    use std::io;

    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Caps {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let mut header = Header { version: 0x2008_0522, pid: 0 };
    let empty = [Caps::default(); 2];
    // SAFETY: the initialized v3 records describe only this child process.
    assert_eq!(unsafe { libc::syscall(libc::SYS_capset, &header as *const Header, empty.as_ptr()) }, 0, "{}", io::Error::last_os_error());
    let mut observed = [Caps::default(); 2];
    // SAFETY: the v3 header and two writable records have the required sizes.
    assert_eq!(unsafe { libc::syscall(libc::SYS_capget, &mut header as *mut Header, observed.as_mut_ptr()) }, 0, "{}", io::Error::last_os_error());
    assert!(observed.iter().all(|c| c.effective == 0 && c.permitted == 0 && c.inheritable == 0));
    // SAFETY: no_new_privs can only constrain this child.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong) }, 0);

    // Query, change, verify and restore this child's flags before filtering.
    // This control distinguishes the Asagiri filter from a runner restriction.
    // SAFETY: the query value reads only the calling task's flags.
    let query = || unsafe { libc::syscall(libc::SYS_personality, 0xffff_ffff as libc::c_ulong) };
    let original = query();
    assert!(original >= 0, "personality query: {}", io::Error::last_os_error());
    let flag = 0x0004_0000 as libc::c_ulong; // ADDR_NO_RANDOMIZE
    assert_eq!(original as libc::c_ulong & flag, 0, "the control needs ASLR initially enabled");
    // SAFETY: personality changes only the calling task's flags.
    let result = unsafe { libc::syscall(libc::SYS_personality, original as libc::c_ulong | flag) };
    assert!(result >= 0, "unfiltered control: {}", io::Error::last_os_error());
    assert_eq!(query() as libc::c_ulong & flag, flag);
    // SAFETY: restore the flags observed in this same task.
    assert!(unsafe { libc::syscall(libc::SYS_personality, original as libc::c_ulong) } >= 0);
    assert_eq!(query(), original);
    println!("personality-control capabilities=0 changed-own-flags=true restored=true");

    seccomp::install(Some(0)).expect("install the production default filter");
    assert_eq!(query(), original, "the read-only query remains available");
    // SAFETY: the filter must reject this change to the same calling task.
    let result = unsafe { libc::syscall(libc::SYS_personality, original as libc::c_ulong | flag) };
    let error = io::Error::last_os_error();
    assert_eq!(result, -1, "the default filter allowed ADDR_NO_RANDOMIZE");
    assert_eq!(error.raw_os_error(), Some(libc::EPERM), "{error}");
    assert_eq!(query(), original, "a rejected operation must preserve the flags");
    println!("personality-default capabilities=0 denied=EPERM query=allowed flags-unchanged=true");
}

#[cfg(target_os = "linux")]
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--probe") {
        probe();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--probe")
        .output()
        .expect("start the isolated default-filter probe");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "the default-filter probe failed: {}", output.status);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("seccomp-default: real syscall validation runs on Linux");
}
