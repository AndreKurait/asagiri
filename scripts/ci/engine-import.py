#!/usr/bin/env python3
"""Exercise the desktop migration module between two disposable Linux engines."""
import argparse
import gzip
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import tarfile
import tempfile
import time
import urllib.parse
import uuid

ROOT = Path(__file__).resolve().parents[2]
OWNER = "dev.asagiri.import-test"


def archive_identity(data):
    """Hash the saved configuration and actual uncompressed layer contents."""
    maximum = 256 * 1024 * 1024
    assert len(data) <= maximum, "fixture image archive exceeds the inspection budget"

    def name(value):
        assert isinstance(value, str) and value and not value.startswith("/"), value
        parts = value.split("/")
        assert ".." not in parts and "\0" not in value, value
        return "/".join(part for part in parts if part and part != ".")

    with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as archive:
        files = {}
        for member in archive:
            path = name(member.name)
            if member.isdir():
                continue
            assert member.isfile(), "fixture archive has a non-regular member: " + path
            assert path and path not in files, "duplicate fixture archive member: " + path
            assert 0 <= member.size <= maximum, "fixture archive member exceeds the inspection budget"
            files[path] = member
            assert len(files) <= 4096, "fixture archive contains too many files"

        def read(path, limit):
            path = name(path)
            assert path in files, "missing fixture archive member: " + path
            assert files[path].size <= limit, "fixture archive metadata exceeds the inspection budget"
            stream = archive.extractfile(files[path])
            assert stream is not None
            with stream:
                return stream.read()

        manifest = json.loads(read("manifest.json", 4 * 1024 * 1024))
        assert isinstance(manifest, list) and len(manifest) == 1, "save exactly one fixture image"
        entry = manifest[0]
        raw_config = read(entry["Config"], 4 * 1024 * 1024)
        config = json.loads(raw_config)
        diff_ids = config["rootfs"]["diff_ids"]
        assert config["rootfs"]["type"] == "layers"
        assert isinstance(diff_ids, list) and len(diff_ids) == len(entry["Layers"])
        layers = []
        total = 0
        for path, expected in zip(entry["Layers"], diff_ids):
            raw = read(path, maximum)
            stream = gzip.GzipFile(fileobj=io.BytesIO(raw)) if raw.startswith(b"\x1f\x8b") else io.BytesIO(raw)
            digest = hashlib.sha256()
            length = 0
            with stream:
                while chunk := stream.read(1024 * 1024):
                    total += len(chunk)
                    length += len(chunk)
                    assert total <= maximum, "fixture layers exceed the expanded inspection budget"
                    digest.update(chunk)
            actual = "sha256:" + digest.hexdigest()
            assert actual == expected, f"fixture layer {path}: contents {actual} differ from {expected}"
            layers.append({"path": name(path), "blob_sha256": hashlib.sha256(raw).hexdigest(),
                           "blob_bytes": len(raw), "diff_id": actual, "uncompressed_bytes": length})
    return {
        "archive_sha256": hashlib.sha256(data).hexdigest(),
        "config_sha256": hashlib.sha256(raw_config).hexdigest(),
        "platform": {key: config.get(key, "") for key in ("os", "architecture", "variant")},
        "diff_ids": diff_ids, "layers": layers,
    }


def same_image_content(expected, observed):
    for key in ("config_sha256", "platform", "diff_ids"):
        assert expected[key] == observed[key], f"imported image changed {key}: {expected[key]!r} != {observed[key]!r}"
    assert [layer["uncompressed_bytes"] for layer in expected["layers"]] == [
        layer["uncompressed_bytes"] for layer in observed["layers"]
    ], "imported image changed the unpacked layer lengths"


def command(*args, check=True, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=check, capture_output=True,
                          timeout=kwargs.pop("timeout", 120), **kwargs)


