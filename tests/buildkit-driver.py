#!/usr/bin/env python3
"""Exercise the actual Buildx Docker driver on a disposable Linux engine."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=20)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image):
    build_base = os.environ.get("ASAGIRI_TEST_BUILD_BASE") or image
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1", "Use only the private CI engine"
    assert Path(path).is_socket()
    token = uuid.uuid4().hex[:12]
    tags = ["asagiri-buildkit-" + token + ":" + name for name in ("first", "alias", "plain", "one", "two", "cancel", "wrapper")]
    created = []

    def api(method, target, body=None, headers=None):
        connection = Connection(path)
        try:
            connection.request(method, target, None if body is None else json.dumps(body),
                               headers or ({"Content-Type": "application/json"} if body is not None else {}))
            response = connection.getresponse()
            data = response.read()
            return response.status, data, dict(response.getheaders())
        finally:
            connection.close()

    with tempfile.TemporaryDirectory(prefix="asagiri-buildkit-") as directory:
        root = Path(directory)
        context = root / "context"
        context.mkdir()
        config = root / "docker-config"
        config.mkdir(mode=0o700)
        env = {**os.environ, "DOCKER_CONFIG": str(config), "DOCKER_HOST": "unix://" + path,
               "BUILDX_BUILDER": "default", "DOCKER_BUILDKIT": "1"}
        env.pop("DOCKER_CONTEXT", None)
        env.pop("BUILDX_CONFIG", None)
        command = ["docker", "--host", "unix://" + path, "--config", str(config)]

        def cli(*args, check=True, timeout=180):
            result = subprocess.run([*command, *args], cwd=context, env=env,
                                    text=True, capture_output=True, timeout=timeout)
            if check and result.returncode:
                raise AssertionError(f"{args}: exit {result.returncode}\n{result.stdout[-16000:]}\n{result.stderr[-16000:]}")
            return result

        try:
            status, _, headers = api("GET", "/_ping")
            assert status == 200 and headers.get("Builder-Version") == "2", (status, headers)
            for target in ("/grpc", "/session"):
                status, _, _ = api("POST", target)
                assert status == 400, (target, status)
            status, body, _ = api("POST", "/containers/create", {
                "Image": image, "Cmd": ["true"], "EngineService": True, "engine_service": True,
                "Labels": {"dev.asagiri.builder": "1"},
            })
            assert status == 201, (status, body)
            spoofed = json.loads(body)["Id"]
            created.append(spoofed)
            status, body, _ = api("GET", "/containers/json?all=1")
            assert status == 200 and any(item["Id"] == spoofed for item in json.loads(body))
            print("PASS upgrade validation and user fields cannot hide a container", flush=True)

            data = os.urandom(8 << 20)
            (context / "input.bin").write_bytes(data)
            digest = hashlib.sha256(data).hexdigest()
            secret = root / "secret"
            secret.write_text("test-only-" + token)
            secret_digest = hashlib.sha256(secret.read_bytes()).hexdigest()
            dockerfile = (
                f"FROM {build_base}\n"
                "ARG MESSAGE=first\n"
                "COPY input.bin /input.bin\n"
                "RUN --mount=type=secret,id=proof "
                "sha256sum /run/secrets/proof | cut -d' ' -f1 > /secret.digest && "
                "printf '%s' \"$MESSAGE\" > /message\n"
                'CMD ["cat", "/message"]\n'
            )
            (context / "Dockerfile").write_text(dockerfile)
            arguments = ["buildx", "build", "--builder", "default", "--progress", "plain",
                         "--secret", "id=proof,src=" + str(secret), "-t", tags[0], "-t", tags[1],
                         "--iidfile", str(root / "iid"), "--metadata-file", str(root / "metadata.json"), "."]
            result = cli(*arguments, timeout=300)
            print(result.stderr[-6000:], flush=True)
            inspected = json.loads(cli("image", "inspect", tags[0]).stdout)[0]
            assert (root / "iid").read_text().strip() == inspected["Id"], (
                (root / "iid").read_text(), inspected["Id"], (root / "metadata.json").read_text())
            assert json.loads(cli("image", "inspect", tags[1]).stdout)[0]["Id"] == inspected["Id"]
            output = cli("run", "--rm", "--network", "none", tags[0], "sh", "-c",
                         "sha256sum /input.bin; cat /secret.digest; test ! -e /run/secrets/proof").stdout
            assert digest in output and secret_digest in output, output
            status, body, _ = api("GET", "/containers/json?all=1")
            assert status == 200 and not any("buildx_buildkit_asagiri-build0" in item["Names"][0] for item in json.loads(body)), body
            state = json.loads(api("GET", "/asagiri/builder")[1])
            managed = state["container"]
            assert cli("inspect", managed).returncode == 0, "Existing explicit builder references remain usable"
            print("PASS Docker driver, 8 MiB context, secrets, local image loading, aliases and image ID", flush=True)

            cached = cli(*arguments)
            assert "CACHED" in cached.stderr, cached.stderr[-8000:]
            cli("build", "--progress", "plain", "--secret", "id=proof,src=" + str(secret), "-t", tags[2], ".")
            assert cli("run", "--rm", "--network", "none", tags[2]).stdout == "first"
            print("PASS build cache and ordinary docker build", flush=True)

            # A client-requested output belongs to that client, never to the
            # adapter's image importer. The same session service carries it.
            target = root / "rootfs.tar"
            cli("buildx", "build", "--builder", "default", "--progress", "plain",
                "--secret", "id=proof,src=" + str(secret), "--output", "type=tar,dest=" + str(target), ".")
            with tarfile.open(target) as archive:
                member = next(item for item in archive.getmembers() if item.name.lstrip("./") == "input.bin")
                assert hashlib.sha256(archive.extractfile(member).read()).hexdigest() == digest
            print("PASS explicit tar output stays with the original client", flush=True)

            bake = {
                "group": {"default": {"targets": ["one", "two"]}},
                "target": {name: {"context": ".", "dockerfile": "Dockerfile", "tags": [tags[i]],
                                  "args": {"MESSAGE": name}, "secret": ["id=proof,src=" + str(secret)]}
                           for i, name in enumerate(("one", "two"), 3)},
            }
            bakefile = root / "docker-bake.json"
            bakefile.write_text(json.dumps(bake))
            cli("buildx", "bake", "--builder", "default", "--progress", "plain",
                "--allow", "fs.read=" + str(secret), "-f", str(bakefile))
            for i, name in enumerate(("one", "two"), 3):
                assert cli("run", "--rm", "--network", "none", tags[i]).stdout == name
            print("PASS concurrent Bake targets keep their exports separate", flush=True)

            # --add-host reaches the host gateway label returned by the
            # adapted ListWorkers response, not the backend's loopback.
            (context / "Gatewayfile").write_text(f"FROM {build_base}\nRUN grep -E '^[0-9.]+[[:space:]]+probe.host' /etc/hosts\n")
            cli("buildx", "build", "--builder", "default", "--progress", "plain",
                "--add-host", "probe.host:host-gateway", "-f", "Gatewayfile", ".")
            print("PASS host-gateway build argument", flush=True)

            # Interrupt an actual running solve, then immediately reuse the
            # backend. Kill only this process group, created for this test.
            (context / "Cancelfile").write_text(f"FROM {build_base}\nRUN echo asagiri-cancel-{token}; sleep 120\n")
            cancel_log = root / "cancel.log"
            with cancel_log.open("w") as output:
                process = subprocess.Popen([*command, "buildx", "build", "--builder", "default",
                    "--progress", "plain", "-t", tags[5], "-f", "Cancelfile", "."],
                    cwd=context, env=env, stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    deadline = time.monotonic() + 60
                    while time.monotonic() < deadline:
                        if re.search(r"(?m)^#\d+ \d+\.\d+ asagiri-cancel-" + token + r"\s*$", cancel_log.read_text()):
                            break
                        assert process.poll() is None, cancel_log.read_text()
                        time.sleep(0.1)
                    else:
                        raise AssertionError("cancellable build did not start\n" + cancel_log.read_text())
                    time.sleep(0.5)
                    os.killpg(process.pid, signal.SIGINT)
                    process.wait(timeout=20)
                    assert process.returncode != 0, cancel_log.read_text()
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait(timeout=10)
            assert cli("image", "inspect", tags[5], check=False).returncode != 0
            cli(*arguments, timeout=45)
            print("PASS cancellation does not publish an image or block the next build", flush=True)

            # A real upgrade of the former context shadow. Keep the explicit
            # global builder (Buildx cannot select an implicit context globally),
            # but asagiri build must use the engine's own Docker driver.
            cli("context", "create", "asagiri", "--docker", "host=unix://" + path)
            cli("buildx", "create", "--name", "asagiri-build", "--driver", "docker-container",
                "--driver-opt", "default-load=true", "asagiri")
            instances = config / "buildx/instances"
            shadow = json.loads((instances / "asagiri-build").read_text())
            shadow["Name"] = "asagiri"
            (instances / "asagiri").write_text(json.dumps(shadow))
            before_config = json.loads((config / "config.json").read_text()) if (config / "config.json").exists() else {}
            before_config["aliases"] = {"builder": "my-previous-builder"}
            (config / "config.json").write_text(json.dumps(before_config))
            previous = '{"Key":"other-engine","Name":"previous","Global":false}'
            current = config / "buildx/current"
            current.write_text(previous)
            home = root / "home"
            home.mkdir(mode=0o700)
            asagiri_env = {**env, "HOME": str(home), "ASAGIRI_SOCKET": path}
            asagiri_env.pop("BUILDX_BUILDER", None)

            def asagiri(*args):
                result = subprocess.run(["/opt/asagiri/asagiri", *args], cwd=context, env=asagiri_env,
                                        text=True, capture_output=True, timeout=120)
                assert result.returncode == 0, (args, result.stdout, result.stderr)
                return result

            asagiri("build-with", "asagiri")
            assert not (instances / "asagiri").exists(), "Only Asagiri's exact shadow should be removed"
            assert (instances / "asagiri-build").exists(), "Keep the explicit builder and cache"
            state = json.loads((home / ".asagiri/build.json").read_text())
            assert state["builtin"] is True and state["before"]["current"] == previous, state
            assert json.loads(current.read_text())["Name"] == "asagiri-build"
            wrapped = asagiri("build", "--progress", "plain", "--secret", "id=proof,src=" + str(secret), "-t", tags[6], ".")
            assert 'using docker driver' in wrapped.stderr, wrapped.stderr[-8000:]
            assert cli("run", "--rm", "--network", "none", tags[6]).stdout == "first"
            # Buildx may update its current file while following an explicit
            # builder. Selecting Asagiri again must keep the FIRST snapshot.
            asagiri("build-with", "asagiri")
            asagiri("build-with", "docker")
            assert current.read_text() == previous
            assert json.loads((config / "config.json").read_text())["aliases"] == before_config["aliases"]
            assert not (instances / "asagiri").exists()
            print("PASS real builder migration, native CLI builds, cache retention and exact preference restoration", flush=True)
        finally:
            if created:
                cli("rm", "-f", *created, check=False)
            cli("image", "rm", "-f", *tags, check=False)
    print("BuildKit Docker driver checks passed", flush=True)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
