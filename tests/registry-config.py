#!/usr/bin/env python3
"""Real registry policy and transfer tests, only in the private Linux CI engine."""
from concurrent.futures import ThreadPoolExecutor
import base64
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
import socket
import ssl
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
from urllib.parse import parse_qs, quote, urlsplit
import uuid


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=90)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


class Registry:
    def __init__(self, seed):
        self.lock = threading.Lock()
        self.layers, self.blobs = set(), {}
        descriptors, diff_ids = [], []
        for index in range(6):
            out = io.BytesIO()
            with tarfile.open(fileobj=out, mode="w") as archive:
                data = (seed + str(index)).encode() * 64
                entry = tarfile.TarInfo(f"registry-test/{seed}-{index}")
                entry.size = len(data)
                archive.addfile(entry, io.BytesIO(data))
            raw = out.getvalue()
            compressed = gzip.compress(raw, mtime=0)
            sha = digest(compressed)
            self.layers.add(sha)
            self.blobs[sha] = compressed
            diff_ids.append(digest(raw))
            descriptors.append({"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": sha, "size": len(compressed)})
        config = json.dumps({
            "architecture": {"x86_64": "amd64", "aarch64": "arm64"}[platform.machine()],
            "os": "linux", "config": {}, "rootfs": {"type": "layers", "diff_ids": diff_ids},
        }).encode()
        self.blobs[digest(config)] = config
        self.manifest = json.dumps({
            "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": digest(config), "size": len(config)},
            "layers": descriptors,
        }).encode()
        self.uploads = {}
        self.active = {"download": 0, "upload": 0}
        self.peak = dict(self.active)
        self.requests = []
        self.auth = None
        self.redirect = None
        self.upload_location = None
        self.manifests_published = 0

    def transfer(self, kind, delta):
        with self.lock:
            self.active[kind] += delta
            self.peak[kind] = max(self.peak[kind], self.active[kind])


def handler(model):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def send_error(self, code, message=None, explain=None):
            # A TLS ClientHello sent to a real plain-HTTP registry gets an
            # HTTP error response. Python otherwise defaults malformed
            # request lines to HTTP/0.9 and sends only HTML, which is not
            # the HTTP/1.x signal BuildKit uses for its HTTPS fallback.
            self.request_version = "HTTP/1.1"
            super().send_error(code, message, explain)

        def reply(self, code, body=b"", **headers):
            self.send_response(code)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Docker-Distribution-Api-Version", "registry/2.0")
            for key, value in headers.items():
                self.send_header(key.replace("_", "-"), value)
            self.end_headers()
            if self.command != "HEAD" and body:
                self.wfile.write(body)

        def request(self):
            with model.lock:
                model.requests.append((self.command, self.path, bool(self.headers.get("Authorization"))))
            if model.auth and self.headers.get("Authorization") != model.auth:
                self.reply(401, WWW_Authenticate='Basic realm="asagiri-registry-test"')
                return
            parsed = urlsplit(self.path)
            path = parsed.path
            if path == "/v2/":
                self.reply(200, b"{}")
            elif "/manifests/" in path:
                if self.command == "PUT":
                    body = self.rfile.read(int(self.headers["Content-Length"]))
                    manifest = json.loads(body)
                    repo = path.split("/manifests/")[0]
                    expected = [manifest["config"], *manifest["layers"]]
                    with model.lock:
                        assert all(blob["digest"] in model.uploads.get(repo, {}) for blob in expected)
                        model.manifests_published += 1
                    self.reply(201, Docker_Content_Digest=digest(body))
                elif model.redirect and "redirect" in path:
                    self.reply(307, Location=model.redirect + path)
                else:
                    self.reply(200, model.manifest, Content_Type="application/vnd.oci.image.manifest.v1+json",
                               Docker_Content_Digest=digest(model.manifest))
            elif "/blobs/uploads/" in path:
                if self.command == "POST":
                    self.reply(202, Location=model.upload_location or path + uuid.uuid4().hex)
                elif self.command == "PUT":
                    sha = parse_qs(parsed.query)["digest"][0]
                    model.transfer("upload", 1)
                    try:
                        time.sleep(0.15)
                        body = self.rfile.read(int(self.headers["Content-Length"]))
                        assert digest(body) == sha
                        repo = path.split("/blobs/uploads/")[0]
                        with model.lock:
                            model.uploads.setdefault(repo, {})[sha] = body
                    finally:
                        model.transfer("upload", -1)
                    self.reply(201, Docker_Content_Digest=sha)
                else:
                    self.reply(405)
            elif "/blobs/" in path:
                repo, sha = path.split("/blobs/", 1)
                if self.command == "HEAD":
                    self.reply(200 if sha in model.uploads.get(repo, {}) else 404)
                elif sha in model.blobs:
                    counted = sha in model.layers
                    if counted:
                        model.transfer("download", 1)
                    try:
                        if counted:
                            time.sleep(0.15)
                    finally:
                        if counted:
                            model.transfer("download", -1)
                    self.reply(200, model.blobs[sha], Docker_Content_Digest=sha)
                else:
                    self.reply(404)
            else:
                self.reply(404)

        do_GET = request
        do_HEAD = request
        do_POST = request
        do_PUT = request

    return Handler


