#!/usr/bin/env python3
"""Fresh Buildx bootstraps and a real build beside a disposable Linux daemon.

Use a private Docker configuration, and never change the selected builder.
The managed-builder check refuses to touch a pre-existing managed container.
"""
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path):
    prefix = "asagiri-bootstrap-" + uuid.uuid4().hex[:10]
    managed = "asagiri-build"
    managed_container = "buildx_buildkit_" + managed + "0"
    made, images = [], []
    failures = []
    mirrors_before = None
    with tempfile.TemporaryDirectory(prefix=prefix) as temp:
        env = dict(os.environ, DOCKER_CONFIG=temp, DOCKER_HOST="unix://" + path)
        env.pop("DOCKER_CONTEXT", None)

        def cli(*args, ok=True, timeout=180):
            p = subprocess.run(["docker", "--host", "unix://" + path, *args],
                               env=env, capture_output=True, text=True, timeout=timeout)
            if ok:
                assert p.returncode == 0, (args, p.stdout[-4000:], p.stderr[-4000:])
            return p

        def api(method, target, body=None):
            connection = Connection(path)
            try:
                connection.request(method, target, None if body is None else json.dumps(body),
                                   {"Content-Type": "application/json"})
                response = connection.getresponse()
                return response.status, response.read()
            finally:
                connection.close()

        def make(builder):
            made.append(builder)
            cli("buildx", "create", "--name", builder, "--driver", "docker-container",
                "--driver-opt", "default-load=true", "unix://" + path)

        def remove(builder):
            cli("buildx", "rm", "--force", builder)
            made.remove(builder)

        assert cli("container", "inspect", managed_container, ok=False).returncode != 0, \
            "use a disposable engine without an existing managed builder"
        print(cli("version", "--format", "{{json .}}").stdout.strip(), flush=True)
        print(cli("buildx", "version").stdout.strip(), flush=True)
        try:
            for attempt in range(6):
                builder = prefix + "-" + str(attempt)
                make(builder)
                first = cli("buildx", "inspect", builder, "--bootstrap", ok=False)
                if first.returncode:
                    failures.append(attempt + 1)
                    print(f"FAIL initial bootstrap {attempt + 1}: {first.stdout[-4000:]} {first.stderr[-4000:]}", flush=True)
                    print(cli("container", "inspect", "buildx_buildkit_" + builder + "0",
                              "--format", "{{json .State}}", ok=False).stdout, flush=True)
                    cli("buildx", "inspect", builder, "--bootstrap")
                    print(f"Retry {attempt + 1} succeeded", flush=True)
                else:
                    print(f"PASS fresh bootstrap {attempt + 1}/6", flush=True)
                remove(builder)

            make(managed)
            cli("buildx", "inspect", managed, "--bootstrap")
            print(cli("exec", managed_container, "buildkitd", "--version").stdout.strip(), flush=True)
            print(cli("image", "inspect", "moby/buildkit:buildx-stable-1",
                      "--format", "{{.Id}} {{json .RepoDigests}}").stdout.strip(), flush=True)
            context = Path(temp) / "context"
            context.mkdir()
            (context / "Dockerfile").write_text(
                'FROM busybox:stable\nRUN echo asagiri-buildx-ok > /result\nCMD ["cat", "/result"]\n')
            tag = prefix + ":build"
            images.append(tag)
            cli("buildx", "build", "--builder", managed, "--progress=plain",
                "--load", "--tag", tag, str(context), timeout=300)
            assert cli("run", "--rm", tag).stdout.strip() == "asagiri-buildx-ok"
            print("PASS managed Buildx build, load and run", flush=True)

            # A new tag in the same repository still has RepoDigests, but
            # only the local mirror knows that name. Preserve it for FROM.
            local_base = "busybox:" + prefix
            images.append(local_base)
            cli("tag", "busybox:stable", local_base)
            local_tag = prefix + ":local-base"
            images.append(local_tag)
            (context / "Dockerfile").write_text(
                f'FROM {local_base}\nRUN echo local-tag-ok > /result\nCMD ["cat", "/result"]\n')
            cli("buildx", "build", "--builder", managed, "--progress=plain",
                "--load", "--tag", local_tag, str(context), timeout=300)
            assert cli("run", "--rm", local_tag).stdout.strip() == "local-tag-ok"
            print("PASS local FROM tag in a pulled image's repository", flush=True)

            # A remote tag must resolve all its platforms. Export without
            # executing the other architecture, then inspect the ELF itself:
            # an exporter can label native layers with the requested platform.
            native = cli("image", "inspect", "busybox:stable", "--format", "{{.Architecture}}").stdout.strip()
            other, machine = ("arm64", 183) if native == "amd64" else ("amd64", 62)
            (context / "Dockerfile").write_text("FROM busybox:stable\n")
            archive = Path(temp) / "other-platform.tar"
            cli("buildx", "build", "--builder", managed, "--progress=plain",
                "--platform", "linux/" + other, "--pull", "--provenance=false",
                "--output", "type=oci,dest=" + str(archive), str(context), timeout=300)
            with tarfile.open(archive) as oci:
                def blob(digest):
                    algorithm, value = digest.split(":", 1)
                    assert algorithm == "sha256" and len(value) == 64
                    return oci.extractfile("blobs/sha256/" + value)

                def manifest(document):
                    if "layers" in document:
                        return document
                    children = document["manifests"]
                    assert len(children) == 1, document
                    return manifest(json.load(blob(children[0]["digest"])))

                image_manifest = manifest(json.load(oci.extractfile("index.json")))
                binary = None
                for layer in image_manifest["layers"]:
                    with tarfile.open(fileobj=blob(layer["digest"]), mode="r:*") as files:
                        for member in files:
                            # BusyBox's official layer hard-links this path
                            # to bin/[; extractfile resolves that in memory.
                            if member.name.removeprefix("./") == "bin/busybox" and (member.isfile() or member.islnk()):
                                binary = files.extractfile(member).read(20)
                assert binary and binary[:4] == b"\x7fELF", "export has no BusyBox ELF"
                endian = {1: "little", 2: "big"}[binary[5]]
                assert int.from_bytes(binary[18:20], endian) == machine, \
                    f"requested linux/{other}, but the build used a different architecture's layers"
            print(f"PASS remote FROM resolves linux/{other} with matching ELF machine", flush=True)

            # Settings may write the next builder config during a build, but
            # must not stop buildx's active dial-stdio exec. A long-lived exec
            # exercises the same running-exec guard without racing a fast build.
            status, response = api("GET", "/asagiri/registry-mirrors")
            if status == 200:
                mirrors_before = json.loads(response)["mirrors"]
                status, response = api("POST", "/v1.47/containers/" + managed_container + "/exec",
                                       {"Cmd": ["sh", "-c", "sleep 3; echo survived > /tmp/refresh-proof"]})
                assert status == 201, (status, response)
                exec_id = json.loads(response)["Id"]
                status, response = api("POST", "/v1.47/exec/" + exec_id + "/start", {"Detach": True})
                assert status in (200, 204), (status, response)
                status, response = api("GET", "/v1.47/exec/" + exec_id + "/json")
                assert status == 200 and json.loads(response)["Running"], response
                changed = [] if mirrors_before else ["https://mirror.example.invalid"]
                status, response = api("POST", "/asagiri/registry-mirrors", {"mirrors": changed})
                assert status == 200, (status, response)
                deadline = time.monotonic() + 20
                while time.monotonic() < deadline:
                    status, response = api("GET", "/v1.47/exec/" + exec_id + "/json")
                    assert status == 200, response
                    state = json.loads(response)
                    if not state["Running"]:
                        assert state["ExitCode"] == 0, state
                        break
                    time.sleep(0.1)
                else:
                    raise AssertionError("builder exec did not finish")
                assert cli("exec", managed_container, "cat", "/tmp/refresh-proof").stdout.strip() == "survived"
                print("PASS mirror changes preserve an active Buildx container exec", flush=True)

            assert not failures, f"initial bootstraps failed on attempts {failures}; retries are not passes"
            print("6 initial bootstraps and managed Buildx checks passed", flush=True)
        finally:
            if mirrors_before is not None:
                status, response = api("POST", "/asagiri/registry-mirrors", {"mirrors": mirrors_before})
                assert status == 200, (status, response)
            for builder in reversed(made):
                cli("buildx", "rm", "--force", builder, ok=False)
            for image in images:
                cli("image", "rm", "-f", image, ok=False)


if __name__ == "__main__":
    check(sys.argv[1])
