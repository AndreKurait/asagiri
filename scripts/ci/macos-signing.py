#!/usr/bin/env python3
"""Prepare an ephemeral signing keychain and notarize final release artifacts."""
import argparse
import base64
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import subprocess
import sys

SECRET_NAMES = (
    "ASAGIRI_APPLE_CERTIFICATE_BASE64",
    "ASAGIRI_APPLE_CERTIFICATE_PASSWORD",
    "ASAGIRI_APPLE_API_KEY_BASE64",
    "ASAGIRI_APPLE_API_KEY_ID",
    "ASAGIRI_APPLE_API_ISSUER_ID",
)
ROOT = Path(__file__).resolve().parents[2]


def run(*args):
    # Never echo commands: certificate passwords can occur in their arguments.
    result = subprocess.run(args, text=True, capture_output=True, check=False)
    if result.returncode:
        raise RuntimeError(f"{Path(args[0]).name} failed (exit {result.returncode})")
    return result.stdout


def state_dir():
    if sys.platform != "darwin" or os.environ.get("GITHUB_ACTIONS") != "true":
        raise RuntimeError("Signing keychains may only be managed on macOS Actions runners")
    return Path(os.environ["RUNNER_TEMP"]) / "asagiri-signing"


def export(**values):
    with open(os.environ["GITHUB_ENV"], "a") as stream:
        for key, value in values.items():
            if "\n" in value or "\r" in value:
                raise ValueError("Invalid environment value")
            stream.write(f"{key}={value}\n")


def configuration():
    values = {name: os.environ.get(name, "") for name in SECRET_NAMES}
    if not any(values.values()):
        if os.environ.get("ASAGIRI_REQUIRE_APPLE_SIGNING") == "true":
            raise ValueError("Apple signing is required, but its credentials are absent")
        return None
    missing = [name for name, value in values.items() if not value]
    if missing:
        raise ValueError("Incomplete Apple signing configuration: " + ", ".join(missing))
    if not re.fullmatch(r"[A-Za-z0-9]{10,64}", values["ASAGIRI_APPLE_API_KEY_ID"]):
        raise ValueError("Invalid App Store Connect API key ID")
    if not re.fullmatch(r"[a-fA-F0-9]{8}(?:-[a-fA-F0-9]{4}){3}-[a-fA-F0-9]{12}",
                        values["ASAGIRI_APPLE_API_ISSUER_ID"]):
        raise ValueError("Invalid App Store Connect issuer ID")
    return values


def prepare():
    directory = state_dir()
    values = configuration()
    if values is None:
        export(ASAGIRI_SIGN_IDENTITY="-", ASAGIRI_NOTARY_PROFILE="", ASAGIRI_APPLE_SIGNING="false")
        print("Apple credentials are absent: explicitly building ad hoc, unnotarized artifacts.")
        return
    directory.mkdir(mode=0o700, parents=False, exist_ok=False)
    keychain = directory / "release.keychain-db"
    original = shlex.split(run("security", "list-keychains", "-d", "user"))
    (directory / "state.json").write_text(json.dumps({"keychain": str(keychain), "original": original}))
    certificate = directory / "certificate.p12"
    api_key = directory / "AuthKey.p8"
    for path, name in ((certificate, SECRET_NAMES[0]), (api_key, SECRET_NAMES[2])):
        path.write_bytes(base64.b64decode(values[name], validate=True))
        path.chmod(0o600)
    password = secrets.token_urlsafe(32)
    run("security", "create-keychain", "-p", password, str(keychain))
    run("security", "set-keychain-settings", "-lut", "21600", str(keychain))
    run("security", "unlock-keychain", "-p", password, str(keychain))
    run("security", "import", str(certificate), "-P", values[SECRET_NAMES[1]],
        "-t", "cert", "-f", "pkcs12", "-k", str(keychain),
        "-T", "/usr/bin/codesign", "-T", "/usr/bin/security")
    run("security", "set-key-partition-list", "-S", "apple-tool:,apple:,codesign:",
        "-k", password, str(keychain))
    run("security", "list-keychains", "-d", "user", "-s", str(keychain), *original)
    identities = re.findall(r'([A-Fa-f0-9]{40}) "Developer ID Application:[^"]+"',
                            run("security", "find-identity", "-v", "-p", "codesigning", str(keychain)))
    if len(identities) != 1:
        raise RuntimeError("Expected exactly one valid Developer ID Application identity")
    run("xcrun", "notarytool", "store-credentials", "asagiri-release",
        "--keychain", str(keychain), "--key", str(api_key),
        "--key-id", values[SECRET_NAMES[3]], "--issuer", values[SECRET_NAMES[4]])
    certificate.unlink()
    api_key.unlink()
    # Notarization happens after packing, not inside xtask's app build.
    export(ASAGIRI_SIGN_IDENTITY=identities[0], ASAGIRI_NOTARY_PROFILE="", ASAGIRI_APPLE_SIGNING="true")
    print("Developer ID and notarization credentials prepared in a temporary keychain.")


