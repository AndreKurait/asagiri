#!/usr/bin/env python3
"""Check OCI hard links through image load, container create and archive read."""
import argparse
from datetime import datetime, timezone
import hashlib
import http.client
import io
import json
from pathlib import Path
import socket
import tarfile
from urllib.parse import urlencode
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def fixture(tag, architecture, source, destination, target, pax=False, replace=False):
    content = ("hard-link fixture " + tag + "\n").encode()
    layers = []
    if replace:
        # Replacement is across layers. A single OCI layer must not contain
        # duplicate paths; Docker's containerd image store rejects that input.
        previous = io.BytesIO()
        with tarfile.open(fileobj=previous, mode="w") as archive:
            node = tarfile.TarInfo("bin")
            node.type, node.mode = tarfile.DIRTYPE, 0o755
            archive.addfile(node)
            node = tarfile.TarInfo(destination)
            node.size, node.mode = 3, 0o600
            archive.addfile(node, io.BytesIO(b"old"))
        layers.append(previous.getvalue())
    layer = io.BytesIO()
    with tarfile.open(fileobj=layer, mode="w", format=tarfile.PAX_FORMAT if pax else tarfile.GNU_FORMAT) as archive:
        node = tarfile.TarInfo("bin")
        node.type, node.mode = tarfile.DIRTYPE, 0o755
        archive.addfile(node)
        node = tarfile.TarInfo(source)
        node.size, node.mode, node.uid, node.gid = len(content), 0o640, 1001, 1002
        archive.addfile(node, io.BytesIO(content))
        node = tarfile.TarInfo("ignored-path" if pax else destination)
        # Deliberately different from the source entry. Docker applies these
        # attributes to the shared inode, so both paths must report them.
        node.type, node.linkname, node.mode = tarfile.LNKTYPE, "ignored-target" if pax else target, 0o7777
        if pax:
            node.pax_headers = {"path": destination, "linkpath": target}
        archive.addfile(node)
    layers.append(layer.getvalue())
    digests = [hashlib.sha256(data).hexdigest() for data in layers]
    config = json.dumps({
        "architecture": architecture, "os": "linux", "config": {"Cmd": ["/unused"]},
        "rootfs": {"type": "layers", "diff_ids": ["sha256:" + digest for digest in digests]},
    }).encode()
    config_digest = hashlib.sha256(config).hexdigest()
    saved = io.BytesIO()
    with tarfile.open(fileobj=saved, mode="w") as archive:
        manifest = [{"Config": config_digest + ".json", "RepoTags": [tag],
                     "Layers": [digest + "/layer.tar" for digest in digests]}]
        blobs = [(digest + "/layer.tar", data) for digest, data in zip(digests, layers)]
        for name, data in blobs + [(config_digest + ".json", config),
                                  ("manifest.json", json.dumps(manifest).encode())]:
            node = tarfile.TarInfo(name)
            node.size = len(data)
            archive.addfile(node, io.BytesIO(data))
    return saved.getvalue(), content


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("--reference-docker", action="store_true")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    record = {"schema": "asagiri.layer-hardlinks.v1", "status": "running",
              "started_at": datetime.now(timezone.utc).isoformat(),
              "reference_docker": args.reference_docker, "cases": [], "cleanup": []}
    containers, images = [], []

    def request(method, path, data=None, expected=200):
        connection = Connection(args.socket)
        try:
            content_type = "application/x-tar" if isinstance(data, bytes) else "application/json"
            if isinstance(data, dict):
                data = json.dumps(data).encode()
            connection.request(method, "/v1.47" + path, data, {"Content-Type": content_type})
            response = connection.getresponse()
            body = response.read(4 * 1024**2 + 1)
            assert len(body) <= 4 * 1024**2, "unexpectedly large fixture response"
            if method == "POST" and path.startswith("/containers/create") and response.status == 201:
                containers.append(json.loads(body)["Id"])
            assert response.status == expected, (method, path, response.status, body[:1000])
            return body
        finally:
            connection.close()

    try:
        version = json.loads(request("GET", "/version"))
        record["engine"] = version
        asagiri = version.get("Platform", {}).get("Name") == "Asagiri"
        assert asagiri is not args.reference_docker, "the requested engine identity does not match"
        architecture = {"aarch64": "arm64", "arm64": "arm64", "x86_64": "amd64",
                        "amd64": "amd64"}[version["Arch"]]
        prefix = "asagiri-hardlink-" + uuid.uuid4().hex
        long_name = "bin/" + "long-name-" * 20
        cases = [
            ("relative", "bin/tool", "bin/copy", "bin/tool", False, False),
            ("absolute", "bin/tool", "bin/copy", "/bin/tool", False, False),
            ("rooted-destination", "bin/tool", "/bin/copy", "/bin/tool", False, False),
            ("leading-slashes", "bin/tool", "bin/copy", "///bin/tool", False, False),
            ("contained-parent", "bin/tool", "bin/copy", "/bin/../bin/tool", False, False),
            ("gnu-long", long_name, "links/" + "long-copy-" * 20, "/" + long_name, False, False),
            ("pax-override", long_name, "links/pax-copy", "/" + long_name, True, False),
            ("replace-destination", "bin/tool", "bin/copy", "/bin/tool", False, True),
        ]
        for name, source, destination, target, pax, replace in cases:
            tag = prefix + ":" + name
            images.append(tag)
            saved, content = fixture(tag, architecture, source, destination, target, pax, replace)
            result = {"name": name, "fixture_sha256": hashlib.sha256(saved).hexdigest(), "passed": False, "files": []}
            record["cases"].append(result)
            messages = request("POST", "/images/load", saved)
            for line in messages.splitlines():
                message = json.loads(line)
                assert not message.get("error") and not message.get("errorDetail"), message
            created = json.loads(request("POST", "/containers/create?" + urlencode({"name": prefix + "-" + name}), {
                "Image": tag, "Cmd": ["/unused"], "Labels": {"io.asagiri.test": "layer-hardlinks"},
                "HostConfig": {"NetworkMode": "none"},
            }, 201))
            cid = created["Id"]
            for path in (source, destination):
                payload = request("GET", "/containers/" + cid + "/archive?" + urlencode({"path": "/" + path.lstrip("/")}))
                with tarfile.open(fileobj=io.BytesIO(payload)) as archive:
                    files = [member for member in archive.getmembers() if member.isfile()]
                    assert len(files) == 1, (name, path, [member.name for member in files])
                    member = files[0]
                    assert archive.extractfile(member).read() == content, (name, path, "file contents")
                    result["files"].append({"path": path, "uid": member.uid, "gid": member.gid, "mode": member.mode & 0o7777})
                    assert (member.uid, member.gid, member.mode & 0o7777) == (0, 0, 0o7777), (
                        name, path, member.uid, member.gid, member.mode)
            result["passed"] = True
            print("PASS", name, "loaded hard link has source contents and link-header metadata", flush=True)
            request("DELETE", "/containers/" + cid + "?force=1", expected=204)
            containers.remove(cid)
            request("DELETE", "/images/" + tag)
            images.remove(tag)
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error))
        raise
    finally:
        for kind, identifiers in (("container", containers), ("image", images)):
            for identifier in identifiers:
                try:
                    path = "/containers/" + identifier + "?force=1" if kind == "container" else "/images/" + identifier
                    request("DELETE", path, expected=204 if kind == "container" else 200)
                    record["cleanup"].append({"kind": kind, "id": identifier, "passed": True})
                except Exception as error:
                    record["cleanup"].append({"kind": kind, "id": identifier, "passed": False, "error": repr(error)})
                    record["status"] = "failed"
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(record, indent=2) + "\n")
        print(json.dumps(record), flush=True)
    assert record["status"] == "passed", record
    print("PASS", len(record["cases"]), "layer hard-link Docker API cases", flush=True)


if __name__ == "__main__":
    main()
