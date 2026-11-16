#!/usr/bin/env python3
"""Pull owned registry digest references and check Docker-compatible saved images."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import runpy
import sys
import tarfile
import threading
import urllib.parse
import uuid

helpers = runpy.run_path(str(Path(__file__).with_name("registry-config.py")))
Connection, Registry, Server, digest = (helpers[name] for name in ("Connection", "Registry", "Server", "digest"))
LABEL = "dev.asagiri.image-digest-export"


def check(args):
    if not args.reference_docker:
        assert sys.platform == "linux" and os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
        assert os.environ.get("ASAGIRI_DATA_DIR") == "/test-store", "Use a private test engine"
    if args.archives:
        args.archives.mkdir()
    nonce = uuid.uuid4().hex
    model = Registry(nonce)
    manifest = json.loads(model.manifest)
    config = json.loads(model.blobs.pop(manifest["config"]["digest"]))
    config["config"]["Labels"] = {LABEL: nonce}
    config_bytes = json.dumps(config, separators=(",", ":")).encode()
    config_digest = digest(config_bytes)
    model.blobs[config_digest] = config_bytes
    manifest["config"].update(digest=config_digest, size=len(config_bytes))
    model.manifest = json.dumps(manifest, separators=(",", ":")).encode()
    server = Server(("127.0.0.1", 0), model)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    repository = f"127.0.0.1:{server.server_port}/image-digest-{nonce}"
    reference, alias = repository + "@" + digest(model.manifest), repository + ":kept"
    record = {
        "schema": "asagiri.image-digest-export.v1", "status": "running",
        "reference_docker": args.reference_docker, "owner_nonce": nonce,
        "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "reference": reference, "alias": alias, "config_digest": config_digest,
        "cases": [], "cleanup": [],
    }
    image_id = None
    before = None

    def api(method, path, statuses=(200,)):
        connection = Connection(args.socket)
        try:
            connection.request(method, "/v1.47" + path)
            response = connection.getresponse()
            data = response.read(8 * 1024**2 + 1)
            assert len(data) <= 8 * 1024**2
            assert response.status in statuses, (method, path, response.status, data[:1000])
            return response.status, data
        finally:
            connection.close()

    def get(path):
        return json.loads(api("GET", path)[1])

    def image_path(name):
        return "/images/" + urllib.parse.quote(name, safe="") + "/json"

    def inventory():
        return sorted((row["Id"], sorted(row.get("RepoTags") or []), sorted(row.get("RepoDigests") or []))
                      for row in get("/images/json?all=1"))

    def saved(name, path, tags):
        data = api("GET", path)[1]
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            members = archive.getmembers()
            assert len({member.name for member in members}) == len(members)
            rows = json.load(archive.extractfile("manifest.json"))
            assert len(rows) == 1 and sorted(rows[0]["RepoTags"] or []) == tags, rows
            raw_config = archive.extractfile(rows[0]["Config"]).read()
            assert digest(raw_config) == config_digest
            diff_ids = json.loads(raw_config)["rootfs"]["diff_ids"]
            assert len(rows[0]["Layers"]) == len(diff_ids) == 6
            for path, expected in zip(rows[0]["Layers"], diff_ids, strict=True):
                blob = archive.extractfile(path).read()
                if blob.startswith(b"\x1f\x8b"):
                    blob = gzip.decompress(blob)
                assert digest(blob) == expected
        if args.archives:
            (args.archives / (name + ".tar")).write_bytes(data)
        record["cases"].append({"name": name, "passed": True, "tags": tags,
                                "archive_sha256": digest(data)[7:], "config_digest": config_digest,
                                "layer_diff_ids": diff_ids})

    try:
        version = get("/version")
        assert bool(version.get("AsagiriVersion")) is not args.reference_docker
        assert "Podman" not in json.dumps(version)
        record["engine_version"] = version
        before = inventory()
        assert config_digest not in {row[0] for row in before}
        api("GET", image_path(reference), (404,))
        raw = api("POST", "/images/create?" + urllib.parse.urlencode({"fromImage": reference}))[1]
        progress = [json.loads(line) for line in raw.splitlines() if line]
        assert progress and not any(row.get("error") for row in progress), progress
        info = get(image_path(reference))
        image_id = info["Id"]
        assert image_id not in {row[0] for row in before}
        assert info["Config"]["Labels"][LABEL] == nonce
        assert not info.get("RepoTags") and reference in info["RepoDigests"], info
        for name, rows in (("list", get("/images/json?all=1")), ("disk-usage", get("/system/df")["Images"])):
            selected = [row for row in rows if row["Id"] == image_id]
            assert selected and all(not row.get("RepoTags") for row in selected), selected
            assert all(reference in row["RepoDigests"] for row in selected), selected
            record["cases"].append({"name": name, "passed": True, "image_id": image_id})
        saved("digest-only-save", "/images/" + urllib.parse.quote(reference, safe="") + "/get", [])
        api("POST", "/images/" + urllib.parse.quote(reference, safe="") + "/tag?"
            + urllib.parse.urlencode({"repo": repository, "tag": "kept"}), (201,))
        assert get(image_path(alias))["Id"] == image_id
        query = urllib.parse.urlencode([("names", reference), ("names", alias), ("names", image_id)])
        saved("mixed-reference-save", "/images/get?" + query, [alias])
        record.update(status="passed", image_id=image_id)
    except BaseException as error:
        record.update(status="failed", error=repr(error))
    finally:
        try:
            status, data = api("GET", image_path(image_id or reference), (200, 404))
            if status == 200:
                info = json.loads(data)
                assert before is not None and info["Id"] not in {row[0] for row in before}
                assert info["Config"]["Labels"][LABEL] == nonce
                assert all(name in (reference, alias) for name in info.get("RepoTags") or [])
                assert all(name.startswith(repository + "@") for name in info.get("RepoDigests") or [])
                image_id = info["Id"]
                api("DELETE", "/images/" + urllib.parse.quote(image_id, safe="") + "?force=true")
                api("GET", image_path(image_id), (404,))
                record["cleanup"].append({"image_id": image_id, "absent": True})
            assert before is not None and inventory() == before, "Original image inventory changed"
            record["original_image_inventory_unchanged"] = True
        except BaseException as error:
            record.update(status="failed", cleanup_error=repr(error))
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
        assert not thread.is_alive()
        print(json.dumps(record, indent=2), flush=True)
    assert record["status"] == "passed", "Inspect the image digest export evidence"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("--reference-docker", action="store_true")
    parser.add_argument("--archives", type=Path)
    check(parser.parse_args())