class Server(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, model, context=None):
        self.context = context
        super().__init__(address, handler(model))

    def get_request(self):
        sock, address = super().get_request()
        sock.settimeout(5)
        if self.context:
            sock = self.context.wrap_socket(sock, server_side=True, do_handshake_on_connect=False)
        return sock, address

    def handle_error(self, _request, _address):
        # Expected failed TLS handshakes and the TLS probe of an HTTP endpoint.
        pass


def check(path, binary):
    assert sys.platform == "linux" and os.environ.get("ASAGIRI_TEST_ENGINE") == "1", "private Linux test engine only"
    address = socket.gethostbyname(socket.gethostname())
    assert ipaddress.ip_address(address).is_private and not ipaddress.ip_address(address).is_loopback, address
    token = uuid.uuid4().hex
    owned_images, servers = [], []
    credentials = {"username": "registry-test", "password": token}
    # docker-java includes explicit nulls for unused authentication fields.
    header_credentials = {**credentials, "auth": None, "identitytoken": None, "registrytoken": None}
    auth_header = base64.urlsafe_b64encode(json.dumps(header_credentials).encode()).decode()
    basic = "Basic " + base64.b64encode(("registry-test:" + token).encode()).decode()

    def api(method, target, body=None, auth=False, expected=200):
        connection = Connection(path)
        try:
            headers = {"Content-Type": "application/json"}
            if auth:
                headers["X-Registry-Auth"] = auth_header
            connection.request(method, target, None if body is None else json.dumps(body), headers)
            response = connection.getresponse()
            data = response.read()
            assert response.status == expected, (method, target, response.status, data[:2000])
            return data
        finally:
            connection.close()

    def config(patch=None):
        return json.loads(api("GET" if patch is None else "POST", "/asagiri/daemon-config", patch))

    def pull(host, repository, success=True, auth=False):
        name = host + "/" + repository + ":latest"
        raw = api("POST", "/images/create?fromImage=" + quote(name, safe=""), auth=auth)
        rows = [json.loads(line) for line in raw.splitlines() if line]
        errors = [row["error"] for row in rows if row.get("error")]
        assert bool(errors) != success, (name, rows)
        if success:
            owned_images.append(name)
            api("GET", "/images/" + quote(name, safe="") + "/json")
        return name, errors

    original = config()
    try:
        # Competing patches must preserve the other's field.
        with ThreadPoolExecutor(max_workers=2) as pool:
            list(pool.map(config, [{"max-concurrent-downloads": 2}, {"max-concurrent-uploads": 3}]))
        state = config()
        assert state["max-concurrent-downloads"] == 2 and state["max-concurrent-uploads"] == 3, state
        for bad in [{"max-concurrent-downloads": 0}, {"max-concurrent-uploads": 17},
                    {"insecure-registries": ["https://example.com"]}, {"registry-mirrors": ["http://example.com"]},
                    {"unknown-setting": True}]:
            api("POST", "/asagiri/daemon-config", bad, expected=400)
            assert config() == state

        with tempfile.TemporaryDirectory(prefix="asagiri-registry-") as directory:
            directory = Path(directory)
            subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                            "-keyout", str(directory / "key.pem"), "-out", str(directory / "cert.pem"),
                            "-days", "1", "-subj", "/CN=asagiri-registry-test",
                            "-addext", "subjectAltName=IP:" + address],
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(directory / "cert.pem", directory / "key.pem")

            def start(model, tls=False):
                server = Server((address, 0), model, context if tls else None)
                servers.append(server)
                threading.Thread(target=server.serve_forever, daemon=True).start()
                return f"{address}:{server.server_port}"

            models = [Registry(token + str(i)) for i in range(4)]
            http_model, tls_model, other_tls, parallel_model = models
            http_model.auth = basic
            hosts = [start(http_model), start(tls_model, True), start(other_tls, True), start(parallel_model)]
            plain, tls, outside, parallel = hosts

            config({"insecure-registries": [], "registry-mirrors": []})
            pull(plain, "default-http-denied-" + token, False, True)
            pull(tls, "default-self-signed-denied-" + token, False)
            config({"insecure-registries": [plain]})
            login = {**credentials, "serveraddress": plain}
            assert json.loads(api("POST", "/auth", login))["Status"] == "Login Succeeded"
            api("POST", "/auth", {**login, "password": "wrong"}, expected=401)
            api("POST", "/auth", {**login, "serveraddress": tls}, expected=400)
            pull(tls, "other-port-denied-" + token, False)

            # A one-worker pull really serializes the layer transfers.
            config({"max-concurrent-downloads": 1})
            image, _ = pull(plain, "serial-" + token, auth=True)
            assert http_model.peak["download"] == 1, http_model.peak
            assert any(auth for method, target, auth in http_model.requests if "/blobs/" in target)

            config({"insecure-registries": [plain, parallel], "max-concurrent-downloads": 3})
            pull(parallel, "parallel-" + token)
            assert 1 < parallel_model.peak["download"] <= 3, parallel_model.peak

            # Updating mirrors via the existing API keeps transport/limits.
            before = config()
            api("POST", "/asagiri/registry-mirrors", {"mirrors": ["http://" + plain]})
            mirrored = config()
            for key in ("insecure-registries", "max-concurrent-downloads", "max-concurrent-uploads"):
                assert mirrored[key] == before[key]
            api("POST", "/asagiri/registry-mirrors", {"mirrors": []})

            for limit in (1, 3):
                config({"max-concurrent-uploads": limit})
                target = plain + "/uploaded-" + token + "-" + str(limit)
                api("POST", "/images/" + quote(image, safe="") + "/tag?repo=" + quote(target, safe="") + "&tag=latest", expected=201)
                owned_images.append(target + ":latest")
                http_model.peak["upload"] = 0
                raw = api("POST", "/images/" + quote(target, safe="") + "/push?tag=latest", auth=True)
                assert not any(json.loads(line).get("error") for line in raw.splitlines() if line), raw[:2000]
                assert http_model.peak["upload"] == limit, (limit, http_model.peak)
            assert http_model.manifests_published == 2

            config({"insecure-registries": [plain, tls]})
            tls_login = {**credentials, "serveraddress": tls}
            tls_model.auth = basic
            try:
                assert json.loads(api("POST", "/auth", tls_login))["Status"] == "Login Succeeded"
            finally:
                tls_model.auth = None
            pull(tls, "self-signed-allowed-" + token)
            tls_model.redirect = "https://" + outside
            pull(tls, "redirect-" + token, False)
            assert not other_tls.requests, "TLS exception escaped to another authority"
            config({"insecure-registries": [plain]})
            api("POST", "/auth", tls_login, expected=400)
            pull(tls, "revoked-" + token, False)

            # Explicit literal-IP CIDRs cover the matching address.
            config({"insecure-registries": [address + "/32"]})
            pull(tls, "cidr-" + token)
            config({"insecure-registries": [plain]})
            http_model.upload_location = "https://" + outside + "/stolen-upload"
            target = plain + "/bad-location-" + token
            api("POST", "/images/" + quote(image, safe="") + "/tag?repo=" + quote(target, safe="") + "&tag=latest", expected=201)
            owned_images.append(target + ":latest")
            raw = api("POST", "/images/" + quote(target, safe="") + "/push?tag=latest", auth=True)
            assert any("same origin" in str(json.loads(line).get("error")) for line in raw.splitlines() if line), raw[:2000]
            assert not other_tls.requests
            http_model.upload_location = None
            print("PASS registry pulls/pushes: HTTP/TLS policy, credential boundaries, redirects/revocation, CIDRs and 1/3-worker limits", flush=True)

            # Keep TLS resolution independent of the local-image mirror:
            # a mirror's initial 404 can mask the registry's certificate
            # error in containerd's final diagnostic.
            for name in list(owned_images):
                if name.startswith(tls + "/"):
                    api("DELETE", "/images/" + quote(name, safe="") + "?force=true")
                    owned_images.remove(name)

            # BuildKit has its own resolver and trust configuration. Exercise
            # real FROM resolution through the Docker driver, including the
            # client's registry credentials and a later trust revocation.
            build_root = directory / "build"
            build_root.mkdir()
            docker_config = directory / "docker"
            docker_config.mkdir(mode=0o700)
            (docker_config / "config.json").write_text(json.dumps({
                "auths": {plain: {"auth": base64.b64encode(("registry-test:" + token).encode()).decode()}},
            }))
            env = {**os.environ, "DOCKER_CONFIG": str(docker_config), "DOCKER_HOST": "unix://" + path,
                   "BUILDX_BUILDER": "default", "DOCKER_BUILDKIT": "1"}
            env.pop("DOCKER_CONTEXT", None)
            env.pop("BUILDX_CONFIG", None)

            def build(host, suffix, success=True):
                tag = "registry-build-" + token + ":" + suffix
                (build_root / "Dockerfile").write_text(
                    f"FROM {host}/buildkit-{token}-{suffix}:latest\nLABEL asagiri.registry-test={suffix}\n")
                result = subprocess.run([
                    "docker", "--host", "unix://" + path, "--config", str(docker_config),
                    "buildx", "build", "--builder", "default", "--pull", "--progress", "plain",
                    "-t", tag, ".",
                ], cwd=build_root, env=env, text=True, capture_output=True, timeout=300)
                if result.returncode != 0 and success:
                    print(json.dumps({"build": suffix, "registryRequests": {
                        host: [row for row in model.requests if "/buildkit-" in row[1]][-20:]
                        for host, model in zip(hosts, models)
                    }}), flush=True)
                    import re
                    backend = json.loads(api("GET", "/asagiri/builder"))["container"]
                    for arguments in (["exec", backend, "cat", "/etc/buildkit/buildkitd.toml"],
                                      ["exec", backend, "wget", "-S", "-O", "-", "http://" + parallel + "/v2/"],
                                      ["logs", "--tail", "30", backend]):
                        diagnostic = subprocess.run(["docker", "--host", "unix://" + path, *arguments],
                                                    text=True, capture_output=True, timeout=20)
                        text = diagnostic.stdout + diagnostic.stderr
                        print(re.sub(r"/[a-f0-9]{32}/", "/<local-token>/", text)[-6000:], flush=True)
                if success:
                    assert result.returncode == 0, result.stdout[-2000:] + result.stderr[-12000:]
                    owned_images.append(tag)
                    built = json.loads(api("GET", "/images/" + quote(tag, safe="") + "/json"))
                    assert built["Config"]["Labels"]["asagiri.registry-test"] == suffix, built
                else:
                    assert result.returncode != 0 and "certificate" in result.stderr.lower(), result.stderr[-12000:]
                print("PASS BuildKit private registry " + suffix, flush=True)

            config({"insecure-registries": [plain, parallel, tls]})
            build(outside, "unlisted-self-signed", False)
            build(parallel, "http")
            build(plain, "http-auth")
            build(tls, "self-signed")
            config({"insecure-registries": [plain, parallel]})
            build(tls, "revoked-tls", False)

            cli = subprocess.run([binary, "config", "set", "-"], input=b'{"max-concurrent-downloads":2}',
                                 capture_output=True, check=True, timeout=10)
            assert json.loads(cli.stdout)["max-concurrent-downloads"] == 2
            assert config()["max-concurrent-downloads"] == 2
            saved = json.loads((Path.home() / ".asagiri" / "daemon.json").read_text())
            assert saved == config(), "persisted configuration differs from active settings"

        print("PASS daemon config: atomic patches, mirror compatibility, HTTP/TLS opt-in, scoped redirects/revocation, CIDRs, authenticated push, 1/3-worker limits, BuildKit HTTP/auth/TLS/revocation, CLI and persistence", flush=True)
    finally:
        # This never addresses resources outside this disposable engine.
        config(original)
        for name in reversed(owned_images):
            connection = Connection(path)
            try:
                connection.request("DELETE", "/images/" + quote(name, safe="") + "?force=true")
                response = connection.getresponse()
                response.read()
            finally:
                connection.close()
        for server in servers:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "/opt/asagiri/asagiri")