def checksum(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = str(path)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    assert platform.system() == "Linux", "Use disposable Linux CI engines"
    # Both daemon sockets enforce Linux peer credentials. The test client and
    # its private runtime directories must have the daemon owner's identity.
    assert os.geteuid() == 0, "Run this private-engine fixture as root"
    bins, fixtures, output = args.binaries.resolve(), args.fixtures.resolve(), args.out.resolve()
    output.mkdir(mode=0o755)
    # This folder contains only generated CI evidence. Socket directories and
    # context files remain private; Actions uploads these results as its user.
    output.chmod(0o755)
    hashes = dict(line.split()[::-1] for line in (bins / "SHA256SUMS").read_text().splitlines())
    assert set(hashes) == {"asagiri", "asagiri-init"}
    assert all(checksum(bins / name) == digest for name, digest in hashes.items())
    head = command("git", "-c", "safe.directory=" + str(ROOT), "-C", ROOT,
                   "rev-parse", "HEAD", text=True).stdout.strip()
    assert (bins / "SOURCE_COMMIT").read_text().strip() == head
    assert (fixtures / "SOURCE_COMMIT").read_text().strip() == head
    fixture_hashes = dict(line.split()[::-1] for line in (fixtures / "SHA256SUMS").read_text().splitlines())
    assert set(fixture_hashes) == {"asagiri-daemon-fixture", "asagiri-import-fixture"}
    assert all(checksum(fixtures / name) == digest for name, digest in fixture_hashes.items())
    import_sources = dict(line.split()[::-1]
                          for line in (fixtures / "IMPORT_SOURCES_SHA256SUMS").read_text().splitlines())
    assert set(import_sources) == {
        "crates/asagiri/examples/import-fixture.rs", "crates/asagiri/src/import.rs",
        "crates/asagiri/src/client.rs", "crates/asagiri/src/client/health.rs",
        "crates/asagiri/src/client/health_response.rs", "crates/asagiri/src/flavor.rs",
        "crates/asagiri/src/diag/ops.rs",
    }
    assert all(checksum(ROOT / name) == digest for name, digest in import_sources.items())
    images = json.loads((ROOT / "scripts/ci/reference-images.json").read_text())
    nonce = uuid.uuid4().hex
    names = {key: "asagiri-import-" + nonce + "-" + key for key in ("source", "destination")}
    record = {
        "schema": "asagiri.engine-import.v3", "status": "running", "source": head,
        "client_uid": os.geteuid(),
        "run_id": os.environ.get("GITHUB_RUN_ID"), "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        "binaries": hashes, "fixture_sha256": checksum(Path(__file__)),
        "import_entrypoint": {
            "kind": "CI example including the production desktop importer, Docker CLI resolver and HTTP client",
            "binary_sha256": fixture_hashes["asagiri-import-fixture"],
            "sources_sha256": import_sources,
            "scope": "Real Docker CLI, HTTP client, operation headers and engines; desktop app dispatch and diagnostic logging are not exercised.",
        },
        "images_sha256": checksum(ROOT / "scripts/ci/reference-images.json"),
        "hosts": {}, "cases": [], "cleanup": [], "import_commands": [],
    }
    ids, sockets = {}, {"runner": Path("/var/run/docker.sock")}

    def save():
        temporary = output / "verification.tmp"
        temporary.write_text(json.dumps(record, indent=2) + "\n")
        temporary.chmod(0o644)
        temporary.replace(output / "verification.json")

    def request(engine, method, path):
        connection = Connection(sockets[engine])
        try:
            connection.request(method, "/v1.47" + path)
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    def get(engine, path):
        status, raw = request(engine, "GET", path)
        assert status == 200, (engine, path, status, raw)
        return json.loads(raw)

    def docker(engine, *args, **kwargs):
        path = "/sockets/docker.sock" if engine == "source" else "/run/asagiri/docker.sock"
        return command("docker", "exec", "-i", ids[engine], "docker", "-H", "unix://" + path,
                       *args, **kwargs)

    def imported(kind, name, success=True):
        result = command(
            "docker", "exec", "-e", "DOCKER_CONFIG=/tmp/import-config", ids["destination"],
            "/opt/asagiri-fixtures/asagiri-import-fixture", "desktop-linux", kind, name, check=False,
        )
        record["import_commands"].append({
            "arguments": ["desktop-linux", kind, name], "returncode": result.returncode,
            "stdout": result.stdout.decode(errors="replace"), "stderr": result.stderr.decode(errors="replace"),
        })
        save()
        text = (result.stdout + result.stderr).decode(errors="replace")
        assert (result.returncode == 0) == success, (kind, name, result.returncode, text)
        return text

    def case(name, **details):
        record["cases"].append({"name": name, "status": "passed", **details})
        save()

    save()
    try:
        assert get("runner", "/info")["ID"] == command(
            "docker", "info", "--format", "{{.ID}}", text=True).stdout.strip()
        with tempfile.TemporaryDirectory(prefix="asagiri-import-") as temporary:
            work = Path(temporary)
            for key in names:
                (work / key).mkdir(mode=0o700)
                assert (work / key).stat().st_uid == 0 and not (work / key).is_symlink()
                sockets[key] = work / key / "docker.sock"
            try:
                common = ["--privileged", "--cgroupns=private", "--tmpfs", "/run", "--tmpfs", "/tmp",
                          "--label", OWNER + "=" + nonce]
                commands = {
                    "source": [
                        *common, "-e", "DOCKER_TLS_CERTDIR=", "-v", str(work / "source") + ":/sockets",
                        "-v", "/var/lib/docker", images["docker"], "dockerd", "--host=unix:///sockets/docker.sock",
                    ],
                    "destination": [
                        *common, "-v", str(bins) + ":/opt/asagiri:ro",
                        "-v", str(fixtures) + ":/opt/asagiri-fixtures:ro",
                        "-v", str(work / "destination") + ":/run/asagiri",
                        "-v", str(work / "source") + ":/source-socket:ro",
                        "-v", "/var/lib/asagiri", "-v", "/test-store",
                        "-e", "ASAGIRI_SOCKET=/run/asagiri/docker.sock",
                        "-e", "ASAGIRI_DATA_DIR=/test-store", "-e", "ASAGIRI_TEST_ENGINE=1",
                        "--entrypoint", "/opt/asagiri/asagiri", images["asagiri_host"], "daemon",
                    ],
                }
                for engine, argv in commands.items():
                    command("docker", "run", "-d", "--name", names[engine], *argv, timeout=300)
                    info = json.loads(command("docker", "inspect", names[engine]).stdout)[0]
                    assert (info["Config"]["Labels"] or {}).get(OWNER) == nonce
                    assert not info["HostConfig"]["PortBindings"]
                    ids[engine] = info["Id"]
                    record["hosts"][engine] = {
                        "id": info["Id"], "image": info["Config"]["Image"],
                        "image_id": info["Image"], "published_ports": False,
                        "store_volumes": [m["Name"] for m in info["Mounts"] if m["Type"] == "volume"],
                        "socket_directory_uid": (work / engine).stat().st_uid,
                    }
                    deadline = time.monotonic() + 120
                    path = "/sockets/docker.sock" if engine == "source" else "/run/asagiri/docker.sock"
                    while time.monotonic() < deadline:
                        if sockets[engine].exists():
                            assert sockets[engine].is_socket() and sockets[engine].stat().st_uid == 0
                            command("docker", "exec", "--user", "0", ids[engine], "chmod", "600", path)
                            try:
                                version = get(engine, "/version")
                                break
                            except (OSError, AssertionError):
                                pass
                        state = json.loads(command("docker", "inspect", "--format", "{{json .State}}", ids[engine]).stdout)
                        if not state["Running"]:
                            raise RuntimeError(engine + " exited before becoming ready: " + json.dumps(state))
                        time.sleep(.25)
                    else:
                        raise RuntimeError(engine + " did not become ready")
                    assert bool(version.get("AsagiriVersion")) == (engine == "destination"), version
                    record["hosts"][engine]["version"] = version
                    assert get(engine, "/containers/json?all=1") == []
                    assert not get(engine, "/volumes").get("Volumes")
                    assert get(engine, "/images/json?all=1") == []
                    save()

                # Context files exist only inside this disposable client's /tmp.
                for context, path in [("desktop-linux", "/source-socket/docker.sock"),
                                      ("asagiri", "/run/asagiri/docker.sock")]:
                    command("docker", "exec", "-e", "DOCKER_CONFIG=/tmp/import-config",
                            ids["destination"], "docker", "context", "create", context,
                            "--docker", "host=unix://" + path)
                    version = command(
                        "docker", "exec", "-e", "DOCKER_CONFIG=/tmp/import-config",
                        ids["destination"], "docker", "--context", context, "version",
                        "--format", "{{json .Server}}", text=True)
                    engine = "source" if context == "desktop-linux" else "destination"
                    cli_version = json.loads(version.stdout)
                    assert cli_version["Version"] == record["hosts"][engine]["version"]["Version"]
                    record["hosts"][engine]["context_server_version"] = cli_version

                # Bare `import` is reserved for the desktop migration command.
                # Linux exposes Docker's rootfs command through `image import`.
                record["linux_import_dispatch"] = []
                def import_help(executable, *arguments):
                    result = command("docker", "exec", ids["destination"], executable,
                                     *arguments, "--help", text=True, check=False)
                    record["linux_import_dispatch"].append({
                        "arguments": [executable, *arguments, "--help"],
                        "returncode": result.returncode,
                        "stdout": result.stdout, "stderr": result.stderr,
                    })
                    save()
                    return result

                bare = import_help("/opt/asagiri/asagiri", "import")
                assert bare.returncode == 1 and bare.stderr.startswith("usage: asagiri COMMAND"), bare
                help_result = import_help("/opt/asagiri/asagiri", "image", "import")
                assert help_result.returncode == 0, help_result
                record["linux_rootfs_import_help"] = help_result.stdout
                assert "--change" in help_result.stdout and "--message" in help_result.stdout, help_result.stdout
                desktop = import_help("/opt/asagiri-fixtures/asagiri-import-fixture")
                assert desktop.returncode == 0 and "image|volume|container NAME" in desktop.stdout, desktop
                case("linux-image-import-and-desktop-fixture-dispatch")

                # Keep the disposable source free of recognized helper tags.
                seed = command("docker", "image", "inspect", "alpine:3.21").stdout
                seed_info = json.loads(seed)[0]
                record["seed"] = seed_info["Id"]
                record["image_inspections"] = {"seed": seed_info}
                archive = command("docker", "image", "save", "alpine:3.21").stdout
                seed_archive = output / "seed-image.tar"
                seed_archive.write_bytes(archive)
                seed_archive.chmod(0o644)
                record["image_archives"] = {"seed": archive_identity(archive)}
                docker("source", "load", input=archive)
                image = "asagiri-import-fixture:" + nonce
                docker("source", "tag", "alpine:3.21", image)
                record["seed_archive_sha256"] = hashlib.sha256(archive).hexdigest()
                del archive
                volume = "asagiri-import-volume-" + nonce
                missing = volume + "-missing"
                assert request("source", "GET", "/volumes/" + missing)[0] == 404
                error = imported("volume", missing, success=False)
                assert "could not check volume" in error, error
                assert request("destination", "GET", "/volumes/" + missing)[0] == 404
                case("missing-source-does-not-create-a-destination")

                docker("source", "volume", "create", "--label", OWNER + "=" + nonce, volume)
                writer = "asagiri-import-writer-" + nonce
                cid = docker("source", "run", "-d", "--name", writer, "--network", "none",
                             "--label", OWNER + "=" + nonce, "-v", volume + ":/data",
                             image, "sleep", "600", text=True).stdout.strip()
                for state in ("running", "paused"):
                    if state == "paused":
                        docker("source", "pause", cid)
                    assert get("source", "/containers/" + cid + "/json")["State"]["Status"] == state
                    error = imported("volume", volume, success=False)
                    assert writer in error and "stop" in error, error
                    assert request("destination", "GET", "/volumes/" + volume)[0] == 404
                    assert get("destination", "/containers/json?all=1") == []
                    assert get("destination", "/images/json?all=1") == []
                    case("reject-" + state + "-before-helper-or-destination")
                definition = "asagiri-import-definition-" + nonce
                source_id = docker("source", "create", "--name", definition, "--network", "none",
                                   "--label", OWNER + "=" + nonce, "--env", "IMPORT_MARKER=" + nonce,
                                   "--user", "1001:1002", "--workdir", "/tmp",
                                   "-v", volume + ":/data:ro", image, "sh", "-ec",
                                   "test \"$IMPORT_MARKER\" = " + nonce + "; cat /data/payload",
                                   text=True).stdout.strip()
                error = imported("container", definition, success=False)
                assert "import volume " + volume + " first" in error, error
                assert request("destination", "GET", "/volumes/" + volume)[0] == 404
                assert get("destination", "/containers/json?all=1") == []
                case("failed-volume-copy-does-not-create-empty-dependent-volume")
                docker("source", "unpause", cid)
                docker("source", "stop", "-t", "1", cid)
                docker("source", "rm", cid)
                assert request("source", "GET", "/containers/" + cid + "/json")[0] == 404
                cid = docker("source", "run", "-d", "--name", writer, "--network", "none",
                             "--restart", "always", "--label", OWNER + "=" + nonce,
                             "-v", volume + ":/data", image, "sh", "-c", "exit 7",
                             text=True).stdout.strip()
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    info = get("source", "/containers/" + cid + "/json")
                    if info["State"]["Status"] == "restarting" and info["RestartCount"] >= 5:
                        break
                    time.sleep(.1)
                else:
                    raise RuntimeError("The source did not enter a stable restart backoff")
                error = imported("volume", volume, success=False)
                assert writer in error and "stop" in error, error
                assert request("destination", "GET", "/volumes/" + volume)[0] == 404
                assert get("destination", "/images/json?all=1") == []
                case("reject-restarting-before-helper-or-destination")
                docker("source", "update", "--restart", "no", cid)
                docker("source", "stop", "-t", "1", cid)
                assert get("source", "/containers/" + cid + "/json")["State"]["Status"] == "exited"

                payload = bytes(range(256)) * 257
                digest = hashlib.sha256(payload).hexdigest()
                docker("source", "run", "--rm", "-i", "--network", "none", "-v", volume + ":/data",
                       "--label", OWNER + "=" + nonce, image, "sh", "-ec",
                       "cat > /data/payload; chmod 640 /data/payload; chown 1001:1002 /data/payload; "
                       "ln /data/payload /data/hard; ln -s payload /data/link", input=payload)
                inspect_files = (
                    "test \"$(stat -c %i /data/payload)\" = \"$(stat -c %i /data/hard)\"; "
                    "test \"$(readlink /data/link)\" = payload; "
                    "stat -c '%u:%g %a %s' /data/payload; sha256sum /data/payload /data/hard /data/link"
                )

                def contents(engine):
                    return docker(engine, "run", "--rm", "--network", "none",
                                  "--label", OWNER + "=" + nonce, "-v", volume + ":/data:ro",
                                  image, "sh", "-ec", inspect_files, text=True).stdout

                before = contents("source")
                assert before.splitlines()[0] == "1001:1002 640 " + str(len(payload)), before
                assert all(line.split()[0] == digest for line in before.splitlines()[1:]), before
                imported("image", image)
                escaped = urllib.parse.quote(image, safe="")
                original = get("source", "/images/" + escaped + "/json")
                copied = get("destination", "/images/" + escaped + "/json")
                record["image_inspections"].update(source=original, destination=copied)
                save()
                # Docker's containerd store reports its manifest/index digest as
                # Id; the classic store and Asagiri report the config digest.
                # Compare the actual saved config and unpacked layer bytes.
                for engine in ("source", "destination"):
                    archive = docker(engine, "image", "save", image).stdout
                    saved_archive = output / (engine + "-image.tar")
                    saved_archive.write_bytes(archive)
                    saved_archive.chmod(0o644)
                    record["image_archives"][engine] = archive_identity(archive)
                    save()
                    same_image_content(record["image_archives"]["seed"], record["image_archives"][engine])
                expected = record["image_archives"]["seed"]
                assert copied["Id"] == "sha256:" + expected["config_sha256"], copied
                for inspected in (seed_info, original, copied):
                    assert inspected["RootFS"]["Layers"] == expected["diff_ids"], inspected
                    assert inspected["Os"] == expected["platform"]["os"], inspected
                    assert inspected["Architecture"] == expected["platform"]["architecture"], inspected
                case("image-configuration-and-layer-content-preserved",
                     config_sha256=expected["config_sha256"],
                     api_image_ids={key: value["Id"] for key, value in record["image_inspections"].items()})
                del archive
                source_tags = {tag for entry in get("source", "/images/json?all=1")
                               for tag in (entry.get("RepoTags") or [])}
                helper_tags = {"alpine:latest", "busybox:latest", "alpine:3.22", "debian:trixie", "ubuntu:24.04"}
                assert source_tags.isdisjoint(helper_tags), source_tags
                imported("volume", volume)
                assert contents("destination") == before == contents("source")
                assert get("source", "/containers/" + cid + "/json")["State"]["Status"] == "exited"
                assert get("destination", "/containers/json?all=1") == []
                case("idle-volume-copy", source_helper_tags_absent=True, bytes=len(payload), sha256=digest,
                     ownership="1001:1002", mode="640", hardlink=True, symlink=True,
                     source_unchanged=True, stopped_source_user_preserved=True)

                imported("container", definition)
                copied = get("destination", "/containers/" + definition + "/json")
                original = get("source", "/containers/" + source_id + "/json")
                assert copied["State"]["Status"] == "created" and not copied["State"]["Running"]
                for field in ("Cmd", "Entrypoint", "User", "WorkingDir", "Labels"):
                    assert copied["Config"][field] == original["Config"][field], field
                assert "IMPORT_MARKER=" + nonce in copied["Config"]["Env"]
                assert copied["HostConfig"]["NetworkMode"] == "none"
                assert any(m["Name"] == volume and m["Destination"] == "/data" and not m["RW"]
                           for m in copied["Mounts"]), copied["Mounts"]
                assert docker("destination", "start", "-a", copied["Id"]).stdout == payload
                assert get("source", "/containers/" + source_id + "/json")["State"]["Status"] == "created"
                case("container-definition-arrives-stopped-and-runs-with-copied-volume",
                     retried_source_container=source_id)

                # Prove removal through each API before removing its private host.
                for engine in names:
                    for row in get(engine, "/containers/json?all=1"):
                        info = get(engine, "/containers/" + row["Id"] + "/json")
                        assert (info["Config"]["Labels"] or {}).get(OWNER) == nonce
                        docker(engine, "rm", "-f", row["Id"])
                        assert request(engine, "GET", "/containers/" + row["Id"] + "/json")[0] == 404
                    docker(engine, "volume", "rm", volume)
                    assert request(engine, "GET", "/volumes/" + volume)[0] == 404
                    assert get(engine, "/containers/json?all=1") == []
                    assert not get(engine, "/volumes").get("Volumes")
                    record["cleanup"].append({"engine": engine, "containers_and_volume_absent": True})
                record["status"] = "passed"
            finally:
                errors = []
                for engine, name in names.items():
                    inspected = command("docker", "inspect", name, check=False)
                    if inspected.returncode:
                        if engine in ids:
                            errors.append(engine + ": cannot confirm host identity")
                        continue
                    info = json.loads(inspected.stdout)[0]
                    if (info["Config"]["Labels"] or {}).get(OWNER) != nonce:
                        errors.append(engine + ": refuse to remove another run's host")
                        continue
                    logs = command("docker", "logs", info["Id"], check=False)
                    log_path = output / (engine + "-host.log")
                    log_path.write_bytes(logs.stdout + logs.stderr)
                    log_path.chmod(0o644)
                    removed = command("docker", "rm", "-fv", info["Id"], check=False)
                    absent = request("runner", "GET", "/containers/" + info["Id"] + "/json")[0] == 404
                    stores = [m["Name"] for m in info["Mounts"] if m["Type"] == "volume"]
                    stores_absent = all(request("runner", "GET", "/volumes/" + name)[0] == 404
                                        for name in stores)
                    if removed.returncode or not absent or not stores_absent:
                        errors.append(engine + ": host removal failed")
                    else:
                        record["cleanup"].append({"engine_host": engine, "removed": True,
                                                  "anonymous_store_volumes_removed": stores,
                                                  "api_absence_verified": True})
                save()
                if errors:
                    raise RuntimeError("; ".join(errors))
    except BaseException as error:
        record.update(status="failed", error=repr(error))
        if isinstance(error, subprocess.CalledProcessError):
            def text(value):
                return value if isinstance(value, str) else (value or b"").decode(errors="replace")
            record["failed_command"] = {
                "argv": error.cmd, "returncode": error.returncode,
                "stdout": text(error.stdout)[-12000:],
                "stderr": text(error.stderr)[-12000:],
            }
        save()
        raise
    finally:
        save()
        print(json.dumps(record, indent=2), flush=True)


if __name__ == "__main__":
    main()
