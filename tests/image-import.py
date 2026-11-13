#!/usr/bin/env python3
"""Exercise docker import/load beside a daemon, as the private CI harness does.

--reference records Docker's leftover tags after a rejected import. Asagiri's
default test additionally requires that an invalid import publish no image.
"""
import gzip
import http.client
import http.server
import io
import json
from pathlib import Path
import re
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
from urllib.parse import urlencode
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=120)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, base, reference=False):
    prefix = "asagiri-import-" + uuid.uuid4().hex[:12]
    images, containers = [], []

    def cli(*args, data=None, ok=True):
        p = subprocess.run(["docker", "--host", "unix://" + path, *args],
                           input=data, capture_output=True, timeout=120)
        if ok:
            assert p.returncode == 0, (args, p.stderr.decode(errors="replace"))
        return p

    def api(method, target, body=None, chunked=False):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, body,
                               {"Content-Type": "application/x-tar"}, encode_chunked=chunked)
            response = connection.getresponse()
            if not reference and method == "POST" and target.split("?", 1)[0] in ("/images/create", "/images/load"):
                assert response.will_close, (target, response.getheaders())
                assert response.getheader("Connection", "").lower() == "close"
            return response.status, response.read()
        finally:
            connection.close()

    def inspect(name):
        return json.loads(cli("image", "inspect", name).stdout)[0]

    def created(p):
        found = re.findall(rb"sha256:[0-9a-f]{64}", p.stdout)
        assert found, (p.stdout, p.stderr)
        image_id = found[-1].decode()
        images.append(image_id)
        return image_id

    try:
        if reference:
            assert "Asagiri" not in cli("info", "--format", "{{.OperatingSystem}}").stdout.decode(), \
                "Asagiri must pass the full transactional import checks"
        cid = cli("create", base, "true").stdout.decode().strip()
        containers.append(cid)
        data = io.BytesIO(cli("export", cid).stdout)
        with tarfile.open(fileobj=data, mode="a") as tar:
            for name, content in [("import-fixture.bin", b"x" * (9 << 20)),
                                  ("import-message", b"asagiri-import-ok\n")]:
                item = tarfile.TarInfo(name)
                item.size, item.mode = len(content), 0o751
                tar.addfile(item, io.BytesIO(content))
            link = tarfile.TarInfo("import-link")
            link.type, link.linkname = tarfile.SYMTYPE, "import-message"
            tar.addfile(link)
        raw = data.getvalue()
        assert len(raw) > 8 << 20, "exercise an upload above the JSON body limit"
        tag = prefix + ":file"
        images.append(tag)
        with tempfile.TemporaryDirectory(prefix="asagiri-import-") as temp:
            archive = Path(temp) / "rootfs.tar"
            archive.write_bytes(raw)
            made = created(cli("import", "--message", "import regression",
                               "--change", 'CMD ["sh","-c","printf \\"$IMPORTED|\\"; cat /import-link; wc -c </import-fixture.bin"]',
                               "--change", "ENV IMPORTED=yes", "--change", "WORKDIR /tmp",
                               "--change", "LABEL import.test=" + prefix,
                               "--change", "EXPOSE 8080", str(archive), tag))
        info = inspect(tag)
        assert info["Id"] == made and info["Os"] == "linux", info
        assert "Healthcheck" not in info["Config"], info["Config"]
        assert info["Config"]["WorkingDir"] == "/tmp", info["Config"]
        assert "IMPORTED=yes" in info["Config"]["Env"], info["Config"]
        assert info["Config"]["Labels"]["import.test"] == prefix, info["Config"]
        assert "8080/tcp" in info["Config"]["ExposedPorts"], info["Config"]
        output = cli("run", "--rm", tag).stdout.decode()
        assert "yes|asagiri-import-ok" in output and str(9 << 20) in output, output
        status, history = api("GET", "/images/" + made + "/history")
        assert status == 200 and any(h.get("Comment") == "import regression" for h in json.loads(history))
        print("PASS large CLI file import preserves files, links, config and history", flush=True)

        zipped = gzip.compress(raw)
        untagged = created(cli("import", "--change", 'CMD ["cat","/import-link"]', "-", data=zipped))
        assert not inspect(untagged).get("RepoTags"), inspect(untagged)
        dangling = cli("images", "--filter", "dangling=true", "--no-trunc", "--quiet").stdout.decode().split()
        assert untagged in dangling, dangling
        assert cli("run", "--rm", untagged).stdout == b"asagiri-import-ok\n"
        print("PASS gzip stdin import creates a runnable untagged image", flush=True)

        query = urlencode([("fromSrc", "-"), ("repo", prefix), ("tag", "chunked"),
                           ("changes", "ENV WIRE=chunked"), ("changes", 'CMD ["cat","/import-link"]')])
        chunked_tag = prefix + ":chunked"
        images.append(chunked_tag)
        chunks = (raw[i:i + 65536] for i in range(0, len(raw), 65536))
        status, answer = api("POST", "/images/create?" + query, chunks, chunked=True)
        assert status == 200, (status, answer)
        assert all(not json.loads(line).get("error") for line in answer.splitlines() if line), answer
        assert "WIRE=chunked" in inspect(chunked_tag)["Config"]["Env"]
        assert cli("run", "--rm", chunked_tag).stdout == b"asagiri-import-ok\n"
        print("PASS chunked Engine API import streams beyond the JSON body limit", flush=True)

        class Source(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.send_header("Content-Length", str(len(zipped)))
                self.end_headers()
                self.wfile.write(zipped)

            def log_message(self, *_args):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Source)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            tag_url = prefix + ":url"
            images.append(tag_url)
            created(cli("import", "--change", 'CMD ["cat","/import-link"]',
                        f"http://127.0.0.1:{server.server_port}/rootfs.tar.gz", tag_url))
            assert cli("run", "--rm", tag_url).stdout == b"asagiri-import-ok\n"
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
        print("PASS HTTP source import", flush=True)

        saved = cli("save", tag).stdout
        cli("image", "rm", tag)
        cli("load", data=saved)
        assert inspect(tag)["Id"] == made
        assert "yes|asagiri-import-ok" in cli("run", "--rm", tag).stdout.decode()
        print("PASS imported image survives docker save/load", flush=True)

        for case, options, body in [("tar", (), b"not a tar" * 64),
                                    ("change", ("--change", "RUN echo forbidden"), raw),
                                    ("gzip", (), zipped[:len(zipped) // 2])]:
            broken = prefix + ":broken-" + case
            images.append(broken)
            p = cli("import", *options, "-", broken, data=body, ok=False)
            assert p.returncode != 0, case + ": a damaged archive or invalid instruction was accepted"
            published = cli("image", "inspect", broken, ok=False).returncode == 0
            if reference:
                print(f"REFERENCE rejected {case} import; image record remains: {published}", flush=True)
            else:
                assert not published, case + ": a rejected import published an image"
        print("PASS invalid input is rejected" if reference else "PASS invalid input publishes no image", flush=True)
    finally:
        for cid in containers:
            cli("rm", "-f", cid, ok=False)
        for name in dict.fromkeys(reversed(images)):
            cli("image", "rm", "-f", name, ok=False)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21", "--reference" in sys.argv[3:])
