#!/usr/bin/env python3
"""Qualify registry CA trust in a new, disposable Linux engine host.

Preparation only unless `qualify --execute` is explicitly supplied. This tests
a TLS OCI protocol fixture, not a deployed Distribution/authentication service.
The outer Docker engine is used only to create and remove this qualifier's own
host, private network and anonymous volumes. No outer socket or ports are shared.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import gzip
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import ipaddress
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import socket
import ssl
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import tomllib
from urllib.parse import quote, urlsplit
import uuid

SOCKET = "/run/asagiri/docker.sock"
REFERENCE_SOCKET = "/run/registry-ca-reference.sock"
WRONG_NAME = "registry-ca-wrong-name.invalid"
MAX_API_BYTES = 32 * 1024**2
CHECKS = [
    "TLS fixture verified independently",
    "reference Docker trusted pull",
    "reference Docker wrong CA",
    "reference Docker wrong name",
    "reference Docker unconfigured authority",
    "engine rejects default roots",
    "engine trusted pull",
    "engine wrong CA",
    "engine wrong name",
    "engine unconfigured authority",
    "BuildKit trusted export",
    "BuildKit initial CA snapshot",
    "leading-space PEM rejected before persistence",
    "inter-block-space PEM rejected before persistence",
    "concatenated PEM rejected before persistence",
    "BuildKit wrong CA",
    "BuildKit wrong name",
    "BuildKit unconfigured authority",
    "engine revoked CA",
    "BuildKit revoked CA",
    "BuildKit revoked snapshot",
    "engine rotated CA",
    "BuildKit rotated export",
    "BuildKit rotated snapshot",
    "active Docker driver refuses changed trust",
    "active classic build refuses changed trust",
    "active container driver refuses changed trust",
    "failed apply persists desired policy",
    "identical failed apply remains pending",
    "direct start refuses failed installation",
    "native build refuses failed installation",
    "identical repaired apply awaits start",
    "BuildKit repaired export",
    "BuildKit repaired CA snapshot",
]


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def file_sha(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024**2), b""):
            digest.update(chunk)
    return digest.hexdigest()


def now():
    return datetime.now(timezone.utc).isoformat()


def write_json(path, data):
    Path(path).write_text(json.dumps(data, indent=2) + "\n")


def tar_bytes(name, data):
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w") as archive:
        entry = tarfile.TarInfo(name)
        entry.mode, entry.mtime, entry.size = 0o644, 0, len(data)
        archive.addfile(entry, io.BytesIO(data))
    return out.getvalue()


def archive_file(data, expected):
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:*") as archive:
        entries = [entry for entry in archive.getmembers()
                   if entry.name in (expected, "./" + expected)]
        require(len(entries) == 1 and entries[0].isfile(), "Archive lacks the exact regular proof file")
        require(entries[0].size <= MAX_API_BYTES, "Archive proof file is too large")
        return archive.extractfile(entries[0]).read()


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "Duplicate JSON key: " + key)
        result[key] = value
    return result


def decode(raw):
    return json.loads(raw, object_pairs_hook=unique_object)


def pull_error_messages(status, raw, reference, success):
    require(type(status) is int and type(reference) is bool and type(success) is bool,
            "Malformed pull status/client/outcome")
    if status == 500:
        require(reference and not success, "HTTP 500 is only expected for a rejected reference pull")
        error = decode(raw)
        require(isinstance(error, dict) and set(error) == {"message"}
                and isinstance(error["message"], str) and error["message"].strip(),
                "Reference HTTP 500 lacks a single nonempty JSON message")
        return [error["message"]]
    require(status == 200, "Unexpected pull HTTP status")
    errors = []
    for row in (decode(line) for line in raw.splitlines() if line):
        require(isinstance(row, dict), "Pull stream contains a non-object record")
        error = row.get("error")
        require(error is None or isinstance(error, str), "Pull error is not a string")
        if error:
            errors.append(error)
    return errors


def assert_pulled_image_identity(inspected, image, tag, reference, architecture, token):
    """Check the exact fixture content and the selected engine's image ID."""
    expected_id = image["config_digest"]
    if reference:
        # The pinned Docker containerd store identifies this single-platform
        # image by its manifest target. Asagiri retains its config-digest ID.
        expected_id = image["manifest_digest"]
        require(inspected.get("Descriptor") == {
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": image["manifest_digest"], "size": len(image["manifest"]),
        } and inspected.get("RepoDigests") == [tag.rsplit(":", 1)[0] + "@" + image["manifest_digest"]],
                "Reference Docker manifest descriptor/repository identity differs from the OCI fixture")
    require(inspected["Id"] == expected_id
            and inspected["RootFS"]["Type"] == "layers"
            and inspected["RootFS"]["Layers"] == [image["diff_id"]]
            and inspected["Architecture"] == architecture and inspected["Os"] == "linux"
            and inspected["Config"]["Labels"].get("dev.asagiri.registry-ca-fixture") == token,
            "Pulled image identity/content differs from the OCI fixture")


class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=180)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


class Registry:
    """Small real OCI pull endpoint with per-repository content and TLS."""
    def __init__(self, address, context, fingerprints, token, name):
        self.token, self.name = token, name
        self.fingerprints = fingerprints
        self.lock = threading.Lock()
        self.images, self.blobs, self.requests = {}, {}, []
        self.gate_repo = None
        self.entered, self.release = threading.Event(), threading.Event()
        self.release.set()
        self.gate_expired = False
        model = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def reply(self, status, body=b"", media="application/json", digest=None):
                content_digest = "sha256:" + sha(body)
                if digest is not None:
                    require(digest == content_digest, "Fixture response has the wrong content digest")
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Content-Type", media)
                self.send_header("Docker-Distribution-Api-Version", "registry/2.0")
                # Every request observes the current leaf certificate. A pooled
                # TLS connection must not conceal a leaf rotation in this test.
                self.send_header("Connection", "close")
                self.close_connection = True
                if digest:
                    self.send_header("Docker-Content-Digest", content_digest)
                self.end_headers()
                if self.command != "HEAD" and body:
                    self.wfile.write(body)

            def request(self):
                path = urlsplit(self.path).path
                with model.lock:
                    model.requests.append({"method": self.command, "path": path,
                                           "peer": self.client_address[0], "at": now(),
                                           "certificate_sha256": model.fingerprints[self.connection.context]})
                if path == "/v2/":
                    self.reply(200, b"{}")
                    return
                match = re.fullmatch(r"/v2/(registryca/[a-z0-9-]+)/(manifests|blobs)/([^/]+)", path)
                if not match:
                    self.reply(404)
                    return
                repository, kind, ref = match.groups()
                image = model.image(repository)
                if repository == model.gate_repo:
                    model.entered.set()
                    if not model.release.wait(45):
                        model.gate_expired = True
                        self.reply(504)
                        return
                if kind == "manifests":
                    if ref not in ("latest", image["manifest_digest"]):
                        self.reply(404)
                    else:
                        self.reply(200, image["manifest"], "application/vnd.oci.image.manifest.v1+json",
                                   image["manifest_digest"])
                else:
                    with model.lock:
                        body = model.blobs.get(ref)
                    if body is None:
                        self.reply(404)
                    else:
                        self.reply(200, body, "application/octet-stream", ref)

            do_GET = request
            do_HEAD = request

        class Server(ThreadingHTTPServer):
            daemon_threads = True

            def get_request(self):
                stream, peer = super().get_request()
                stream.settimeout(10)
                return model.context.wrap_socket(stream, server_side=True,
                                                  do_handshake_on_connect=False), peer

            def handle_error(self, _request, _address):
                # Expected TLS rejections never become HTTP registry requests.
                pass

        self.context = context
        self.server = Server((address, 0), Handler)
        self.authority = f"{address}:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def image(self, repository):
        with self.lock:
            if repository in self.images:
                return self.images[repository]
            proof = f"asagiri-registry-ca {self.token} {self.name} {repository}\n".encode()
            raw_layer = tar_bytes("registry-ca-proof", proof)
            layer = gzip.compress(raw_layer, mtime=0)
            oci_sha = lambda data: "sha256:" + sha(data)
            config = json.dumps({
                "architecture": {"aarch64": "arm64", "x86_64": "amd64"}[platform.machine()],
                "os": "linux", "config": {"Labels": {"dev.asagiri.registry-ca-fixture": self.token}},
                "rootfs": {"type": "layers", "diff_ids": [oci_sha(raw_layer)]},
            }, sort_keys=True).encode()
            manifest = json.dumps({
                "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                           "digest": oci_sha(config), "size": len(config)},
                "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                            "digest": oci_sha(layer), "size": len(layer)}],
            }, sort_keys=True).encode()
            image = {"manifest": manifest, "manifest_digest": oci_sha(manifest),
                     "config_digest": oci_sha(config), "diff_id": oci_sha(raw_layer),
                     "layer_digest": oci_sha(layer), "proof": proof}
            self.blobs[oci_sha(config)], self.blobs[oci_sha(layer)] = config, layer
            self.images[repository] = image
            return image

    def seen(self, repository):
        prefix = "/v2/" + repository + "/"
        with self.lock:
            return [item.copy() for item in self.requests if item["path"].startswith(prefix)]

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


