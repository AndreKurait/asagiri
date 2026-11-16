#!/usr/bin/env python3
"""Distribution inspection reads authenticated remote metadata without pulling images. Run in a disposable engine namespace.

Usage: distribution-inspect.py SOCKET PRIVATE_ADDRESS CERT KEY CA OUTPUT
The caller supplies a certificate for PRIVATE_ADDRESS and isolated engine/store.
This fixture publishes no host port and uses synthetic credentials/content only.
"""
import base64
import gzip
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
from pathlib import Path
import socket
import ssl
import sys
import tarfile
import threading
from urllib.parse import quote, urlsplit


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def tar(name, data):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w") as archive:
        info = tarfile.TarInfo(name)
        info.mode, info.size = 0o644, len(data)
        archive.addfile(info, io.BytesIO(data))
    return stream.getvalue()


def main():
    sock, address, cert, key, ca, output = sys.argv[1:]
    output = Path(output)
    output.mkdir(exist_ok=False)
    events = []
    auth = "Basic " + base64.b64encode(b"fixture:fixture-only").decode()
    layer = tar("parent-proof", b"classic-build-parent\n")
    compressed = gzip.compress(layer, mtime=0)
    config = json.dumps({"architecture": "amd64", "os": "linux", "config": {},
                         "rootfs": {"type": "layers", "diff_ids": [digest(layer)]}}).encode()
    manifest = json.dumps({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": digest(config), "size": len(config)},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": digest(compressed), "size": len(compressed)}]}).encode()
    index = json.dumps({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": digest(manifest),
                       "size": len(manifest), "platform": {"os": "linux", "architecture": "amd64"}}]}).encode()
    objects = {digest(blob): blob for blob in (config, compressed, manifest, index)}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def request(self):
            path = urlsplit(self.path).path
            authorized = self.headers.get("Authorization") == auth
            events.append({"method": self.command, "path": path, "authorized": authorized})
            if not authorized:
                self.send_response(401)
                self.send_header("WWW-Authenticate", 'Basic realm="fixture"')
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            body, media = b"{}", "application/json"
            if path != "/v2/":
                ref = path.rsplit("/", 1)[-1]
                body = index if ref == "latest" else objects.get(ref)
                if "/tampered/" in path:
                    body = manifest
                if "/no-config/" in path and "/blobs/" in path:
                    body = None
                if body is None:
                    self.send_error(404)
                    return
                if body in (index, manifest):
                    media = json.loads(body)["mediaType"]
                else:
                    media = "application/octet-stream"
            self.send_response(200)
            self.send_header("Content-Type", media)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Docker-Content-Digest", digest(body))
            self.send_header("Docker-Distribution-Api-Version", "registry/2.0")
            self.end_headers()
            if self.command == "GET":
                self.wfile.write(body)

        do_GET = request
        do_HEAD = request

    server = ThreadingHTTPServer((address, 0), Handler)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    authority = address + ":" + str(server.server_port)
    creds = base64.b64encode(json.dumps({authority: {"username": "fixture", "password": "fixture-only"}}).encode()).decode()

    def api(method, path, body=None, headers=None):
        connection = http.client.HTTPConnection("localhost", timeout=300)
        connection.sock = socket.socket(socket.AF_UNIX)
        connection.sock.settimeout(300)
        connection.sock.connect(sock)
        connection.request(method, path, body, headers or {})
        response = connection.getresponse()
        data = response.read()
        connection.close()
        return response.status, data

    def inspect(name, expected=200):
        status, body = api("GET", "/images/" + quote(name, safe="") + "/json")
        assert status == expected, (name, status, body)
        return json.loads(body)

    def build(name, dockerfile, success=True):
        status, body = api("POST", "/build?t=" + quote(name, safe=""), tar("Dockerfile", dockerfile.encode()),
                           {"Content-Type": "application/x-tar", "X-Registry-Config": creds})
        (output / (name.replace(":", "-") + ".ndjson")).write_bytes(body)
        errors = [row["error"] for row in map(json.loads, body.splitlines()) if row.get("error")]
        assert status == 200 and bool(errors) != success, (status, errors, body[-2000:])
        if success:
            inspect(name)

    try:
        policy = json.dumps({"registry-ca-certificates": {authority: Path(ca).read_text()}})
        status, body = api("POST", "/asagiri/daemon-config", policy, {"Content-Type": "application/json"})
        assert status == 200, (status, body)
        headers = {"X-Registry-Auth": base64.urlsafe_b64encode(json.dumps({"username": "fixture", "password": "fixture-only", "serveraddress": authority}).encode()).decode()}
        repository = authority + "/metadata/image"
        def distribution(reference, expected=200, selected_headers=headers):
            status, payload = api("GET", "/v1.47/distribution/" + quote(reference, safe="") + "/json", headers=selected_headers)
            assert status == expected, (reference, status, payload)
            return json.loads(payload)
        for reference, content in [(repository + "@" + digest(index), index), (repository + ":latest", index), (repository + "@" + digest(manifest), manifest)]:
            value = distribution(reference)
            assert value["Descriptor"] == {"digest": digest(content), "size": len(content), "mediaType": json.loads(content)["mediaType"]}, value
            assert value["Platforms"] == [{"os": "linux", "architecture": "amd64"}], value
            inspect(reference, 404)
        before = len(events)
        distribution(repository + "@" + digest(index))
        assert any(row["path"].endswith("/manifests/" + digest(index)) for row in events[before:]), "inspection must recheck registry"
        distribution(repository + ":latest", 401, {})
        wrong = {"X-Registry-Auth": base64.urlsafe_b64encode(json.dumps({"username": "fixture", "password": "wrong", "serveraddress": authority}).encode()).decode()}
        distribution(repository + ":latest", 401, wrong)
        distribution(repository + ":absent", 404)
        unavailable = distribution(authority + "/no-config/image@" + digest(manifest))
        assert unavailable["Descriptor"]["digest"] == digest(manifest) and unavailable["Platforms"] == [], unavailable
        distribution(authority + "/tampered/image@" + digest(index), 400)
        assert not any(row["path"].endswith("/blobs/" + digest(compressed)) for row in events), "metadata inspection fetched a layer"
        status, payload = api("GET", "/images/json")
        assert status == 200 and json.loads(payload) == [], payload
        (output / "result.json").write_text(json.dumps({"passed": True, "index": digest(index), "manifest": digest(manifest), "config": digest(config),
            "checks": ["index", "tag", "single-manifest-config", "fresh-remote-lookup", "anonymous-denied", "wrong-credential-denied", "missing-manifest", "unavailable-config-keeps-descriptor", "digest-tamper", "no-local-image-or-layer-fetch"]}, indent=2))
    finally:
        server.shutdown()
        server.server_close()
        (output / "registry-requests.json").write_text(json.dumps(events, indent=2))


if __name__ == "__main__":
    main()
