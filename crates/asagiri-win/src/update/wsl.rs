//! Explicit engine activation inside the selected WSL distro.
//! Source paths arrive as shell arguments, never interpolated shell code.

pub(super) const OWNED_DISTRO: &str = "set -eu\n\
    cp -f -- \"$1\" /usr/local/bin/asagiri.new\n\
    cp -f -- \"$2\" /usr/local/bin/asagiri-init.new\n\
    chmod 755 /usr/local/bin/asagiri.new /usr/local/bin/asagiri-init.new\n\
    mv -f /usr/local/bin/asagiri.new /usr/local/bin/asagiri\n\
    mv -f /usr/local/bin/asagiri-init.new /usr/local/bin/asagiri-init\n\
    pkill -f '^/usr/local/bin/[a]sagiri daemon($| )' || [ \"$?\" -eq 1 ]\n\
    pkill -f '^/usr/local/bin/[a]sagiri bridge($| )' || [ \"$?\" -eq 1 ]\n\
    sleep 1\n\
    exec /usr/local/bin/asagiri-start";

pub(super) const PACKAGE_DISTRO: &str =
    "set -eu\nsh \"$3\"\npkill -f '^/usr/local/bin/[a]sagiri bridge($| )' || [ \"$?\" -eq 1 ]";

// These execute the actual shell programs with only a private directory as
// their destination. pkill is a fixture that evaluates the real expressions
// against sample command lines; no process or WSL instance is signalled.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture { root: PathBuf }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = PathBuf::from("/tmp").join(format!("asagiri-wsl-script-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            let fixture = Self { root };
            std::fs::create_dir(fixture.root.join("engine")).unwrap();
            std::fs::create_dir(fixture.root.join("tools")).unwrap();
            fixture.program("engine/asagiri-start", "#!/bin/sh\nprintf 'start\\n' >> \"$ASAGIRI_TEST_EVENTS\"\n");
            fixture.program("tools/pkill", "#!/bin/sh\n\
                [ \"$1\" = -f ] || exit 2\n\
                [ \"${ASAGIRI_TEST_KILL_FAILURE:-0}\" = 0 ] || exit 2\n\
                matched=$(/usr/bin/grep -E -- \"$2\" \"$ASAGIRI_TEST_PROCESSES\") || exit \"$?\"\n\
                printf '%s\\n' \"$matched\" >> \"$ASAGIRI_TEST_EVENTS\"\n");
            fixture.program("tools/sleep", "#!/bin/sh\nexit 0\n");
            std::fs::write(fixture.root.join("engine/asagiri"), b"old engine").unwrap();
            std::fs::write(fixture.root.join("engine/asagiri-init"), b"old init").unwrap();
            // These must remain literal path characters when passed to sh.
            std::fs::write(fixture.root.join("app's new $engine"), b"new engine").unwrap();
            std::fs::write(fixture.root.join("init's new; file"), b"new init").unwrap();
            std::fs::write(fixture.root.join("processes"), format!(
                "{0}/engine/asagiri daemon\n{0}/engine/asagiri bridge --stdio\n\
                 sh -c pkill -f '^{0}/engine/[a]sagiri daemon($| )'\n\
                 {0}/engine/asagiri daemon-other\n{0}/engine/other daemon\n", fixture.root.display())).unwrap();
            fixture
        }

        fn program(&self, path: &str, content: &str) {
            let file = self.root.join(path);
            std::fs::write(&file, content).unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        fn run(&self, source: &str, init: &Path, installer: &Path, kill_failure: bool) -> Output {
            // Fixed production destinations become this test's private files.
            let script = source.replace("/usr/local/bin/", &format!("{}/engine/", self.root.display()));
            Command::new("/bin/sh").args(["-c", &script, "asagiri-update"])
                .arg(self.root.join("app's new $engine")).arg(init).arg(installer)
                .env("PATH", format!("{}/tools:/usr/bin:/bin", self.root.display()))
                .env("ASAGIRI_TEST_EVENTS", self.root.join("events"))
                .env("ASAGIRI_TEST_PROCESSES", self.root.join("processes"))
                .env("ASAGIRI_TEST_KILL_FAILURE", if kill_failure { "1" } else { "0" })
                .output().unwrap()
        }

        fn events(&self) -> String { std::fs::read_to_string(self.root.join("events")).unwrap_or_default() }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.root); }
    }

    #[test]
    fn replacement_handles_quoted_paths_and_matches_only_engine_processes() {
        let fixture = Fixture::new();
        let output = fixture.run(OWNED_DISTRO, &fixture.root.join("init's new; file"), Path::new(""), false);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(std::fs::read(fixture.root.join("engine/asagiri")).unwrap(), b"new engine");
        assert_eq!(std::fs::read(fixture.root.join("engine/asagiri-init")).unwrap(), b"new init");
        assert_eq!(fixture.events(), format!("{0}/engine/asagiri daemon\n{0}/engine/asagiri bridge --stdio\nstart\n", fixture.root.display()));
    }

    #[test]
    fn copy_failure_keeps_the_old_engine_and_does_not_stop_or_start_anything() {
        let fixture = Fixture::new();
        assert!(!fixture.run(OWNED_DISTRO, &fixture.root.join("missing init"), Path::new(""), false).status.success());
        assert_eq!(std::fs::read(fixture.root.join("engine/asagiri")).unwrap(), b"old engine");
        assert_eq!(std::fs::read(fixture.root.join("engine/asagiri-init")).unwrap(), b"old init");
        assert!(fixture.events().is_empty());
    }

    #[test]
    fn a_package_installer_failure_is_not_reported_as_a_restart() {
        let fixture = Fixture::new();
        fixture.program("broken installer.sh", "#!/bin/sh\nexit 42\n");
        let output = fixture.run(PACKAGE_DISTRO, Path::new(""), &fixture.root.join("broken installer.sh"), false);
        assert_eq!(output.status.code(), Some(42));
        assert!(fixture.events().is_empty());
    }

    #[test]
    fn no_matching_process_is_normal_but_a_signal_error_stops_activation() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("processes"), b"").unwrap();
        assert!(fixture.run(OWNED_DISTRO, &fixture.root.join("init's new; file"), Path::new(""), false).status.success());
        assert_eq!(fixture.events(), "start\n");
        std::fs::remove_file(fixture.root.join("events")).unwrap();
        assert!(!fixture.run(OWNED_DISTRO, &fixture.root.join("init's new; file"), Path::new(""), true).status.success());
        assert!(fixture.events().is_empty());
    }
}
