#!/usr/bin/env python3
"""Image ID/tag removal and container-use conflicts on Docker and Asagiri."""
import argparse
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import socket
import tarfile
import time
from urllib.parse import quote
import uuid

LABEL = "dev.asagiri.image-remove"


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(args):
    if not args.reference_docker:
        assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
    nonce = uuid.uuid4().hex
    repository = "asagiri-image-remove-" + nonce
    record = {
        "schema": "asagiri.image-remove.v1", "status": "running", "owner_nonce": nonce,
        "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "reference_docker": args.reference_docker, "cases": [], "events": [], "cleanup": [],
    }
    images, containers = set(), set()
    before = None
    began = time.monotonic()

    def api(method, path, body=None, statuses=(200,), binary=False):
        connection = Connection(args.socket)
        headers = {}
        if body is not None and not isinstance(body, bytes):
            body = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        elif body is not None:
            headers["Content-Type"] = "application/x-tar"
        try:
            connection.request(method, "/v1.47" + path, body, headers)
            response = connection.getresponse()
            data = response.read(128 * 1024**2 + 1)
            assert len(data) <= 128 * 1024**2
            record["events"].append({
                "method": method, "path": path, "status": response.status,
                "response_sha256": hashlib.sha256(data).hexdigest(),
                "response": None if binary else data.decode(errors="replace"),
            })
            assert response.status in statuses, (method, path, response.status, data[:1000])
            return data if binary else json.loads(data) if data else None
        finally:
            connection.close()

    def image_path(name):
        return "/images/" + quote(name, safe="")

    def inventory():
        return sorted((row["Id"], sorted(row.get("RepoTags") or []), sorted(row.get("RepoDigests") or []))
                      for row in api("GET", "/images/json?all=1"))

    def own_image(image_id):
        info = api("GET", image_path(image_id) + "/json")
        assert info["Id"] == image_id and info["Config"]["Labels"].get(LABEL) == nonce
        return info

    def absent(*names):
        for name in names:
            api("GET", image_path(name) + "/json", statuses=(404,))

    def loaded(phase):
        tag = repository + ":" + phase
        config = json.loads(seed_config)
        config.setdefault("config", {}).setdefault("Labels", {})
        config["config"]["Labels"] = {**(config["config"]["Labels"] or {}), LABEL: nonce, LABEL + ".case": phase}
        raw = json.dumps(config, separators=(",", ":")).encode()
        image_id = "sha256:" + hashlib.sha256(raw).hexdigest()
        assert image_id not in {row[0] for row in before}
        config_name = image_id[7:] + ".json"
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as tar:
            for name, data in [(config_name, raw), *seed_layers,
                               ("manifest.json", json.dumps([{"Config": config_name, "RepoTags": [tag],
                                                              "Layers": [name for name, _ in seed_layers]}]).encode())]:
                entry = tarfile.TarInfo(name)
                entry.size, entry.mode = len(data), 0o644
                tar.addfile(entry, io.BytesIO(data))
        images.add(image_id)
        raw = api("POST", "/images/load", archive.getvalue(), binary=True)
        progress = [json.loads(line) for line in raw.splitlines() if line]
        assert progress and not any(row.get("error") for row in progress), progress
        own_image(image_id)
        assert api("GET", image_path(tag) + "/json")["Id"] == image_id
        return image_id, tag

    def tag(image_id, suffix):
        own_image(image_id)
        name = repository + ":" + suffix
        api("POST", image_path(image_id) + "/tag?repo=" + repository + "&tag=" + suffix, statuses=(201,))
        assert api("GET", image_path(name) + "/json")["Id"] == image_id
        return name

    def remove(name, force=False, status=200):
        return api("DELETE", image_path(name) + ("?force=true" if force else ""), statuses=(status,))

    def deleted(rows, image_id, tags):
        assert image_id in {row.get("Deleted") for row in rows}, rows
        assert set(tags) <= {row.get("Untagged") for row in rows}, rows
        absent(image_id, *tags)

    def case(name):
        record["cases"].append({"name": name, "passed": True})

    def create(image_id):
        own_image(image_id)
        body = {
            "Image": image_id,
            "Cmd": ["sh", "-c", "trap 'exit 0' TERM; while :; do sleep 0.02; done"],
            "Labels": {LABEL: nonce},
            "HostConfig": {"NetworkMode": "none", "RestartPolicy": {"Name": "no"}},
        }
        cid = api("POST", "/containers/create", body, statuses=(201,))["Id"]
        containers.add(cid)
        return cid

    def container(cid):
        value = api("GET", "/containers/" + cid + "/json")
        assert value["Config"]["Labels"].get(LABEL) == nonce and value["Image"] in images
        assert value["HostConfig"]["NetworkMode"] == "none" and not value["Mounts"]
        return value

    def start(cid):
        container(cid)
        api("POST", "/containers/" + cid + "/start", statuses=(204,))
        assert container(cid)["State"]["Running"]

    def stop(cid):
        container(cid)
        api("POST", "/containers/" + cid + "/stop?t=2", statuses=(204, 304))
        assert not container(cid)["State"]["Running"]

    try:
        record["engine_version"] = api("GET", "/version")
        assert bool(record["engine_version"].get("AsagiriVersion")) is not args.reference_docker
        before = inventory()
        record["inventory_before"] = before
        seed = api("GET", image_path(args.image) + "/get", binary=True)
        with tarfile.open(fileobj=io.BytesIO(seed)) as tar:
            rows = json.load(tar.extractfile("manifest.json"))
            assert len(rows) == 1
            seed_config = tar.extractfile(rows[0]["Config"]).read()
            seed_layers = [(name, tar.extractfile(name).read()) for name in rows[0]["Layers"]]

        image_id, first = loaded("multiple")
        second = tag(image_id, "second")
        snapshot = inventory()
        remove(image_id, status=409)
        assert inventory() == snapshot
        case("unforced-multiple-reference-ID-conflict")
        deleted(remove(image_id, force=True), image_id, [first, second])
        case("forced-ID-removes-all-tags")

        image_id, first = loaded("short")
        second = tag(image_id, "short-second")
        deleted(remove(image_id[7:19], force=True), image_id, [first, second])
        case("forced-short-ID-removes-all-tags")

        image_id, first = loaded("single")
        deleted(remove(image_id), image_id, [first])
        case("unforced-single-reference-ID-removes-image")

        image_id, first = loaded("untag")
        second = tag(image_id, "untag-second")
        rows = remove(first)
        assert {row.get("Untagged") for row in rows} == {first} and not any("Deleted" in row for row in rows)
        absent(first)
        assert api("GET", image_path(second) + "/json")["Id"] == image_id
        case("tag-removal-preserves-other-tag")
        deleted(remove(second), image_id, [second])

        image_id, first = loaded("in-use")
        cid = create(image_id)
        for name in (image_id, first):
            snapshot = inventory()
            remove(name, status=409)
            assert inventory() == snapshot
        case("created-container-unforced-ID-and-tag-conflicts")
        start(cid)
        for name, force in ((image_id, False), (image_id, True), (first, False)):
            snapshot = inventory()
            remove(name, force, 409)
            assert inventory() == snapshot and container(cid)["State"]["Running"]
        case("running-container-ID-cannot-be-forced")
        second = tag(image_id, "running-alias")
        rows = remove(second)
        assert second in {row.get("Untagged") for row in rows}
        assert not any("Deleted" in row for row in rows)
        case("running-container-allows-one-of-several-tags-to-be-removed")
        rows = remove(first, force=True)
        assert first in {row.get("Untagged") for row in rows}
        assert not any("Deleted" in row for row in rows)
        absent(first)
        assert not own_image(image_id).get("RepoTags") and container(cid)["State"]["Running"]
        case("forced-last-tag-keeps-running-image-dangling")
        remove(image_id, force=True, status=409)
        stop(cid)
        remove(image_id, status=409)
        deleted(remove(image_id, force=True), image_id, [])
        case("forced-ID-removes-stopped-container-image")
        start(cid)
        absent(image_id)
        stop(cid)
        case("container-restarts-after-forced-image-removal")
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error))
    finally:
        try:
            for cid in sorted(containers):
                if container(cid)["State"]["Running"]:
                    stop(cid)
                api("DELETE", "/containers/" + cid + "?v=1", statuses=(204,))
                api("GET", "/containers/" + cid + "/json", statuses=(404,))
                record["cleanup"].append({"container": cid, "absent": True})
            for row in api("GET", "/images/json?all=1"):
                if row["Id"] in images:
                    own_image(row["Id"])
                    assert all(name.startswith(repository + ":") for name in row.get("RepoTags") or [])
                    remove(row["Id"], force=True)
                    absent(row["Id"])
                    record["cleanup"].append({"image": row["Id"], "absent": True})
            assert before is not None and inventory() == before, "Original image inventory changed"
            record["original_image_inventory_unchanged"] = True
        except BaseException as error:
            record.update(status="failed", cleanup_error=repr(error))
        record["seconds"] = time.monotonic() - began
        print(json.dumps(record, indent=2), flush=True)
    assert record["status"] == "passed", "Inspect image removal evidence"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image")
    parser.add_argument("--reference-docker", action="store_true")
    check(parser.parse_args())
