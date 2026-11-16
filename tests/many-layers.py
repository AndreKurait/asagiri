#!/usr/bin/env python3
"""Start and restart a 120-layer image on an explicit Docker-compatible socket.

The base image must already be local. No network, host binds or privileged
containers are used. Only the uniquely named fixture image/container is removed.
"""
import argparse
import hashlib
import http.client
import io
import json
import socket
import tarfile
import uuid
from pathlib import Path
from urllib.parse import quote


def check(path, base_image, total, record_path):
    def request(method, target, body=None, raw=False, accepted=(200,)):
        connection = http.client.HTTPConnection("localhost", timeout=180)
        connection.sock = socket.socket(socket.AF_UNIX)
        connection.sock.settimeout(180)
        connection.sock.connect(path)
        payload = body if isinstance(body, bytes) else json.dumps(body).encode() if body is not None else b""
        try:
            connection.request(method, target, body=payload, headers={
                "Content-Type": "application/x-tar" if isinstance(body, bytes) else "application/json",
                "Connection": "close",
            })
            response = connection.getresponse()
            data = response.read(64 * 1024 * 1024 + 1)
            assert len(data) <= 64 * 1024 * 1024, "fixture response exceeds 64 MiB"
            assert response.status in accepted, (method, target, response.status, data[:2048])
            return data if raw else json.loads(data) if data else None
        finally:
            connection.close()

    version = request("GET", "/version")
    request("GET", "/images/" + quote(base_image, safe="") + "/json")
    base = request("GET", "/images/" + quote(base_image, safe="") + "/get", raw=True)
    with tarfile.open(fileobj=io.BytesIO(base), mode="r:*") as archive:
        def member(name):
            item = archive.getmember(name)
            assert item.isfile() and item.size <= 32 * 1024 * 1024, (name, item.size)
            return archive.extractfile(item).read()
        manifests = json.loads(member("manifest.json"))
        assert len(manifests) == 1, "select a single local base image"
        manifest = manifests[0]
        config = json.loads(member(manifest["Config"]))
        parts = {name: member(name) for name in manifest["Layers"]}
    base_count = len(manifest["Layers"])
    assert 0 < base_count < total <= 256
    assert len(config["rootfs"]["diff_ids"]) == base_count
    token = uuid.uuid4().hex
    name = "asagiri-many-layers-" + token
    tag = name + ":test"
    proof = "asagiri-layer-proof-" + token
    added = total - base_count
    layers = list(manifest["Layers"])
    for number in range(added):
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode="w") as archive:
            if number == 0:
                directory = tarfile.TarInfo(proof)
                directory.type = tarfile.DIRTYPE
                directory.mode = 0o755
                archive.addfile(directory)
            for filename, contents in [
                (f"{proof}/layer-{number}", f"layer {number}\n".encode()),
                (f"{proof}/winner", str(number).encode()),
            ]:
                item = tarfile.TarInfo(filename)
                item.mode = 0o644
                item.size = len(contents)
                archive.addfile(item, io.BytesIO(contents))
        blob = stream.getvalue()
        filename = f"asagiri-extra-{number}/layer.tar"
        parts[filename] = blob
        layers.append(filename)
        config["rootfs"]["diff_ids"].append("sha256:" + hashlib.sha256(blob).hexdigest())
        config.setdefault("history", []).append({"created_by": "Asagiri many-layer compatibility fixture"})
    script = (
        f'test "$(cat /{proof}/winner)" = "{added - 1}"; '
        f'test "$(find /{proof} -name "layer-*" | wc -l)" -eq {added}; '
        f'i=0; while [ "$i" -lt {added} ]; do '
        f'test "$(cat /{proof}/layer-$i)" = "layer $i"; i=$((i + 1)); done; '
        'echo ASAGIRI_MANY_LAYERS_OK'
    )
    config["comment"] = token
    config["config"].update({"Entrypoint": None, "Cmd": ["sh", "-ec", script]})
    config_blob = json.dumps(config).encode()
    config_name = hashlib.sha256(config_blob).hexdigest() + ".json"
    parts[config_name] = config_blob
    parts["manifest.json"] = json.dumps([{"Config": config_name, "RepoTags": [tag], "Layers": layers}]).encode()
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w") as archive:
        for filename, contents in parts.items():
            item = tarfile.TarInfo(filename)
            item.mode = 0o644
            item.size = len(contents)
            archive.addfile(item, io.BytesIO(contents))
    result = {"version": version.get("Version"), "base": base_image, "layers": total, "added": added, "image": tag, "cycles": []}
    cid = None
    image_owned = False
    try:
        request("GET", "/images/" + tag + "/json", accepted=(404,))
        image_owned = True
        loaded = request("POST", "/images/load", stream.getvalue(), raw=True)
        assert not any(json.loads(line).get("error") for line in loaded.splitlines() if line.strip()), loaded[:2048]
        image = request("GET", "/images/" + tag + "/json")
        assert len(image["RootFS"]["Layers"]) == total
        created = request("POST", "/containers/create?name=" + name,
                          {"Image": tag, "HostConfig": {"NetworkMode": "none"}}, accepted=(201,))
        cid = created["Id"]
        result["container"] = cid
        for cycle in range(2):
            request("POST", "/containers/" + cid + "/start", accepted=(204,))
            status = request("POST", "/containers/" + cid + "/wait?condition=not-running")
            assert status["StatusCode"] == 0, status
            logs = request("GET", "/containers/" + cid + "/logs?stdout=1&stderr=1", raw=True)
            assert b"ASAGIRI_MANY_LAYERS_OK" in logs, logs
            target = quote(f"/{proof}/layer-{added - 1}", safe="")
            copied = request("GET", "/containers/" + cid + "/archive?path=" + target, raw=True)
            with tarfile.open(fileobj=io.BytesIO(copied), mode="r:*") as archive:
                files = [item for item in archive if item.isfile()]
                assert len(files) == 1
                assert archive.extractfile(files[0]).read() == f"layer {added - 1}\n".encode()
            result["cycles"].append({"cycle": cycle + 1, "exit_code": 0, "archive": "verified"})
        result["passed"] = True
        print(f"PASS {total}-layer image start, complete contents, precedence, stopped archive and restart", flush=True)
    except BaseException as error:
        result["error"] = str(error)
        raise
    finally:
        cleanup = []
        if cid:
            try:
                request("DELETE", "/containers/" + cid + "?force=true", accepted=(204, 404))
            except Exception as error:
                cleanup.append(str(error))
        if image_owned:
            try:
                request("DELETE", "/images/" + tag, accepted=(200, 404))
            except Exception as error:
                cleanup.append(str(error))
        result["cleanup_errors"] = cleanup
        if record_path:
            Path(record_path).write_text(json.dumps(result, indent=2) + "\n")
        assert not cleanup, cleanup


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("base", nargs="?", default="alpine:3.21")
    parser.add_argument("--layers", type=int, default=120)
    parser.add_argument("--record")
    args = parser.parse_args()
    check(args.socket, args.base, args.layers, args.record)
