#!/usr/bin/env python3
"""Inspect a loopback registry through the real Docker-compatible daemon API.

All daemon requests are reads. The registry is temporary; no image is pulled,
no container is started, and the same checks run against Docker and Asagiri.
"""
import base64
import hashlib
import http.client
import http.server
import json
import socket
import sys
import threading
import urllib.parse
import uuid


INDEX = "application/vnd.oci.image.index.v1+json"
MANIFEST = "application/vnd.docker.distribution.manifest.v2+json"
CONFIG = "application/vnd.docker.container.image.v1+json"


def encoded(value):
    return json.dumps(value, separators=(",", ":")).encode()


def digest(body):
    return "sha256:" + hashlib.sha256(body).hexdigest()


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=15)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(socket_path, reference_docker=False):
    def request(target, auth=None):
        connection = Connection(socket_path)
        headers = {} if auth is None else {"X-Registry-Auth": base64.urlsafe_b64encode(encoded(auth)).decode()}
        try:
            connection.request("GET", target, headers=headers)
            reply = connection.getresponse()
            return reply.status, json.loads(reply.read())
        finally:
            connection.close()

    status, version = request("/version")
    assert status == 200, (status, version)
    is_asagiri = version.get("Platform", {}).get("Name") == "Asagiri"
    assert is_asagiri != reference_docker, ("unexpected test engine", version)
    print("Engine:", json.dumps({key: version.get(key) for key in ("Version", "ApiVersion", "AsagiriVersion")}))
    repository = "distribution-test-" + uuid.uuid4().hex
    config = encoded({"architecture": "arm64", "os": "linux", "variant": "v8"})
    single = encoded({"schemaVersion": 2, "mediaType": MANIFEST,
                      "config": {"mediaType": CONFIG, "size": len(config), "digest": digest(config)},
                      "layers": []})
    platforms = [
        {"architecture": "amd64", "os": "linux"},
        {"architecture": "arm64", "os": "linux", "variant": "v8"},
        {"architecture": "amd64", "os": "windows", "os.version": "10.0.20348.1", "os.features": ["win32k"]},
        {"architecture": "unknown", "os": "unknown"},
    ]
    index = encoded({"schemaVersion": 2, "mediaType": INDEX, "manifests": [
        {"mediaType": MANIFEST, "size": len(single), "digest": digest(single), "platform": platform}
        for platform in platforms
    ]})
    state = {"body": index, "media": INDEX, "config": True, "auth": False, "missing": False, "requests": []}
    password = uuid.uuid4().hex
    basic = "Basic " + base64.b64encode(("fixture:" + password).encode()).decode()

    class Registry(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def do_HEAD(self):
            self.do_GET()

        def do_GET(self):
            path = urllib.parse.urlsplit(self.path).path
            state["requests"].append(path)
            code, body, media = 200, b"{}", "application/json"
            challenge = False
            if state["auth"] and self.headers.get("Authorization") != basic:
                code, challenge = 401, True
                body = encoded({"errors": [{"code": "UNAUTHORIZED", "message": "authentication required"}]})
            elif path in ("/v2", "/v2/"):
                pass
            elif path.startswith("/v2/" + repository + "/manifests/"):
                reference = path.rsplit("/", 1)[1]
                if state["missing"] or reference == "missing":
                    code = 404
                    body = encoded({"errors": [{"code": "MANIFEST_UNKNOWN", "message": "manifest unknown"}]})
                else:
                    body, media = state["body"], state["media"]
            elif path == "/v2/" + repository + "/blobs/" + digest(config) and state["config"]:
                body, media = config, CONFIG
            else:
                code = 404
                body = encoded({"errors": [{"code": "BLOB_UNKNOWN", "message": "blob unknown"}]})
            # Header bytes come only from this fixture's fixed media types,
            # even though the shared state also records incoming request paths.
            content_type = next(kind for kind in ("application/json", INDEX, MANIFEST, CONFIG) if kind == media)
            self.send_response(code)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Docker-Distribution-Api-Version", "registry/2.0")
            self.send_header("Connection", "close")
            if code == 200 and "/manifests/" in path:
                self.send_header("Docker-Content-Digest", digest(body))
            if challenge:
                self.send_header("WWW-Authenticate", 'Basic realm="distribution-test"')
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Registry)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    authority = "127.0.0.1:" + str(server.server_port)
    reference = authority + "/" + repository
    cases = 0

    def inspect(suffix=":latest", auth=None):
        # Encoded slashes and the registry port exercise the actual API router.
        return request("/v1.47/distribution/" + urllib.parse.quote(reference + suffix, safe="") + "/json", auth)

    def answer(body, media, expected_platforms, suffix=":latest", auth=None):
        status, got = inspect(suffix, auth)
        assert status == 200, (status, got)
        assert got["Descriptor"] == {"mediaType": media, "digest": digest(body), "size": len(body)}, got
        assert got["Platforms"] == expected_platforms, got

    try:
        answer(index, INDEX, platforms)
        cases += 1
        print("PASS all index platforms, including attestations and Windows metadata")

        state.update(body=single, media=MANIFEST)
        answer(single, MANIFEST, [{"architecture": "arm64", "os": "linux", "variant": "v8"}])
        cases += 1
        print("PASS mutable tag observes a new digest and reads the single-manifest config")

        state["config"] = False
        answer(single, MANIFEST, None)
        cases += 1
        print("PASS a missing config preserves the descriptor and reports unknown platforms")

        state.update(body=index, media=INDEX)
        answer(index, INDEX, platforms, "@" + digest(index))
        state["missing"] = True
        status, got = inspect("@" + digest(index))
        assert status == 404, (status, got)
        cases += 1
        print("PASS a deleted digest is not served from a manifest cache")

        state.update(missing=False, auth=True)
        status, got = inspect("@" + digest(index))
        if reference_docker:
            # Docker 28.0.4 wraps a Basic challenge without credentials in
            # an HTTP 500 response. Require its observed diagnostic instead
            # of accepting any server error as an authentication failure.
            assert status == 500 and "no basic auth credentials" in got.get("message", ""), (status, got)
        else:
            assert status == 401, (status, got)
        cases += 1
        print("PASS revoked access is rejected:", json.dumps({"status": status, "body": got}))

        auth = {"username": "fixture", "password": password, "serveraddress": authority}
        answer(index, INDEX, platforms, auth=auth)
        cases += 1
        print("PASS X-Registry-Auth reaches the registry")

        state["auth"] = False
        status, got = inspect(":missing")
        assert status == 404, (status, got)
        cases += 1
        print("PASS an absent tag returns 404")

        if is_asagiri:
            # Docker 28's inspection route trusts the manifest GET response.
            # Asagiri additionally verifies the requested content digest.
            status, got = inspect("@sha256:" + "a" * 64)
            assert status >= 400, (status, got)
            cases += 1
            print("PASS an incorrect content digest is rejected")

        status, got = request("/v1.47/distribution/" + urllib.parse.quote("invalid reference", safe="") + "/json")
        assert status == 400, (status, got)
        cases += 1
        print("PASS an invalid reference returns 400")

        status, images = request("/v1.47/images/json?all=1")
        assert status == 200, (status, images)
        assert all(not any(repository in tag for tag in (image.get("RepoTags") or [])) for image in images), images
        assert not any("/blobs/" in path and not path.endswith(digest(config)) for path in state["requests"])
        cases += 1
        print("PASS inspection installs no image and downloads no layers")
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
        assert not thread.is_alive()
    print(cases, "registry distribution cases passed")


if __name__ == "__main__":
    check(sys.argv[1], reference_docker="--reference-docker" in sys.argv[2:])