class Qualifier:
    def __init__(self, args):
        require(sys.platform == "linux" and os.geteuid() == 0, "Private Linux host only")
        require(re.fullmatch(r"[0-9a-f]{32}", args.token)
                and os.environ.get("ASAGIRI_REGISTRY_CA_TOKEN") == args.token
                and os.environ.get("ASAGIRI_TEST_ENGINE") == "1",
                "Missing task-owned private-engine scope")
        address = ipaddress.ip_address(args.address)
        require(address.version == 4 and address.is_private and not address.is_loopback,
                "Registry must use the host's non-loopback private address")
        require(socket.gethostbyname(socket.gethostname()) == args.address,
                "Registry address is not this disposable host")
        require(os.environ.get("ASAGIRI_SOCKET") == SOCKET and Path(SOCKET).is_socket(),
                "Unexpected engine socket")
        self.args, self.root = args, args.evidence
        self.root.mkdir(parents=True, exist_ok=False)
        self.events = (self.root / "events.jsonl").open("w")
        self.events_lock = threading.Lock()
        self.api_index = 0
        (self.root / "api").mkdir()
        self.models = {}
        self.reference_process = None
        self.saved_etc = None
        self.fault_installed = False
        self.config_file = Path.home() / ".asagiri" / "daemon.json"
        self.cli_config = self.root / "docker-config"
        self.cli_config.mkdir(mode=0o700)
        self.environment = dict(os.environ)
        for key in ("DOCKER_CONTEXT", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH", "BUILDX_CONFIG", "BUILDX_BUILDER"):
            self.environment.pop(key, None)
        self.environment.update(DOCKER_HOST="unix://" + SOCKET, DOCKER_CONFIG=str(self.cli_config),
                                DOCKER_BUILDKIT="1")
        self.record = {"schema": "asagiri.registry-ca-private.v1", "status": "running",
                       "source": args.source, "source_tree": args.source_tree,
                       "architecture": args.architecture, "qualifier_sha256": file_sha(__file__),
                       "expected_reference_version": args.reference_version,
                       "started_at": now(), "token": args.token,
                       "address": args.address, "checks": [], "scope": "TLS OCI protocol fixture",
                       "binaries": {name: file_sha(Path("/opt/asagiri") / name)
                                    for name in ("asagiri", "asagiri-init")}}
        require(Path("/opt/asagiri/SOURCE_COMMIT").read_text().strip() == args.source,
                "Inner binary source stamp mismatch")
        require(self.record["qualifier_sha256"] == args.qualifier_sha256,
                "Copied qualifier differs from the outer script")
        require({"aarch64": "arm64", "x86_64": "amd64"}.get(platform.machine()) == args.architecture,
                "Unexpected inner runtime architecture")

    def event(self, **data):
        with self.events_lock:
            self.events.write(json.dumps({"at": now(), **data}, sort_keys=True) + "\n")
            self.events.flush()

    def result(self, name, **details):
        require(name in CHECKS and name not in [row["name"] for row in self.record["checks"]],
                "Unexpected or duplicate qualification check")
        self.record["checks"].append({"name": name, "status": "passed", **details})
        write_json(self.root / "verification.json", self.record)
        print("PASS " + name, flush=True)

    def api(self, method, path, body=None, expected=200, reference=False, raw=False):
        data = body if raw else (None if body is None else json.dumps(body).encode())
        connection = UnixConnection(REFERENCE_SOCKET if reference else SOCKET)
        try:
            connection.request(method, path, data, {
                "Content-Type": "application/x-tar" if raw else "application/json",
            })
            response = connection.getresponse()
            payload = response.read(MAX_API_BYTES + 1)
            require(len(payload) <= MAX_API_BYTES, "API response exceeds qualifier bound")
            status = response.status
        finally:
            connection.close()
        with self.events_lock:
            self.api_index += 1
            prefix = "api/" + str(self.api_index).zfill(5)
        # The stopped builder's whole /etc backup is needed only in memory for
        # restoration. Keep its digest, not arbitrary image-owned key material.
        omit_request = raw and self.saved_etc is not None and body is self.saved_etc
        omit_response = method == "GET" and path == (
            "/containers/" + self.record.get("fault_container", "") + "/archive?path=/etc")
        request_path = prefix + ".request" if data is not None and not omit_request else None
        response_path = None if omit_response else prefix + ".response"
        if request_path is not None:
            (self.root / request_path).write_bytes(data)
        if response_path is not None:
            (self.root / response_path).write_bytes(payload)
        self.event(kind="api", reference=reference, method=method, path=path, status=status,
                   request_sha256=sha(data) if data is not None else None,
                   response_sha256=sha(payload), response_bytes=len(payload),
                   request_artifact=request_path, response_artifact=response_path,
                   omitted_request="whole-builder-etc-archive" if omit_request else None,
                   omitted_response="whole-builder-etc-archive" if omit_response else None)
        if expected is not None:
            require(status == expected, f"{method} {path}: HTTP {status}: {payload[:3000]!r}")
        return status, payload

    def config(self, patch=None, expected=200):
        status, data = self.api("GET" if patch is None else "POST", "/asagiri/daemon-config",
                                patch, expected=expected)
        return status, json.loads(data)

    def saved_config(self):
        return json.loads(self.config_file.read_bytes())

    def policy(self, bundles):
        patch = {"registry-ca-certificates": bundles, "insecure-registries": [], "registry-mirrors": []}
        _, report = self.config(patch)
        require(report["registry-ca-certificates"] == bundles
                and report["insecure-registries"] == [] and report["registry-mirrors"] == [],
                "Active CA policy differs from requested exact-authority trust")
        require(self.saved_config() == report, "Persisted CA policy differs from active configuration")
        return report

    def command(self, name, arguments, timeout=300):
        require(re.fullmatch(r"[a-z0-9-]+", name), "Invalid command evidence name")
        with subprocess.Popen(arguments, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              env=self.environment, start_new_session=True) as process:
            try:
                stdout, stderr = process.communicate(timeout=timeout)
            except subprocess.TimeoutExpired:
                # The still-owned, unreaped leader pins its private process group.
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                stdout, stderr = process.communicate(timeout=15)
                self.event(kind="timeout", name=name, argv=arguments, timeout=timeout)
                (self.root / (name + ".stdout")).write_bytes(stdout)
                (self.root / (name + ".stderr")).write_bytes(stderr)
                raise TimeoutError(f"{name} exceeded {timeout}s")
        (self.root / (name + ".stdout")).write_bytes(stdout)
        (self.root / (name + ".stderr")).write_bytes(stderr)
        self.event(kind="command", name=name, argv=arguments, exit=process.returncode,
                   stdout_sha256=sha(stdout), stderr_sha256=sha(stderr))
        return process.returncode, stdout, stderr

    def docker(self, name, *arguments, timeout=300):
        return self.command(name, ["docker", "--host", "unix://" + SOCKET,
                                  "--config", str(self.cli_config), *arguments], timeout)

    def builder(self):
        _, raw = self.api("GET", "/asagiri/builder")
        name = json.loads(raw)["container"]
        require(re.fullmatch(r"buildx_buildkit_[a-z0-9-]+0", name), "Unexpected managed builder name")
        _, data = self.api("GET", "/containers/" + name + "/json")
        return json.loads(data)

    def container_ids(self):
        _, raw = self.api("GET", "/containers/json?all=true")
        rows = decode(raw)
        require(isinstance(rows, list), "Invalid container inventory")
        ids = []
        for row in rows:
            require(isinstance(row, dict) and re.fullmatch(r"[0-9a-f]{64}", row.get("Id", "")),
                    "Invalid container inventory identity")
            ids.append(row["Id"])
        require(len(ids) == len(set(ids)), "Duplicate container inventory identity")
        return sorted(ids)

    def pull(self, model, suffix, success, reference=False):
        repository = "registryca/" + suffix
        image = model.image(repository)
        manifest_artifact = suffix + "-oci-manifest.json"
        (self.root / manifest_artifact).write_bytes(image["manifest"])
        name = model.authority + "/" + repository + ":latest"
        certificate_sha256 = model.fingerprints[model.context]
        pull_status, raw = self.api("POST", "/images/create?fromImage=" + quote(name, safe=""),
                                    expected=None if reference and not success else 200, reference=reference)
        stream_artifact = suffix + "-pull.ndjson"
        (self.root / stream_artifact).write_bytes(raw)
        errors = pull_error_messages(pull_status, raw, reference, success)
        require(bool(errors) != success, f"{suffix}: unexpected pull outcome: {raw[-5000:]!r}")
        inspect_artifact = suffix + "-image-inspect.json"
        if success:
            status, data = self.api("GET", "/images/" + quote(name, safe="") + "/json", reference=reference)
            (self.root / inspect_artifact).write_bytes(data)
            inspected = json.loads(data)
            assert_pulled_image_identity(inspected, image, name, reference,
                                         self.args.architecture, self.args.token)
            require(model.seen(repository), "Successful pull never reached the TLS registry")
            self.api("DELETE", "/images/" + quote(name, safe="") + "?force=true", reference=reference)
        else:
            require(not model.seen(repository), "Rejected TLS pull reached the HTTP registry")
            require(any(word in " ".join(errors).lower() for word in ("certificate", "x509", "tls", "unknownissuer", "notvalidforname")),
                    f"Pull failure was not identified as TLS: {errors}")
            status, data = self.api("GET", "/images/" + quote(name, safe="") + "/json",
                                    expected=None, reference=reference)
            (self.root / inspect_artifact).write_bytes(data)
            require(status == 404, "Rejected pull published an image")
        requests = model.seen(repository)
        require(all(row["certificate_sha256"] == certificate_sha256 for row in requests),
                "Registry served a different certificate than the intended trust case")
        return {"authority": model.authority, "repository": repository, "success": success,
                "tag": name, "reference": reference, "expected_certificate_sha256": certificate_sha256,
                "manifest_digest": image["manifest_digest"], "config_digest": image["config_digest"],
                "diff_id": image["diff_id"], "errors": errors, "http_requests": requests,
                "stream_artifact": stream_artifact, "inspect_artifact": inspect_artifact,
                "manifest_artifact": manifest_artifact, "pull_status": pull_status, "inspect_status": status}

    def build(self, model, suffix, success=True, driver="default", installation_failure=False):
        repository = "registryca/" + suffix
        image = model.image(repository)
        certificate_sha256 = model.fingerprints[model.context]
        context = self.root / (suffix + "-context")
        context.mkdir()
        dockerfile = (f"FROM {model.authority}/{repository}:latest\n"
                      f"LABEL dev.asagiri.registry-ca-case={suffix}\n").encode()
        (context / "Dockerfile").write_bytes(dockerfile)
        if driver == "classic":
            tag = "registry-ca-" + self.args.token + ":" + suffix
            _, raw = self.api("POST", "/build?t=" + quote(tag, safe="") + "&pull=1&nocache=1",
                              tar_bytes("Dockerfile", dockerfile), raw=True)
            (self.root / (suffix + "-build.ndjson")).write_bytes(raw)
            errors = [row["error"] for row in (json.loads(line) for line in raw.splitlines() if line)
                      if row.get("error")]
            require(not errors and success, f"Classic build failed: {raw[-5000:]!r}")
            _, created = self.api("POST", "/containers/create", {"Image": tag, "Cmd": ["true"]}, expected=201)
            container = json.loads(created)["Id"]
            try:
                _, archive = self.api("GET", "/containers/" + container + "/archive?path=/registry-ca-proof")
                require(archive_file(archive, "registry-ca-proof") == image["proof"],
                        "Classic BuildKit image contains wrong registry content")
                exported = self.root / (suffix + "-proof.tar")
                exported.write_bytes(archive)
            finally:
                self.api("DELETE", "/containers/" + container + "?force=true", expected=204)
                self.api("DELETE", "/images/" + quote(tag, safe="") + "?force=true")
            code, stderr = 0, b""
        else:
            exported = self.root / (suffix + "-rootfs.tar")
            code, _, stderr = self.docker(
                suffix, "buildx", "build", "--builder", driver, "--pull", "--no-cache",
                "--progress", "plain", "--output", "type=tar,dest=" + str(exported), str(context))
            require((code == 0) == success, f"{suffix}: unexpected build exit {code}: {stderr[-8000:]!r}")
            if success:
                require(archive_file(exported.read_bytes(), "registry-ca-proof") == image["proof"],
                        "BuildKit export contains wrong registry content")
            else:
                # Buildx may create the destination before a failed solve.
                require(not exported.exists() or exported.stat().st_size == 0,
                        "Rejected build produced nonempty export content")
                words = ("not a directory", "notdir") if installation_failure else ("certificate", "x509", "tls")
                require(any(word in stderr.decode(errors="replace").lower() for word in words),
                        f"Build failed for an unexpected reason: {stderr[-8000:]!r}")
        requests = model.seen(repository)
        require(bool(requests) == success,
                "Build outcome disagrees with actual TLS registry HTTP requests")
        require(all(row["certificate_sha256"] == certificate_sha256 for row in requests),
                "Build registry traffic used the wrong leaf certificate")
        return {"authority": model.authority, "repository": repository, "driver": driver,
                "success": success, "exit": code, "proof_sha256": sha(image["proof"]),
                "expected_proof_utf8": image["proof"].decode(),
                "expected_certificate_sha256": certificate_sha256,
                "http_requests": requests, "stderr_sha256": sha(stderr),
                "stderr_artifact": None if driver == "classic" else suffix + ".stderr",
                "context_artifact": suffix + "-context/Dockerfile",
                "export_artifact": str(exported.relative_to(self.root)) if exported.exists() else None,
                "export_bytes": exported.stat().st_size if exported.exists() else 0,
                "export_sha256": file_sha(exported) if exported.exists() else None}

    def snapshot(self, suffix, bundles, absent=()):
        builder = self.builder()
        require(builder["State"]["Running"], "Snapshot requires an actual running BuildKit")
        container = builder["Id"]
        _, archive = self.api("GET", f"/containers/{container}/archive?path=/etc/buildkit/buildkitd.toml")
        text = archive_file(archive, "buildkitd.toml")
        (self.root / (suffix + "-buildkitd.toml")).write_bytes(text)
        parsed = tomllib.loads(text.decode())
        files, artifacts = {}, {}
        for authority, pem in bundles.items():
            path = "/etc/buildkit/registry-ca-" + sha(pem.encode()) + ".pem"
            table = parsed["registry"][authority]
            require(table.get("ca") == [path] and not table.get("insecure") and not table.get("http"),
                    "BuildKit CA table does not preserve exact-authority HTTPS verification")
            _, archived = self.api("GET", f"/containers/{container}/archive?path=" + quote(path, safe=""))
            data = archive_file(archived, Path(path).name)
            require(data == pem.encode(), "BuildKit CA bytes differ from admitted policy snapshot")
            files[path] = sha(data)
            artifact = suffix + "-" + Path(path).name
            (self.root / artifact).write_bytes(data)
            artifacts[path] = artifact
        for authority in absent:
            require(not parsed.get("registry", {}).get(authority, {}).get("ca"),
                    "Revoked authority still has a BuildKit CA setting")
        return {"container": container, "pid": builder["State"]["Pid"], "config_sha256": sha(text),
                "config_artifact": suffix + "-buildkitd.toml",
                "ca_files": files, "ca_artifacts": artifacts, "absent_authorities": list(absent)}

    def grammar(self, ca, contexts, baseline):
        good = self.models["good"]
        for case, malformed in (
            ("leading-space", " \t" + ca["a"]),
            ("inter-block-space", ca["b"] + " \t" + ca["a"]),
            ("concatenated", ca["b"].rstrip("\n") + ca["a"]),
        ):
            before = self.config()[1]
            disk_before = file_sha(self.config_file)
            builder_before = self.builder()
            require(before["registry-ca-certificates"] == baseline
                    and builder_before["State"]["Running"], "Wrong PEM control starting state")
            status, error = self.config(
                {"registry-ca-certificates": {**baseline, good.authority: malformed}}, expected=400)
            after, builder_after = self.config()[1], self.builder()
            disk_after = file_sha(self.config_file)
            require(before == after == self.saved_config() and disk_before == disk_after,
                    "Malformed PEM changed the active or persisted trust policy")
            require(builder_after["Id"] == builder_before["Id"]
                    and builder_after["State"]["Pid"] == builder_before["State"]["Pid"]
                    and builder_after["State"]["Running"], "Rejected PEM stopped or replaced the builder")
            row = {"case": case, "disposition": "rejected", "http_status": status,
                   "input_pem_sha256": sha(malformed.encode()), "error": error,
                   "persisted_before_sha256": disk_before, "persisted_after_sha256": disk_after,
                   "container": builder_before["Id"], "pid_before": builder_before["State"]["Pid"],
                   "pid_after": builder_after["State"]["Pid"], "builder_running": True}
            if case == "inter-block-space":
                # Valid CRLF/blank-line control: both roots must work through
                # actual clients. Closing fixture HTTP connections forces a new
                # TLS handshake after each leaf rotation.
                control = "\r\n" + (ca["b"] + "\n" + ca["a"]).replace("\n", "\r\n") + "\r\n"
                policy = {**baseline, good.authority: control}
                self.policy(policy)
                positive = []
                try:
                    for leaf in ("a", "b"):
                        good.context = contexts["good-" + leaf]
                        positive.append({"leaf": leaf,
                                         "engine": self.pull(good, "grammar-engine-" + leaf, True),
                                         "build": self.build(good, "grammar-builder-" + leaf)})
                    row["valid_control"] = {
                        "pem_sha256": sha(control.encode()), "variants": ["CRLF", "blank-lines", "two-roots"],
                        "clients": positive, "snapshot": self.snapshot("grammar-control", policy)}
                finally:
                    good.context = contexts["good-a"]
                    self.policy(baseline)
                # The next rejected case must also observe a live builder.
                row["baseline_restored"] = self.build(good, "grammar-restored")
            self.result(case + " PEM rejected before persistence", **row)

    def active(self, suffix, driver, current, rejected):
        model = self.models["good"]
        model.gate_repo = "registryca/" + suffix
        model.entered.clear()
        model.release.clear()
        model.gate_expired = False
        before = self.config()[1]
        disk_before = file_sha(self.config_file)
        require(before["registry-ca-certificates"] == current, "Wrong active-test starting trust")
        with ThreadPoolExecutor(max_workers=1) as pool:
            build = pool.submit(self.build, model, suffix, True, driver)
            try:
                require(model.entered.wait(90), "Actual build did not reach the bounded registry barrier")
                require(not build.done(), "Build finished before the active-policy test")
                status, error = self.config({"registry-ca-certificates": rejected}, expected=409)
                require("busy" in json.dumps(error).lower(), "Conflict did not identify active builder work")
                require(self.config()[1] == before and file_sha(self.config_file) == disk_before,
                        "Rejected active trust change was persisted or applied")
                same_status, same = self.config({"registry-ca-certificates": current})
                require(same == before and file_sha(self.config_file) == disk_before,
                        "Identical active policy changed its persisted snapshot")
            finally:
                model.release.set()
            built = build.result(timeout=300)
        model.gate_repo = None
        require(not model.gate_expired, "Registry barrier expired; active admission was not established")
        return {"driver": driver, "changed_status": status, "identical_status": same_status,
                "persisted_sha256": disk_before, "build": built, "barrier_expired": False}

    def certificates(self, directory):
        contexts, certificates, fingerprints = {}, {}, {}
        for name in ("a", "b"):
            key, cert = directory / (name + "-ca.key"), directory / (name + "-ca.pem")
            code, _, stderr = self.command("generate-ca-" + name, [
                "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                "-keyout", str(key), "-out", str(cert), "-days", "2", "-subj", "/CN=Asagiri private qualifier " + name,
                "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign",
            ], 30)
            require(code == 0, repr(stderr))
            key.chmod(0o600)
            certificates[name] = cert.read_text()
            (self.root / (name + "-public-ca.pem")).write_text(certificates[name])
        for name, ca, subject in (("good-a", "a", "IP:" + self.args.address), ("good-b", "b", "IP:" + self.args.address),
                                  ("wrong-name", "a", "DNS:" + WRONG_NAME)):
            key, csr, cert = (directory / (name + suffix) for suffix in (".key", ".csr", ".pem"))
            code, _, stderr = self.command("generate-leaf-" + name, [
                "openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes",
                "-keyout", str(key), "-out", str(csr), "-subj", "/CN=Asagiri qualifier " + name,
            ], 30)
            require(code == 0, repr(stderr))
            key.chmod(0o600)
            extensions = directory / (name + ".ext")
            extensions.write_text("basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\n"
                                  "extendedKeyUsage=serverAuth\nsubjectAltName=" + subject + "\n")
            code, _, stderr = self.command("sign-leaf-" + name, [
                "openssl", "x509", "-req", "-in", str(csr), "-CA", str(directory / (ca + "-ca.pem")),
                "-CAkey", str(directory / (ca + "-ca.key")), "-CAcreateserial", "-out", str(cert),
                "-days", "2", "-extfile", str(extensions),
            ], 30)
            require(code == 0, repr(stderr))
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.minimum_version = ssl.TLSVersion.TLSv1_2
            context.load_cert_chain(cert, key)
            contexts[name] = context
            fingerprints[context] = sha(ssl.PEM_cert_to_DER_cert(cert.read_text()))
        return certificates, contexts, fingerprints

    def start_reference(self, bundles):
        for authority, pem in bundles.items():
            directory = Path("/etc/docker/certs.d") / authority
            directory.mkdir(parents=True, exist_ok=False)
            (directory / "ca.crt").write_text(pem)
        # Keep overlay/image storage on the host's own anonymous Docker volume,
        # not an upper directory inside the outer container's overlay rootfs.
        root = Path("/var/lib/docker/registry-ca-reference")
        root.mkdir(parents=True, exist_ok=False)
        output = (self.root / "reference-daemon.log").open("wb")
        arguments = ["dockerd", "--host=unix://" + REFERENCE_SOCKET, "--data-root=" + str(root / "data"),
                     "--exec-root=" + str(root / "run"), "--pidfile=" + str(root / "daemon.pid"),
                     "--bridge=none", "--iptables=false", "--ip6tables=false", "--ip-forward=false",
                     "--ip-masq=false", "--userland-proxy=false"]
        self.reference_process = subprocess.Popen(arguments, stdout=output, stderr=subprocess.STDOUT,
                                                  start_new_session=True)
        output.close()
        self.event(kind="reference-daemon", argv=arguments, pid=self.reference_process.pid)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            require(self.reference_process.poll() is None, "Private reference Docker daemon exited")
            try:
                _, data = self.api("GET", "/version", reference=True)
                version = json.loads(data)
                self.record["reference_version"] = version
                require(version["Version"] == self.args.reference_version,
                        "Reference daemon differs from source-pinned Docker version")
                return
            except (ConnectionError, FileNotFoundError, socket.timeout):
                time.sleep(0.25)
        raise TimeoutError("Private reference Docker did not start")

    def failed_apply(self, current, desired, context):
        builder = self.builder()
        container = builder["Id"]
        require(builder["State"]["Running"] and builder["State"]["Pid"] > 0,
                "Fault test must follow a real running builder")
        self.record["fault_container"] = container
        self.record["fault_original_pid"] = builder["State"]["Pid"]
        # Save/replace the stopped rootfs. A running container has live built-in
        # hosts/resolv.conf bind mounts below /etc, which can make replacing the
        # directory fail with EBUSY instead of testing configuration installation.
        self.api("POST", f"/containers/{container}/stop?t=3", expected=204)
        require(not self.builder()["State"]["Running"], "Owned builder did not stop for fault injection")
        _, self.saved_etc = self.api("GET", f"/containers/{container}/archive?path=/etc")
        require(self.saved_etc, "No builder /etc archive to restore")
        self.record["fault_restore_archive_sha256"] = sha(self.saved_etc)
        require(self.config()[1]["registry-ca-certificates"] == current, "Unexpected fault-test policy")
        self.fault_installed = True
        self.api("PUT", f"/containers/{container}/archive?path=/",
                 tar_bytes("etc", b"owned-registry-ca-installation-fault\n"), raw=True)
        for index, check in enumerate(("failed apply persists desired policy", "identical failed apply remains pending")):
            status, data = self.api("POST", "/asagiri/daemon-config",
                                    {"registry-ca-certificates": desired}, expected=None)
            require(status >= 400 and any(word in data.decode(errors="replace").lower()
                                         for word in ("not a directory", "notdir")),
                    f"Expected real configuration installation failure: {status} {data!r}")
            _, report = self.config()
            require(report["registry-ca-certificates"] == desired and self.saved_config() == report,
                    "Failed apply did not retain the exact desired policy for retry")
            require(not self.builder()["State"]["Running"], "Failed-apply builder is still running")
            self.result(check, http_status=status, identical_retry=bool(index),
                        error=data.decode(errors="replace"), persisted_sha256=file_sha(self.config_file))
        status, data = self.api("POST", f"/containers/{container}/start", expected=None)
        require(status >= 400 and not self.builder()["State"]["Running"]
                and any(word in data.decode(errors="replace").lower() for word in ("not a directory", "notdir")),
                "Direct builder start admitted an uninstalled policy")
        self.result("direct start refuses failed installation", http_status=status,
                    error=data.decode(errors="replace"), builder_running=False)
        before_admission = self.container_ids()
        try:
            rejected = self.build(self.models["good"], "failed-apply-build", success=False, installation_failure=True)
        finally:
            after_admission = self.container_ids()
            self.record["native_admission_containers"] = {
                "before": before_admission, "after": after_admission}
            require(after_admission == before_admission,
                    "Failed native admission created or removed another container")
        require(not self.builder()["State"]["Running"], "Failed native admission started the builder")
        self.result("native build refuses failed installation", **rejected,
                    containers_before=before_admission, containers_after=after_admission,
                    builder_container=container, builder_running=False)
        self.api("PUT", f"/containers/{container}/archive?path=/", self.saved_etc, raw=True)
        self.fault_installed = False
        status, report = self.config({"registry-ca-certificates": desired})
        require(report["registry-ca-certificates"] == desired and self.saved_config() == report
                and not self.builder()["State"]["Running"], "Repaired identical apply started or changed policy")
        self.result("identical repaired apply awaits start", http_status=status,
                    builder_running=False, persisted_sha256=file_sha(self.config_file))
        self.models["good"].context = context
        self.result("BuildKit repaired export", **self.build(self.models["good"], "repaired"))
        snapshot = self.snapshot("repaired", desired)
        require(snapshot["container"] == container and snapshot["pid"] != builder["State"]["Pid"],
                "Repair did not start the same builder with a new process")
        self.result("BuildKit repaired CA snapshot", **snapshot)

    def run(self):
        original = None
        try:
            code, version, error = self.docker("buildx-version", "buildx", "version")
            require(code == 0 and version.startswith(b"github.com/docker/buildx "),
                    f"Cannot record actual Buildx version: {version!r} {error!r}")
            self.record["buildx_version"] = version.decode().strip()
            original = self.config()[1]
            # An identical default policy intentionally does not create a
            # daemon.json file. Seed it through a real reversible API update,
            # then restore defaults before the TLS cases begin.
            if not self.config_file.exists():
                count = 1 if original["max-concurrent-downloads"] != 1 else 2
                _, seeded = self.config({"max-concurrent-downloads": count})
                require(self.saved_config() == seeded, "Initial API update was not persisted")
                _, restored = self.config(original)
                require(restored == original == self.saved_config(), "Could not restore default settings")
            self.policy({})
            with tempfile.TemporaryDirectory(prefix="registry-ca-keys-") as directory:
                ca, context, fingerprints = self.certificates(Path(directory))
                for name in ("good", "wrong-ca", "wrong-name", "outside"):
                    self.models[name] = Registry(self.args.address, context["wrong-name" if name == "wrong-name" else "good-a"],
                                                 fingerprints, self.args.token, name)
                good, wrong_ca, wrong_name, outside = (self.models[name] for name in ("good", "wrong-ca", "wrong-name", "outside"))
                a_policy = {good.authority: ca["a"], wrong_ca.authority: ca["b"], wrong_name.authority: ca["a"]}
                b_policy = {**a_policy, good.authority: ca["b"]}
                probes = []
                for name, model in self.models.items():
                    client = ssl.create_default_context(cadata=ca["a"])
                    client.minimum_version = ssl.TLSVersion.TLSv1_2
                    expected_name = WRONG_NAME if name == "wrong-name" else self.args.address
                    with socket.create_connection(model.server.server_address, timeout=5) as raw:
                        with client.wrap_socket(raw, server_hostname=expected_name) as stream:
                            require(sha(stream.getpeercert(binary_form=True)) == fingerprints[model.context],
                                    "Fixture preflight observed an unexpected certificate")
                            probes.append({"authority": model.authority, "verified_name": expected_name,
                                           "certificate_sha256": sha(stream.getpeercert(binary_form=True)),
                                           "tls_version": stream.version()})
                self.result("TLS fixture verified independently", verified_tls_connections=probes,
                            ca_sha256={name: sha(pem.encode()) for name, pem in ca.items()})
                self.start_reference(a_policy)
                for name, model, suffix, success in (
                    ("reference Docker trusted pull", good, "reference-good", True),
                    ("reference Docker wrong CA", wrong_ca, "reference-wrong-ca", False),
                    ("reference Docker wrong name", wrong_name, "reference-wrong-name", False),
                    ("reference Docker unconfigured authority", outside, "reference-outside", False),
                ):
                    self.result(name, **self.pull(model, suffix, success, reference=True))
                self.result("engine rejects default roots", **self.pull(good, "engine-default", False))
                self.policy(a_policy)
                for name, model, suffix, success in (
                    ("engine trusted pull", good, "engine-good", True),
                    ("engine wrong CA", wrong_ca, "engine-wrong-ca", False),
                    ("engine wrong name", wrong_name, "engine-wrong-name", False),
                    ("engine unconfigured authority", outside, "engine-outside", False),
                ):
                    self.result(name, **self.pull(model, suffix, success))
                self.result("BuildKit trusted export", **self.build(good, "builder-good"))
                self.result("BuildKit initial CA snapshot", **self.snapshot("initial", a_policy))
                self.grammar(ca, context, a_policy)
                for name, model, suffix in (
                    ("BuildKit wrong CA", wrong_ca, "builder-wrong-ca"),
                    ("BuildKit wrong name", wrong_name, "builder-wrong-name"),
                    ("BuildKit unconfigured authority", outside, "builder-outside"),
                ):
                    self.result(name, **self.build(model, suffix, False))
                revoked = {host: pem for host, pem in a_policy.items() if host != good.authority}
                self.policy(revoked)
                self.result("engine revoked CA", **self.pull(good, "engine-revoked", False))
                self.result("BuildKit revoked CA", **self.build(good, "builder-revoked", False))
                self.result("BuildKit revoked snapshot", **self.snapshot("revoked", revoked, [good.authority]))
                good.context = context["good-b"]
                self.policy(b_policy)
                self.result("engine rotated CA", **self.pull(good, "engine-rotated", True))
                self.result("BuildKit rotated export", **self.build(good, "builder-rotated"))
                self.result("BuildKit rotated snapshot", **self.snapshot("rotated", b_policy))
                self.result("active Docker driver refuses changed trust",
                            **self.active("active-docker", "default", b_policy, a_policy))
                self.result("active classic build refuses changed trust",
                            **self.active("active-classic", "classic", b_policy, a_policy))
                before = self.builder()
                external = before["Name"].removeprefix("/buildx_buildkit_").removesuffix("0")
                require(re.fullmatch(r"[a-z0-9-]+", external), "Invalid managed Buildx driver name")
                code, _, error = self.docker("create-container-driver", "buildx", "create", "--name", external,
                                            "--driver", "docker-container", "unix://" + SOCKET, timeout=45)
                require(code == 0, repr(error))
                code, inspected, error = self.docker("inspect-container-driver", "buildx", "inspect", "--bootstrap", external)
                require(code == 0 and b"docker-container" in inspected, repr(error))
                require(self.builder()["Id"] == before["Id"], "Buildx created a different unmanaged backend")
                self.result("active container driver refuses changed trust",
                            **self.active("active-container", external, b_policy, a_policy))
                self.failed_apply(b_policy, a_policy, context["good-a"])
                require([row["name"] for row in self.record["checks"]] == CHECKS,
                        "Qualification did not complete every required check")
                self.record["status"] = "passed"
        except BaseException as error:
            self.record.update(status="failed", error=repr(error))
            raise
        finally:
            for model in self.models.values():
                model.release.set()
            if self.fault_installed and self.saved_etc:
                try:
                    self.api("PUT", "/containers/" + self.record["fault_container"] + "/archive?path=/",
                             self.saved_etc, raw=True)
                    self.fault_installed = False
                except Exception as error:
                    self.record["fault_restore_error"] = repr(error)
            self.record["fault_restored"] = not self.fault_installed
            self.record["original_config_restored"] = False
            if original is not None:
                try:
                    _, restored = self.config(original)
                    self.record["original_config_restored"] = restored == original == self.saved_config()
                except Exception as error:
                    self.record["config_restore_error"] = repr(error)
            if self.reference_process is not None:
                try:
                    if self.reference_process.poll() is None:
                        self.reference_process.terminate()
                        try:
                            self.reference_process.wait(timeout=20)
                        except subprocess.TimeoutExpired:
                            if self.reference_process.poll() is None:
                                os.killpg(self.reference_process.pid, signal.SIGKILL)
                            self.reference_process.wait(timeout=10)
                    self.record["reference_daemon_exit"] = self.reference_process.returncode
                except Exception as error:
                    self.record["reference_stop_error"] = repr(error)
            for name, model in self.models.items():
                try:
                    model.close()
                    write_json(self.root / (name + "-registry-requests.json"), model.requests)
                except Exception as error:
                    self.record.setdefault("fixture_stop_errors", {})[name] = repr(error)
            if self.record["status"] == "passed" and (
                not self.record["fault_restored"] or not self.record["original_config_restored"]
                or self.record.get("reference_daemon_exit") is None
                or self.record.get("fixture_stop_errors")
            ):
                self.record.update(status="failed", error="Private fixture cleanup/restoration was incomplete")
            self.events.close()
            self.record["finished_at"] = now()
            self.record["evidence"] = {str(path.relative_to(self.root)): file_sha(path)
                                      for path in self.root.rglob("*")
                                      if path.is_file() and path.name != "verification.json"}
            write_json(self.root / "verification.json", self.record)
        require(self.record["status"] == "passed", self.record.get("error", "Private qualification failed"))


def source_contract(args):
    require(re.fullmatch(r"[0-9a-f]{40}", args.source), "Full source commit required")
    repository, binary = args.repository.resolve(), args.bin.resolve()
    def git(*argv):
        return subprocess.check_output(["git", "-C", str(repository), *argv], timeout=30)
    tree = git("rev-parse", args.source + "^{tree}").decode().strip()
    config = git("show", args.source + ":crates/asagiri/src/daemon/registry_config.rs")
    require(b"registry-ca-certificates" in config, "Source does not implement registry CA configuration")
    probe = git("show", args.source + ":tests/registry-ca.py")
    require(sha(probe) == file_sha(__file__), "Qualifier differs from the exact source's registry CA probe")
    builder = git("show", args.source + ":crates/asagiri/src/daemon/builder.rs")
    pinned = re.search(rb'const PINNED: &str = "([^"]+)";', builder)
    require(pinned is not None, "Cannot bind the managed BuildKit image pin")
    references = json.loads(git("show", args.source + ":scripts/ci/reference-images.json"))
    host_image = references["docker"]
    version = re.fullmatch(r"docker:([0-9.]+)-dind@sha256:[0-9a-f]{64}", host_image)
    require(version is not None, "Expected a source-pinned Docker reference host image")
    require((binary / "SOURCE_COMMIT").read_text().strip() == args.source, "Binary source differs from requested source")
    sums = {}
    for line in (binary / "SHA256SUMS").read_text().splitlines():
        digest, name = line.split()
        require(re.fullmatch(r"[a-f0-9]{64}", digest) and name not in sums, "Invalid binary checksum manifest")
        sums[name] = digest
    require(set(sums) == {"asagiri", "asagiri-init"}, "Expected exactly engine and init checksums")
    architecture = None
    for name in sums:
        path = binary / name
        require(path.is_file() and file_sha(path) == sums[name], "Binary checksum mismatch: " + name)
        with path.open("rb") as stream:
            header = stream.read(64)
        require(header[:6] == b"\x7fELF\x02\x01", "Expected a 64-bit little-endian Linux binary")
        current = {62: "amd64", 183: "arm64"}.get(struct.unpack_from("<H", header, 18)[0])
        require(current is not None and architecture in (None, current), "Binary architectures differ")
        architecture = current
    return {"source": args.source, "source_tree": tree, "repository": str(repository),
            "binary_directory": str(binary), "binaries": sums, "architecture": architecture,
            "source_manifest_sha256": file_sha(binary / "SHA256SUMS"),
            "host_image": host_image, "expected_reference_version": version[1],
            "buildkit_image_pin": pinned[1].decode(), "builder_source_sha256": sha(builder),
            "registry_config_source_sha256": sha(config), "qualifier_source_sha256": sha(probe)}


def qualify(args):
    if not args.execute:
        print("Prepared qualifier. Add --execute only when the exact candidate binaries and private runtime slot are ready.")
        return
    contract = source_contract(args)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    require(shutil.disk_usage(output).free >= 3 * 1024**3, "At least 3 GiB free space required")
    token = uuid.uuid4().hex
    host, network = ("asagiri-registry-ca-" + token[:16] + suffix for suffix in ("-host", "-net"))
    record = {"schema": "asagiri.registry-ca-integration.v1", "status": "running",
              "started_at": now(), **contract, "qualifier_sha256": file_sha(__file__),
              "host": host, "network": network, "token": token, "volumes": [],
              "outer_ports_published": False, "user_docker_socket_mounted": False,
              "scope": "TLS OCI protocol fixture; not a deployed Distribution/authentication service",
              "required_checks": CHECKS}
    log = (output / "outer.log").open("w")
    requested_host = requested_network = False

    def save():
        write_json(output / "verification.json", record)

    def run(*command, check=True, timeout=120):
        result = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
        log.write("$ " + repr(command) + "\n" + result.stdout + result.stderr + "\n")
        log.flush()
        if check:
            require(result.returncode == 0, f"{command[:5]}: {result.stdout[-2000:]} {result.stderr[-4000:]}")
        return result

    def engine(*arguments, **kwargs):
        require(record.get("host_id") is not None, "Host ownership is not established")
        return run("docker", "exec", record["host_id"], *arguments, **kwargs)

    def inspect(kind, name):
        found = run("docker", kind, "inspect", name, check=False)
        if found.returncode:
            require("no such" in found.stderr.lower() or "not found" in found.stderr.lower(),
                    "Cannot establish resource absence: " + found.stderr)
            return None
        items = json.loads(found.stdout)
        require(len(items) == 1, "Ambiguous task resource")
        return items[0]

    def remember_host(metadata):
        labels = metadata["Config"]["Labels"]
        require(labels.get("asagiri.registry-ca") == token
                and labels.get("asagiri.registry-ca.source") == args.source,
                "Refusing a host without this run's token and source labels")
        identity = metadata["Id"]
        require(re.fullmatch(r"[0-9a-f]{64}", identity)
                and metadata["Name"] == "/" + host
                and record.get("host_id", identity) == identity,
                "Task host identity changed")
        image = metadata["Config"]["Image"]
        require(image.rsplit("@", 1)[-1] == contract["host_image"].rsplit("@", 1)[-1],
                "Task host does not use the source-pinned Docker image")
        record["host_id"] = identity
        # Every -v destination in this command is anonymous. Retain only names
        # observed on the validated immutable container, never a global listing.
        names = [mount["Name"] for mount in metadata["Mounts"] if mount["Type"] == "volume"]
        require(all(re.fullmatch(r"[0-9a-f]{64}", name) for name in names),
                "Unexpected non-anonymous volume on the task host")
        record["volumes"] = sorted(set(record["volumes"]) | set(names))
        path = output / ("host-cleanup-inspect.json" if (output / "host-inspect.json").exists()
                         else "host-inspect.json")
        write_json(path, metadata)
        return metadata

    def remember_network(metadata):
        labels = metadata.get("Labels", {})
        require(labels.get("asagiri.registry-ca") == token
                and labels.get("asagiri.registry-ca.source") == args.source,
                "Refusing a network without this run's token and source labels")
        identity = metadata["Id"]
        require(re.fullmatch(r"[0-9a-f]{64}", identity) and metadata["Name"] == network
                and record.get("network_id", identity) == identity, "Task network identity changed")
        record["network_id"] = identity
        write_json(output / "network-inspect.json", metadata)
        return metadata

    def copy_runtime():
        require(record.get("host_id") is not None, "Cannot collect from an unowned host")
        destination = output / "runtime"
        destination.mkdir(exist_ok=True)
        # Contents syntax is safe to retry after a partial copy; it never
        # creates a second nested registry-ca-evidence directory.
        run("docker", "cp", record["host_id"] + ":/registry-ca-evidence/.", str(destination))

    def cleanup_runtime():
        cleanup = {"host_removed": False, "volumes_removed": False, "network_removed": False,
                   "errors": []}

        def attempt(stage, function):
            try:
                return True, function()
            except BaseException as error:
                cleanup["errors"].append({"stage": stage, "error": repr(error)})
                return False, None

        def discover_host():
            metadata = inspect("container", record.get("host_id", host))
            return None if metadata is None else remember_host(metadata)

        def discover_network():
            metadata = inspect("network", record.get("network_id", network))
            return None if metadata is None else remember_network(metadata)

        def collect_logs():
            logs = run("docker", "logs", record["host_id"], check=False)
            (output / "engine.log").write_text(logs.stdout + logs.stderr)
            require(logs.returncode == 0, "Could not retain the private engine log")

        def cleanup_host():
            if not requested_host:
                cleanup["host_removed"] = True
                return
            observed, metadata = attempt("inspect-owned-host", discover_host)
            if observed and metadata is None:
                cleanup["host_removed"] = True
                return
            identity = record.get("host_id")
            if identity is None:
                return  # Discovery never established ownership: never delete by name.
            try:
                attempt("collect-engine-log", collect_logs)
                attempt("collect-runtime-evidence", copy_runtime)
            finally:
                # An earlier validated immutable ID remains ours even when a
                # later inspect/log/copy request fails. Collection must not
                # prevent this removal attempt.
                attempt("remove-owned-host", lambda: run("docker", "rm", "-f", "-v", identity))
                ok, absent = attempt("observe-host-removal", lambda: inspect("container", identity) is None)
                cleanup["host_removed"] = ok and absent

        def cleanup_volumes():
            missing = []
            for name in record["volumes"]:
                ok, metadata = attempt("inspect-owned-volume-" + name, lambda name=name: inspect("volume", name))
                if ok and metadata is not None and cleanup["host_removed"]:
                    # No force: a volume adopted by another container must
                    # remain in use, and the cleanup must report failure.
                    attempt("remove-owned-volume-" + name, lambda name=name: run("docker", "volume", "rm", name))
                    ok, metadata = attempt("observe-volume-removal-" + name,
                                           lambda name=name: inspect("volume", name))
                missing.append(ok and metadata is None)
            cleanup["volumes_removed"] = all(missing)

        def cleanup_network():
            if not requested_network:
                cleanup["network_removed"] = True
                return
            observed, metadata = attempt("inspect-owned-network", discover_network)
            if observed and metadata is None:
                cleanup["network_removed"] = True
                return
            identity = record.get("network_id")
            if identity is None:
                return
            try:
                attempt("remove-owned-network", lambda: run("docker", "network", "rm", identity))
            finally:
                ok, absent = attempt("observe-network-removal", lambda: inspect("network", identity) is None)
                cleanup["network_removed"] = ok and absent

        try:
            cleanup_host()
        finally:
            try:
                cleanup_volumes()
            finally:
                cleanup_network()
        if cleanup["errors"]:
            cleanup["error"] = "; ".join(row["stage"] + ": " + row["error"] for row in cleanup["errors"])
        return cleanup

    def stream_inner(arguments):
        path = output / "inner-console.log"
        with path.open("w") as stream:
            process = subprocess.Popen(arguments, stdout=stream, stderr=subprocess.STDOUT, text=True)
        deadline = time.monotonic() + 1800
        offset = 0
        try:
            while process.poll() is None:
                with path.open() as progress:
                    progress.seek(offset)
                    text = progress.read()
                    offset = progress.tell()
                if text:
                    print(text, end="", flush=True)
                require(time.monotonic() < deadline, "Private qualifier exceeded 30 minutes")
                time.sleep(1)
            with path.open() as progress:
                progress.seek(offset)
                print(progress.read(), end="", flush=True)
            return process.returncode
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=10)

    try:
        host_arch = run("docker", "info", "--format", "{{.Architecture}}").stdout.strip()
        require({"aarch64": "arm64", "arm64": "arm64", "x86_64": "amd64", "amd64": "amd64"}.get(host_arch)
                == contract["architecture"], "Docker host and candidate binaries must have the same architecture")
        requested_network = True
        created = run("docker", "network", "create", "--driver", "bridge",
                      "--label", "asagiri.registry-ca=" + token,
                      "--label", "asagiri.registry-ca.source=" + args.source, network)
        network_id = created.stdout.strip()
        require(re.fullmatch(r"[0-9a-f]{64}", network_id), "Network create did not return an immutable ID")
        metadata = inspect("network", network_id)
        require(metadata is not None and metadata["Id"] == network_id, "Created network is absent")
        remember_network(metadata)
        requested_host = True
        created = run("docker", "run", "-d", "--name", host, "--hostname", host, "--network", network_id,
                      "--label", "asagiri.registry-ca=" + token,
                      "--label", "asagiri.registry-ca.source=" + args.source,
                      "--privileged", "--cgroupns=private",
                      "--cpus=2", "--memory=4g", "--memory-swap=4g", "--tmpfs", "/run", "--tmpfs", "/tmp",
                      "-e", "ASAGIRI_SOCKET=" + SOCKET, "-e", "ASAGIRI_DATA_DIR=/registry-ca-store",
                      "-e", "ASAGIRI_TEST_ENGINE=1", "-e", "ASAGIRI_REGISTRY_CA_TOKEN=" + token,
                      "-e", "ASAGIRI_BUILDER_IDLE_SECS=0", "-e", "ASAGIRI_BUILD_CACHE_GB=1",
                      "-v", contract["binary_directory"] + ":/opt/asagiri:ro",
                      "-v", "/var/lib/asagiri", "-v", "/registry-ca-store", "-v", "/var/lib/docker",
                      "--entrypoint", "/bin/sh", contract["host_image"], "-ec",
                      "apk add -q --no-cache python3 openssl iproute2 nftables iptables && exec /opt/asagiri/asagiri daemon",
                      timeout=300)
        host_id = created.stdout.strip()
        require(re.fullmatch(r"[0-9a-f]{64}", host_id), "Host create did not return an immutable ID")
        metadata = inspect("container", host_id)
        require(metadata is not None and metadata["Id"] == host_id, "Created host is absent")
        remember_host(metadata)
        require(not metadata["HostConfig"]["PortBindings"]
                and metadata["HostConfig"]["CgroupnsMode"] == "private"
                and set(metadata["NetworkSettings"]["Networks"]) == {network}
                and metadata["NetworkSettings"]["Networks"][network]["NetworkID"] == network_id,
                "Private host isolation mismatch")
        binds = [mount for mount in metadata["Mounts"] if mount["Type"] == "bind"]
        require(len(binds) == 1 and binds[0]["Source"] == contract["binary_directory"]
                and binds[0]["Destination"] == "/opt/asagiri" and not binds[0]["RW"],
                "An unapproved bind mount or Docker socket was exposed")
        require(len(record["volumes"]) == 3 and
                {mount["Destination"] for mount in metadata["Mounts"] if mount["Type"] == "volume"}
                == {"/var/lib/asagiri", "/registry-ca-store", "/var/lib/docker"},
                "Unexpected storage mounts on the private host")
        address = metadata["NetworkSettings"]["Networks"][network]["IPAddress"]
        record["address"] = address
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if engine("docker", "--host", "unix://" + SOCKET, "version", check=False, timeout=10).returncode == 0:
                break
            time.sleep(0.5)
        else:
            raise TimeoutError("Disposable candidate engine did not start")
        run("docker", "cp", str(Path(__file__).resolve()), host_id + ":/registry-ca-qualify.py")
        code = stream_inner(["docker", "exec", host_id, "python3", "-u", "/registry-ca-qualify.py", "_inside",
                             "--source", args.source, "--token", token, "--address", address,
                             "--source-tree", contract["source_tree"], "--architecture", contract["architecture"],
                             "--qualifier-sha256", record["qualifier_sha256"],
                             "--reference-version", contract["expected_reference_version"],
                             "--evidence", "/registry-ca-evidence"])
        record["inside_exit"] = code
        copy_runtime()
        inner = json.loads((output / "runtime/verification.json").read_text())
        require(code == 0 and inner["schema"] == "asagiri.registry-ca-private.v1"
                and inner["status"] == "passed" and inner["source"] == args.source
                and inner["source_tree"] == contract["source_tree"]
                and inner["architecture"] == contract["architecture"]
                and inner["qualifier_sha256"] == record["qualifier_sha256"]
                and inner["expected_reference_version"] == contract["expected_reference_version"]
                and inner["reference_version"]["Version"] == contract["expected_reference_version"]
                and inner["token"] == token and inner["address"] == address
                and inner["binaries"] == contract["binaries"], "Private runtime qualification failed")
        require([row["name"] for row in inner["checks"]] == CHECKS
                and all(row["status"] == "passed" for row in inner["checks"]),
                "Private runtime did not prove every required check")
        require(inner["fault_restored"] is True and inner["original_config_restored"] is True
                and inner["reference_daemon_exit"] is not None,
                "Private fixture restoration or reference daemon cleanup failed")
        record["checks"] = inner["checks"]
        record["reference_version"] = inner["reference_version"]
        record["binary_hashes_after"] = {name: file_sha(args.bin / name) for name in contract["binaries"]}
        require(record["binary_hashes_after"] == contract["binaries"]
                and (args.bin / "SOURCE_COMMIT").read_text().strip() == args.source
                and file_sha(args.bin / "SHA256SUMS") == contract["source_manifest_sha256"],
                "Candidate binary directory changed during qualification")
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error))
        raise
    finally:
        cleanup = cleanup_runtime()
        record["cleanup"] = cleanup
        if cleanup["errors"] or not all(cleanup[key] for key in ("host_removed", "volumes_removed", "network_removed")):
            record.update(status="failed", error=record.get("error") or
                          "Successful qualification requires complete evidence and observed private resource cleanup")
        log.close()
        record["finished_at"] = now()
        record["evidence"] = {str(path.relative_to(output)): file_sha(path)
                              for path in output.rglob("*") if path.is_file() and path != output / "verification.json"}
        save()
    require(record["status"] == "passed", record.get("error", "Private qualification cleanup failed"))
    print("PASS registry CA integration; all owned runtime resources removed.", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="mode", required=True)
    outer = commands.add_parser("qualify")
    outer.add_argument("--repository", type=Path, required=True)
    outer.add_argument("--source", required=True)
    outer.add_argument("--bin", type=Path, required=True)
    outer.add_argument("--output", type=Path, required=True)
    outer.add_argument("--execute", action="store_true")
    inner = commands.add_parser("_inside")
    inner.add_argument("--source", required=True)
    inner.add_argument("--source-tree", required=True)
    inner.add_argument("--architecture", choices=("amd64", "arm64"), required=True)
    inner.add_argument("--qualifier-sha256", required=True)
    inner.add_argument("--token", required=True)
    inner.add_argument("--address", required=True)
    inner.add_argument("--reference-version", required=True)
    inner.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    if args.mode == "qualify":
        qualify(args)
    else:
        Qualifier(args).run()


if __name__ == "__main__":
    main()
