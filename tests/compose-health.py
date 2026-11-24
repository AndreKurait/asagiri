#!/usr/bin/env python3
"""Run a pinned real Compose client against owned build/health/bind projects."""
import argparse
import datetime
import hashlib
import http.client
import json
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.parse
import uuid

LABEL = "dev.asagiri.compose-health-test"


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def timestamp(value):
    return datetime.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()


def project_files(image, arch, token, context, bind, project):
    service = {
        "image": image, "platform": "linux/" + arch, "network_mode": "none",
        "labels": {LABEL: token}, "cap_drop": ["ALL"], "read_only": True,
        "security_opt": ["no-new-privileges"], "user": "65534:65534", "init": True,
        "mem_limit": "64m", "pids_limit": 32, "tmpfs": ["/tmp:uid=65534,gid=65534,mode=0700"],
    }
    compose = {
        "services": {
            "db": {**service,
                   "command": ["sh", "-ec", "sleep 2; touch /tmp/ready; exec sleep 3600"],
                   "healthcheck": {"test": ["CMD", "test", "-f", "/tmp/ready"],
                                   "interval": "30s", "start_period": "20s",
                                   "start_interval": "250ms", "timeout": "1s", "retries": 2}},
            "completed": {**service, "command": ["sh", "-ec", "printf completed"],
                          "depends_on": {"db": {"condition": "service_healthy"}}},
            "app": {**service, "image": project + ":built", "pull_policy": "never",
                    "build": {"context": str(context)}, "volumes": ["proof:/proof"],
                    "command": ["sh", "-ec",
                                "echo $$$$ > /proof/app.pid; cp /proof/host.bin /proof/container.bin; exec sleep 3600"],
                    "depends_on": {"db": {"condition": "service_healthy"},
                                   "completed": {"condition": "service_completed_successfully"}}},
        },
        "volumes": {"proof": {"name": project + "_proof", "driver": "local", "labels": {LABEL: token},
                             "driver_opts": {"type": "none", "o": "bind", "device": str(bind)}}},
    }
    failure = {"services": {
        "completed": {**service, "command": ["sh", "-c", "exit 23"]},
        "app": {**service, "command": ["sleep", "3600"],
                "depends_on": {"completed": {"condition": "service_completed_successfully"}}},
    }}
    return compose, failure


