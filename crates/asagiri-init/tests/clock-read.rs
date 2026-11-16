//! Exercise the production filter against real Linux clock-state queries.
//! A disposable child drops all capabilities and only passes modes=0.

#[cfg(target_os = "linux")]
#[path = "../src/linux/seccomp.rs"]
#[allow(dead_code, unused_imports)] // The standalone harness does not run the module's libtest bodies.
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
    // SAFETY: the v3 capability header takes two initialized data records.
    // This only drops capabilities in the disposable probe process.
    assert_eq!(unsafe { libc::syscall(libc::SYS_capset, &header as *const Header, empty.as_ptr()) }, 0, "{}", io::Error::last_os_error());
    let mut observed = [Caps::default(); 2];
    // SAFETY: a writable header and the required two output records.
    assert_eq!(unsafe { libc::syscall(libc::SYS_capget, &mut header as *mut Header, observed.as_mut_ptr()) }, 0, "{}", io::Error::last_os_error());
    assert!(observed.iter().all(|c| c.effective == 0 && c.permitted == 0 && c.inheritable == 0));
    // SAFETY: no_new_privs only constrains this disposable process.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong) }, 0, "{}", io::Error::last_os_error());

    fn read_clock(name: &str, nr: libc::c_long, filtered: bool) {
        // SAFETY: an all-zero timex is valid; modes=0 requests only a read.
        let mut state: libc::timex = unsafe { std::mem::zeroed() };
        assert_eq!(state.modes, 0);
        // SAFETY: each syscall receives its correct arguments and a valid
        // timex output. No clock-setting or adjustment mode is ever passed.
        let result = unsafe {
            if nr == libc::SYS_clock_adjtime {
                libc::syscall(nr, libc::CLOCK_REALTIME, &mut state as *mut libc::timex)
            } else {
                libc::syscall(nr, &mut state as *mut libc::timex)
            }
        };
        assert!(result >= 0, "{name}, filtered={filtered}: {}", io::Error::last_os_error());
        assert_eq!(state.modes, 0);
        println!("clock-read syscall={name} modes=0 capabilities=0 filtered={filtered} result={result}");
    }

    // Establish that the runner's outer policy permits both read syscalls.
    for (name, nr) in [("adjtimex", libc::SYS_adjtimex), ("clock_adjtime", libc::SYS_clock_adjtime)] {
        read_clock(name, nr, false);
    }
    seccomp::install(Some(0)).expect("install the production default filter");
    // SAFETY: PR_GET_SECCOMP reads this process's seccomp mode.
    assert_eq!(unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong) }, 2);
    for (name, nr) in [("adjtimex", libc::SYS_adjtimex), ("clock_adjtime", libc::SYS_clock_adjtime)] {
        read_clock(name, nr, true);
    }
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
        .expect("start the isolated clock-read probe");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "the clock-state probe failed: {}", output.status);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("clock-read: real seccomp validation runs on Linux");
}
