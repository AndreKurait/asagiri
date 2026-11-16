//! Actual GNU NSS-module coverage. The CI builder installs the provider only
//! in its disposable container; ordinary workspace tests do not alter NSS.

use super::*;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

const USER: u32 = 700001;
const PRIMARY: u32 = 700003;
const GROUP: &str = "asagiri-nss-fixture";
const MODE: &str = "/tmp/asagiri-nss-fixture/mode";

fn required() -> bool {
    std::env::var_os("ASAGIRI_REQUIRE_NATIVE_NSS_TEST").as_deref() == Some(OsStr::new("1"))
}

fn state(value: &str) {
    std::fs::write(MODE, value).unwrap();
}

fn passed(name: &str) {
    println!("NSS_CASE passed {name}");
}

struct Fixture {
    directory: PathBuf,
    child: Option<Child>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::write(MODE, "member");
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn directory_peer_client() {
    let Some(socket) = std::env::var_os("ASAGIRI_NSS_PEER_SOCKET") else { return };
    assert!(required());
    // SAFETY: getuid has no preconditions.
    assert_eq!(unsafe { libc::getuid() }, USER);
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    assert_eq!(stream.read(&mut [0u8; 1]).unwrap(), 0, "revocation closes the actual connection");
}

#[test]
fn directory_provider_controls_admission_and_revocation() {
    if !required() {
        println!("NSS fixture requires the disposable GNU CI builder");
        return;
    }
    // The fixture must already exist before this test changes its state.
    assert_eq!(std::fs::read_to_string(MODE).unwrap(), "member");
    // SAFETY: geteuid has no preconditions.
    assert_eq!(unsafe { libc::geteuid() }, 0);
    for file in ["/etc/passwd", "/etc/group"] {
        let contents = std::fs::read_to_string(file).unwrap();
        assert!(!contents.contains("directory-fixture"));
        assert!(!contents.contains("asagiri-nss-fixture"));
    }
    assert_eq!(uid("directory-fixture").unwrap(), Some(USER));
    let account = account_by_uid(USER).unwrap().unwrap();
    // SAFETY: the returned NSS record owns the terminated name.
    assert_eq!(unsafe { CStr::from_ptr(account.rec.pw_name) }, c"directory-fixture");
    passed("provider-name-and-uid");
    assert_eq!(uid("asagiri-nss-no-such-account").unwrap(), None);
    assert!(account_by_uid(700099).unwrap().is_none());
    assert_eq!(uid("bad\0name").unwrap_err().kind(), io::ErrorKind::InvalidInput);
    passed("missing-and-invalid-account");
    assert_eq!(members(GROUP).unwrap(), ["directory-fixture"]);
    assert_eq!(member_uids(&c_name(GROUP).unwrap()).unwrap(), [USER]);
    assert!(member(PRIMARY, &c_name(GROUP).unwrap()).unwrap());
    assert!(!member(700099, &c_name(GROUP).unwrap()).unwrap());
    passed("named-and-primary-group-membership");

    let directory = std::env::temp_dir().join(format!("asagiri-nss-peer-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let mut fixture = Fixture { directory, child: None };
    std::fs::set_permissions(&fixture.directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    let socket = fixture.directory.join("engine.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660)).unwrap();
    let gate = Gate::new(&socket, OsStr::new(GROUP)).unwrap();
    assert_eq!(gate.grant().unwrap(), 1);
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "daemon::access::nss_tests::directory_peer_client", "--nocapture", "--test-threads=1"])
        .env("ASAGIRI_NSS_PEER_SOCKET", &socket).stdout(Stdio::null()).stderr(Stdio::inherit());
    // SAFETY: only async-signal-safe credential syscalls run before exec.
    // The child loses all supplemental groups, so the named ACL admits it.
    unsafe {
        command.pre_exec(|| {
            if libc::setgroups(0, std::ptr::null()) != 0 || libc::setgid(65534) != 0 || libc::setuid(USER) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    fixture.child = Some(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    let connection = loop {
        match listener.accept() {
            Ok((connection, _)) => break connection,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                assert!(fixture.child.as_mut().unwrap().try_wait().unwrap().is_none(), "peer exited before connecting");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("directory peer did not connect: {e}"),
        }
    };
    assert_eq!(peer_uid(&connection).unwrap(), USER);
    let _seat = gate.admit(&connection).expect("actual directory member is admitted");
    let mut listed = None;
    gate.tick(&mut listed);
    assert_eq!(gate.open.lock().unwrap().len(), 1);
    passed("actual-peer-credentials-and-acl-admission");

    for (value, code) in [("temporary", libc::EAGAIN), ("unavailable", libc::EIO)] {
        state(value);
        assert_eq!(uid("directory-fixture").unwrap_err().raw_os_error(), Some(code));
        // PRIMARY has no cached Gate answer: a fresh admission needs a lookup.
        assert!(gate.allowed(PRIMARY).is_err());
        gate.tick(&mut listed);
        assert_eq!(gate.open.lock().unwrap().len(), 1, "an outage is not membership removal");
        assert!(fixture.child.as_mut().unwrap().try_wait().unwrap().is_none());
        passed(value);
    }
    state("member");
    gate.tick(&mut listed);
    assert!(gate.allowed(PRIMARY).unwrap());
    assert_eq!(gate.open.lock().unwrap().len(), 1);
    passed("provider-recovery");

    state("revoked");
    gate.tick(&mut listed);
    assert!(gate.open.lock().unwrap().is_empty());
    assert_eq!(listed, Some(Vec::new()));
    assert_eq!(gate.grant().unwrap(), 0);
    assert!(!gate.allowed(USER).unwrap(), "revocation replaced the previous cached admission");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = fixture.child.as_mut().unwrap().try_wait().unwrap() {
            assert!(status.success(), "the client must observe EOF");
            fixture.child.take();
            break;
        }
        assert!(Instant::now() < deadline, "revoked connection did not close");
        std::thread::sleep(Duration::from_millis(10));
    }
    passed("live-revocation-and-acl-removal");
    state("missing");
    assert_eq!(uid("directory-fixture").unwrap(), None);
    assert!(!member(USER, &c_name(GROUP).unwrap()).unwrap());
    state("group-missing");
    assert!(current(&c_name(GROUP).unwrap()).unwrap().is_none());
    assert!(!member(USER, &c_name(GROUP).unwrap()).unwrap());
    passed("account-and-group-deletion");
}