def check(args):
    assert sys.platform in ("linux", "darwin"), "Unsupported Compose fixture platform"
    pins = json.loads((Path(__file__).parent / "fixtures/compose/client.json").read_text())
    pinned = pins[sys.platform][platform.machine()]
    plugin_sha = hashlib.sha256(args.compose_plugin.read_bytes()).hexdigest()
    assert plugin_sha == pinned["sha256"] and args.compose_plugin.stat().st_size == pinned["size"]
    token = uuid.uuid4().hex[:16]
    project = "asagiri-health-" + token
    failure_project = project + "-failed"
    image_tag = project + ":built"
    volume_name = project + "_proof"
    names = [project + "-" + service + "-1" for service in ("db", "completed", "app")]
    names += [failure_project + "-" + service + "-1" for service in ("completed", "app")]
    record = {
        "schema": "asagiri.compose-health.v1", "status": "running",
        "compose_version": pins["version"], "compose_sha256": plugin_sha,
        "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "owner_token": token, "container_names": names, "volume_name": volume_name,
        "image_tag": image_tag, "cases": [], "commands": [], "cleanup": [],
    }

    def save():
        if args.output:
            temporary = args.output.with_suffix(args.output.suffix + ".tmp")
            temporary.write_text(json.dumps(record, indent=2) + "\n")
            temporary.replace(args.output)

    def request(method, path, body=None):
        connection = Connection(args.socket)
        try:
            data = None if body is None else json.dumps(body).encode()
            connection.request(method, "/v1.47" + path, data,
                               {} if data is None else {"Content-Type": "application/json"})
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    def api(method, path, expected=200, body=None):
        status, raw = request(method, path, body)
        assert status == expected, (method, path, status, raw)
        return json.loads(raw) if raw else None

    def image_path(tag):
        return "/images/" + urllib.parse.quote(tag, safe="")

    version = api("GET", "/version")
    if sys.platform == "darwin":
        assert not args.reference_docker and version.get("AsagiriVersion"), "Use a disposable Asagiri engine"
        assert Path(args.socket) == Path.cwd() / "docker.sock" and os.environ.get("ASAGIRI_SOCKET") == args.socket
        assert os.environ.get("ASAGIRI_DATA_DIR") == str(Path.cwd() / "engine-data")
        assert args.buildx_plugin and args.buildx_plugin.is_file(), "Pass the packaged macOS Buildx plugin"
    elif args.reference_docker:
        assert not version.get("AsagiriVersion") and "Podman" not in json.dumps(version), version
    else:
        assert (version.get("AsagiriVersion") and Path("/.dockerenv").exists()
                and os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
                and os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
                and os.environ.get("ASAGIRI_SOCKET") == args.socket), "Use a disposable Asagiri engine"
    base = api("GET", image_path(args.image) + "/json")
    arch = base["Architecture"]
    assert arch in ("amd64", "arm64") and base["Os"] == "linux", base
    record.update(engine_version=version.get("AsagiriVersion", version["Version"]),
                  base_image_id=base["Id"], platform="linux/" + arch)
    for name in names:
        assert request("GET", "/containers/" + name + "/json")[0] == 404, name
    assert request("GET", "/volumes/" + volume_name)[0] == 404, volume_name
    assert request("GET", image_path(image_tag) + "/json")[0] == 404, image_tag
    save()
    with tempfile.TemporaryDirectory(prefix=project + "-") as directory:
        root = Path(directory)
        config = root / "config"
        config.mkdir(mode=0o700)
        plugins = config / "cli-plugins"
        plugins.mkdir(mode=0o700)
        shutil.copyfile(args.compose_plugin, plugins / "docker-compose")
        (plugins / "docker-compose").chmod(0o755)
        if args.buildx_plugin:
            shutil.copy2(args.buildx_plugin, plugins / "docker-buildx")
        bind = root / "bind"
        bind.mkdir(mode=0o777)
        bind.chmod(0o777)
        # The daemon mounts this exact folder; an unprivileged app writes only
        # into this fresh test directory.
        root.chmod(0o755)
        payload = bytes(range(256)) * 257
        (bind / "host.bin").write_bytes(payload)
        (bind / "host.bin").chmod(0o644)
        context = root / "app"
        context.mkdir()
        (context / "built.txt").write_text(token)
        (context / "Dockerfile").write_text(
            f"FROM {args.image}\nCOPY built.txt /built.txt\n"
            f"LABEL {LABEL}={token}\n"
            'CMD ["sleep","3600"]\n'
        )
        compose, failure = project_files(args.image, arch, token, context, bind, project)
        compose_path = root / "compose.json"
        compose_path.write_text(json.dumps(compose, indent=2) + "\n")
        failure_path = root / "failure.json"
        failure_path.write_text(json.dumps(failure, indent=2) + "\n")
        env = dict(os.environ, DOCKER_CONFIG=str(config), DOCKER_HOST="unix://" + args.socket,
                   DOCKER_BUILDKIT="1", BUILDX_BUILDER="default", COMPOSE_BAKE="false")
        for key in ("DOCKER_CONTEXT", "BUILDX_CONFIG", "COMPOSE_FILE", "COMPOSE_PROJECT_NAME",
                    "COMPOSE_PROFILES", "COMPOSE_ENV_FILES", "DOCKER_API_VERSION"):
            env.pop(key, None)
        command = ["docker", "--host", "unix://" + args.socket, "--config", str(config)]

        def cli(*arguments, ok=True, timeout=60):
            start = time.monotonic()
            result = subprocess.run([*command, *arguments], cwd=root, env=env, text=True,
                                    capture_output=True, timeout=timeout)
            record["commands"].append({
                "arguments": list(arguments), "exit_code": result.returncode,
                "elapsed_seconds": time.monotonic() - start,
                "stdout": result.stdout[-8000:], "stderr": result.stderr[-16000:],
            })
            save()
            assert not ok or result.returncode == 0, (arguments, result.returncode, result.stdout[-8000:], result.stderr[-16000:])
            return result

        def passed(case, **details):
            record["cases"].append({"case": case, "status": "passed", **details})
            save()

        def inspect(name):
            info = api("GET", "/containers/" + name + "/json")
            assert (info["Config"]["Labels"] or {}).get(LABEL) == token, info
            return info

        try:
            actual_version = cli("compose", "version", "--short").stdout.strip().removeprefix("v")
            assert actual_version == pins["version"], actual_version
            cli("compose", "-p", project, "-f", str(compose_path), "up", "-d", "--build",
                "--wait", "--wait-timeout", "60", timeout=300)
            passed("compose-up-build-wait")
            db, completed, app = (inspect(name) for name in names[:3])
            assert db["State"]["Health"]["Status"] == "healthy", db["State"]
            assert completed["State"]["ExitCode"] == 0 and not completed["State"]["Running"], completed["State"]
            assert app["State"]["Running"], app["State"]
            healthy = [p for p in db["State"]["Health"]["Log"] if p["ExitCode"] == 0]
            assert healthy and timestamp(app["State"]["StartedAt"]) >= min(timestamp(p["End"]) for p in healthy)
            delay = timestamp(app["State"]["StartedAt"]) - timestamp(db["State"]["StartedAt"])
            assert delay >= 2, (delay, db["State"], app["State"])
            passed("service-healthy-dependency", start_delay_seconds=delay)
            assert timestamp(app["State"]["StartedAt"]) >= timestamp(completed["State"]["FinishedAt"])
            passed("service-completed-successfully-dependency")
            built = api("GET", image_path(image_tag) + "/json")
            assert built["Architecture"] == arch and built["Os"] == "linux", built
            assert built["RootFS"]["Layers"][:len(base["RootFS"]["Layers"])] == base["RootFS"]["Layers"]
            assert cli("exec", app["Id"], "cat", "/built.txt").stdout == token
            passed("native-platform-and-built-image", image_id=built["Id"])
            assert (bind / "container.bin").read_bytes() == payload
            assert cli("exec", app["Id"], "sha256sum", "/proof/host.bin").stdout.split()[0] == hashlib.sha256(payload).hexdigest()
            assert int((bind / "app.pid").read_text()) > 1
            passed("local-volume-bind-roundtrip", bytes=len(payload), sha256=hashlib.sha256(payload).hexdigest())
            status = cli("exec", app["Id"], "cat", "/proc/self/status").stdout
            fields = dict(line.split(":", 1) for line in status.splitlines() if ":" in line)
            assert int(fields["CapEff"].strip(), 16) == 0 and fields["NoNewPrivs"].strip() == "1", status
            interfaces = cli("exec", app["Id"], "cat", "/proc/net/dev").stdout.splitlines()[2:]
            assert [line.split(":", 1)[0].strip() for line in interfaces] == ["lo"], interfaces
            assert cli("exec", app["Id"], "stat", "-c", "%u:%g:%a", "/tmp").stdout.strip() == "65534:65534:700"
            assert cli("exec", app["Id"], "sh", "-ec", "echo writable > /tmp/proof; cat /tmp/proof").stdout.strip() == "writable"
            root_write = cli("exec", "--user", "0", app["Id"], "touch", "/readonly-proof", ok=False)
            assert root_write.returncode != 0 and "Read-only file system" in root_write.stderr + root_write.stdout, root_write
            passed("init-readonly-capabilities-nnp-network-and-tmpfs")
            failure = cli("compose", "-p", failure_project, "-f", str(failure_path), "up", "-d", ok=False)
            assert failure.returncode != 0 and "23" in failure.stderr + failure.stdout, failure
            failed = inspect(names[3])
            assert failed["State"]["ExitCode"] == 23 and not failed["State"]["Running"], failed["State"]
            code, raw = request("GET", "/containers/" + names[4] + "/json")
            if code != 404:
                assert code == 200, (code, raw)
                dependent = json.loads(raw)
                assert (dependent["State"]["Status"] == "created" and not dependent["State"]["Running"]
                        and dependent["State"]["StartedAt"].startswith("0001-")), dependent["State"]
            passed("failed-completion-blocks-dependent")
            # Every object in the successful project is checked before Compose
            # removes it; no global prune or shared context is used.
            for name in names[:3]:
                inspect(name)
            volume = api("GET", "/volumes/" + volume_name)
            assert (volume["Labels"] or {}).get(LABEL) == token
            cli("compose", "-p", project, "-f", str(compose_path), "down", "--volumes", "--timeout", "2")
            assert request("GET", "/volumes/" + volume_name)[0] == 404
            assert all(request("GET", "/containers/" + name + "/json")[0] == 404 for name in names[:3])
            assert (bind / "host.bin").read_bytes() == (bind / "container.bin").read_bytes() == payload
            passed("compose-down-preserves-bind-data")
            record["status"] = "passed"
        except BaseException as error:
            record.update(status="failed", error=repr(error))
            raise
        finally:
            errors = []
            for kind, name in [("container", n) for n in names] + [("volume", volume_name), ("image", image_tag)]:
                try:
                    path = ("/containers/" + name if kind == "container" else
                            "/volumes/" + name if kind == "volume" else image_path(name))
                    inspect_path = path + ("/json" if kind in ("container", "image") else "")
                    code, raw = request("GET", inspect_path)
                    if code != 404:
                        assert code == 200, (path, code, raw)
                        info = json.loads(raw)
                        labels = info["Config"].get("Labels") if kind in ("container", "image") else info.get("Labels")
                        assert (labels or {}).get(LABEL) == token, (path, info)
                        status, raw = request("DELETE", path + ("?force=true" if kind == "container" else ""))
                        assert status in (200, 204), (path, status, raw)
                    status, raw = request("GET", inspect_path)
                    assert status == 404, (path, status, raw)
                    record["cleanup"].append({"kind": kind, "name": name, "status": 404})
                except BaseException as error:
                    errors.append({"kind": kind, "name": name, "error": repr(error)})
            if errors:
                record.update(status="failed", cleanup_errors=errors)
            assert (bind / "host.bin").read_bytes() == payload, "The source bind data changed"
            save()
            print(json.dumps(record, sort_keys=True), flush=True)
            assert not errors, errors


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image")
    parser.add_argument("--reference-docker", action="store_true")
    parser.add_argument("--compose-plugin", type=Path, required=True)
    parser.add_argument("--buildx-plugin", type=Path, help="Buildx executable from the macOS release bundle")
    parser.add_argument("--output", type=Path)
    options = parser.parse_args()
    if options.output:
        with options.output.open("x"):
            pass
    check(options)