def submit(path, keychain, label):
    args = ("--keychain-profile", "asagiri-release", "--keychain", str(keychain))
    result = json.loads(run("xcrun", "notarytool", "submit", str(path), *args,
                            "--wait", "--timeout", "20m", "--output-format", "json"))
    directory = state_dir()
    (directory / f"{label}-submission.json").write_text(json.dumps(result, indent=2) + "\n")
    if result.get("status") != "Accepted":
        identifier = result.get("id", "")
        if re.fullmatch(r"[a-fA-F0-9-]{36}", identifier):
            log = run("xcrun", "notarytool", "log", identifier, *args)
            (directory / f"{label}-failure.json").write_text(log)
        raise RuntimeError(f"Apple rejected the {label} submission; see notarization diagnostics")


def finalize(app, arch, version):
    if os.environ.get("ASAGIRI_APPLE_SIGNING") != "true":
        print("Artifacts are ad hoc signed, not Apple notarized.")
        return
    directory = state_dir()
    state = json.loads((directory / "state.json").read_text())
    keychain = Path(state["keychain"])
    archive = directory / "notary-app.zip"
    run("ditto", "-c", "-k", "--keepParent", str(app), str(archive))
    submit(archive, keychain, "app")
    run("xcrun", "stapler", "staple", str(app))
    run("xcrun", "stapler", "validate", str(app))
    # Build the final DMG from the stapled app, then notarize that exact DMG.
    # Recreating it after stapling would discard its ticket.
    run("cargo", "xtask", "dmg", "--arch", arch)
    dmg = ROOT / "target" / f"Asagiri-{version}-{arch}.dmg"
    submit(dmg, keychain, "dmg")
    run("xcrun", "stapler", "staple", str(dmg))
    run("xcrun", "stapler", "validate", str(dmg))
    run("codesign", "--verify", "--deep", "--strict", str(app))
    run("codesign", "--verify", "--strict", str(dmg))
    run("spctl", "--assess", "--type", "execute", "--verbose=2", str(app))
    run("spctl", "--assess", "--type", "open", "--context", "context:primary-signature",
        "--verbose=2", str(dmg))
    # Tar and the update feed must describe the app after its ticket was stapled.
    run("cargo", "xtask", "pack", "--arch", arch)
    archive.unlink()
    print("Final app and DMG are signed, notarized, stapled, and accepted by Gatekeeper.")


def cleanup():
    directory = state_dir()
    state = directory / "state.json"
    try:
        if state.exists():
            saved = json.loads(state.read_text())
            run("security", "list-keychains", "-d", "user", "-s", *saved["original"])
            keychain = Path(saved["keychain"])
            if keychain.exists():
                run("security", "delete-keychain", str(keychain))
    finally:
        if directory.exists():
            shutil.rmtree(directory)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("prepare", "finalize", "cleanup"))
    parser.add_argument("--app", type=Path)
    parser.add_argument("--arch", choices=("arm64", "x86_64"))
    parser.add_argument("--version")
    args = parser.parse_args()
    if args.command == "prepare":
        prepare()
    elif args.command == "cleanup":
        cleanup()
    else:
        if not args.app or not args.arch or not args.version or not re.fullmatch(r"\d+\.\d+\.\d+(?:-dev\.\d+)?", args.version):
            parser.error("finalize requires --app, --arch, and a release --version")
        finalize(args.app, args.arch, args.version)


if __name__ == "__main__":
    main()
