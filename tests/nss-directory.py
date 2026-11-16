#!/usr/bin/env python3
"""Verify real GNU NSS lookup, host-access guards and connection revocation."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import struct
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
MODE = Path("/tmp/asagiri-nss-fixture")
EXPECTED = [
    "provider-name-and-uid", "missing-and-invalid-account",
    "named-and-primary-group-membership", "actual-peer-credentials-and-acl-admission",
    "temporary", "unavailable", "missing-backend", "provider-recovery",
    "live-revocation-and-acl-removal", "account-and-group-deletion",
]


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--unit-test-binary", type=Path, required=True)
    parser.add_argument("--static-control", type=Path, required=True)
    parser.add_argument("--provider", type=Path,
                        help="Use a separately cross-compiled fixture provider for local Linux probes")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    assert platform.system() == "Linux" and os.geteuid() == 0
    assert Path("/.dockerenv").is_file() and os.environ.get("ASAGIRI_NSS_PRIVATE_BUILDER") == "1", \
        "Run only inside the disposable native compiler container"
    output = args.out.resolve()
    output.mkdir(mode=0o755)
    for path in (args.binary, args.unit_test_binary, args.static_control):
        assert path.is_file() and not path.is_symlink()
    assert not MODE.exists()
    switch = Path("/etc/nsswitch.conf")
    original = switch.read_bytes()
    identities = [Path("/etc/passwd"), Path("/etc/group"), Path("/etc/shadow")]
    before = {str(path): sha(path) for path in identities}
    assert b"asagiri:" not in Path("/etc/group").read_bytes()
    assert all(b"directory-fixture" not in path.read_bytes() for path in identities)
    source = (args.binary.parent / "SOURCE_COMMIT").read_text().strip()
    assert re.fullmatch(r"[0-9a-f]{40}", source)
    assert (args.static_control.parent / "SOURCE_COMMIT").read_text().strip() == source
    record = {
        "schema": "asagiri.native-nss.v1", "status": "running",
        "source": source,
        "run_id": os.environ.get("GITHUB_RUN_ID"), "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        "architecture": platform.machine(), "cases": [], "commands": [],
        "binaries_sha256": {name: sha(path) for name, path in [
            ("native", args.binary), ("portable_control", args.static_control),
            ("unit_tests", args.unit_test_binary)]},
        "source_sha256": {name: sha(ROOT / name) for name in [
            "crates/asagiri/src/daemon/access.rs", "crates/asagiri/src/daemon/access/nss_tests.rs",
            "crates/asagiri/src/hostaccess.rs", "tests/fixtures/nss/provider.c", "tests/nss-directory.py"]},
        "account_files_before": before,
    }

    def save():
        (output / "verification.json").write_text(json.dumps(record, indent=2) + "\n")

    def command(label, argv, **kwargs):
        result = subprocess.run([str(value) for value in argv], capture_output=True, text=True,
                                timeout=180, **kwargs)
        (output / (label + ".log")).write_text(result.stdout + result.stderr)
        record["commands"].append({"label": label, "arguments": [str(value) for value in argv],
                                   "returncode": result.returncode, "stdout": result.stdout,
                                   "stderr": result.stderr})
        save()
        return result

    triplet = (platform.machine() + "-linux-gnu" if args.provider else
               subprocess.check_output(["cc", "-dumpmachine"], text=True).strip())
    assert triplet in ("x86_64-linux-gnu", "aarch64-linux-gnu")
    library = Path("/lib") / triplet / "libnss_asagirifixture.so.2"
    assert library.parent.is_dir() and not library.exists()
    installed = False
    save()
    try:
        provider = output / library.name
        if args.provider:
            assert args.provider.is_file() and not args.provider.is_symlink()
            header = args.provider.read_bytes()[:64]
            machine = {"x86_64": 62, "aarch64": 183}[platform.machine()]
            assert len(header) == 64 and header[:6] == b"\x7fELF\x02\x01"
            assert struct.unpack_from("<HH", header, 16) == (3, machine)
            shutil.copyfile(args.provider, provider)
            record["provider_build"] = "external-cross-compiled-fixture"
        else:
            compiled = command("provider-compile", [
                "cc", "-std=c11", "-fPIC", "-shared", "-O2", "-Wall", "-Wextra", "-Werror",
                "-Wl,-soname,libnss_asagirifixture.so.2",
                ROOT / "tests/fixtures/nss/provider.c", "-o", provider,
            ])
            assert compiled.returncode == 0, compiled.stderr
            record["provider_build"] = "compiled-in-private-builder"
        shutil.copyfile(provider, library)
        installed = True
        library.chmod(0o755)
        record["provider_sha256"] = sha(provider)
        MODE.mkdir(mode=0o700)
        (MODE / "mode").write_text("member")
        text = original.decode()
        for database in ("passwd", "group"):
            text, count = re.subn(r"^" + database + r":.*$", database + ": files asagirifixture [TRYAGAIN=return UNAVAIL=return]",
                                 text, flags=re.M)
            assert count == 1
        switch.write_text(text)
        record["fixture_nsswitch"] = text
        getent = command("provider-getent", ["getent", "passwd", "directory-fixture"])
        assert getent.returncode == 0 and getent.stdout.split(":")[:4] == [
            "directory-fixture", "x", "700001", "700002"]
        record["cases"].append({"name": "real-provider-serves-account", "status": "passed"})

        environment = os.environ | {"ASAGIRI_REQUIRE_NATIVE_NSS_TEST": "1"}
        unit = command("production-access-tests", [
            args.unit_test_binary, "daemon::access::", "--nocapture", "--test-threads=1",
        ], env=environment)
        assert unit.returncode == 0
        markers = re.findall(r"NSS_CASE passed ([a-z-]+)", unit.stdout)
        assert markers == EXPECTED, markers
        record["cases"].extend({"name": name, "status": "passed"} for name in markers)
        assert (MODE / "mode").read_text() == "member"
        shutil.copyfile(args.unit_test_binary, output / "nss-unit-tests")

        with tempfile.TemporaryDirectory(prefix="asagiri-nss-commands-") as directory:
            guard = Path(directory)
            log = guard / "guard.log"
            for name in ("groupadd", "usermod", "gpasswd", "systemctl"):
                helper = guard / name
                helper.write_text('#!/bin/sh\nprintf "%s\\n" "$0 $*" >> "$ASAGIRI_NSS_GUARD_LOG"\nexit 121\n')
                helper.chmod(0o755)
            env = os.environ | {"PATH": str(guard) + os.pathsep + os.environ["PATH"],
                                 "ASAGIRI_NSS_GUARD_LOG": str(log)}

            def lookup(label, binary, state, account, message, expected_guard):
                if state == "missing-backend":
                    (MODE / "mode").unlink()
                else:
                    (MODE / "mode").write_text(state)
                log.write_text("")
                result = command(label, [binary, "host-access", account], env=env)
                assert result.returncode != 0 and message in result.stdout + result.stderr
                lines = log.read_text().splitlines()
                if expected_guard:
                    assert len(lines) == 1 and lines[0] == str(guard / "groupadd") + " --system asagiri"
                    assert "no account named" not in result.stdout + result.stderr
                else:
                    assert lines == []
                record["cases"].append({"name": label, "status": "passed",
                                         "guard_calls": len(lines), "mode": state})

            lookup("portable-control-does-not-load-host-NSS", args.static_control,
                   "member", "directory-fixture", "no account named directory-fixture", False)
            lookup("native-CLI-resolves-directory-account", args.binary,
                   "member", "directory-fixture", "groupadd", True)
            lookup("native-CLI-rejects-absent-account", args.binary,
                   "member", "asagiri-nss-no-such-account", "no account named", False)
            for state in ("temporary", "unavailable", "missing-backend"):
                lookup("native-CLI-preserves-" + state + "-account-error", args.binary,
                       state, "directory-fixture", "couldn't look up account", False)
            lookup("native-CLI-preserves-group-outage", args.binary,
                   "unavailable", "root", "couldn't look up group asagiri", False)
            lookup("native-CLI-preserves-missing-backend-group-error", args.binary,
                   "missing-backend", "root", "couldn't look up group asagiri", False)
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error))
        raise
    finally:
        switch.write_bytes(original)
        if installed:
            library.unlink()
        if MODE.exists():
            shutil.rmtree(MODE)
        after = {str(path): sha(path) for path in identities}
        record.update(
            finished_at=datetime.now(timezone.utc).isoformat(), account_files_after=after,
            account_files_unchanged=before == after, nsswitch_restored=switch.read_bytes() == original,
            provider_removed=not library.exists(), control_directory_removed=not MODE.exists(),
        )
        save()
        assert before == after and record["nsswitch_restored"] and record["provider_removed"]
    print(json.dumps({"status": record["status"], "cases": len(record["cases"]),
                      "provider_sha256": record["provider_sha256"]}))


if __name__ == "__main__":
    main()
