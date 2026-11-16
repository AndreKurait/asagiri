#!/usr/bin/env python3
"""A native pull completes only after its layers are installed in the guest."""
import json
import os
from pathlib import Path
import runpy
import sys
import threading
from urllib.parse import quote
import uuid

helpers = runpy.run_path(str(Path(__file__).with_name("registry-config.py")))
Connection, Registry, Server, digest = (helpers[name] for name in ("Connection", "Registry", "Server", "digest"))


def check(path):
    assert sys.platform == "linux" and os.environ.get("ASAGIRI_TEST_ENGINE") == "1", "private Linux test engine only"
    layers = Path("/var/lib/asagiri/layers")
    names, containers, servers = [], [], []
    seed = uuid.uuid4().hex

    def api(method, target, body=None, statuses=(200,)):
        connection = Connection(path)
        try:
            connection.request(method, target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            assert response.status in statuses, (method, target, response.status, data)
            return data
        finally:
            connection.close()

    def ready(sha):
        name = sha.removeprefix("sha256:")
        return (layers / name).is_dir() and (layers / (name + ".done")).is_file()

    original = json.loads(api("GET", "/asagiri/daemon-config"))
    try:
        valid, invalid = Registry(seed), Registry(seed + "-invalid")
        # Registry transport and digests are valid, but native extraction
        # must fail. A download-only pull incorrectly reports this as success.
        corrupt = b"\x1f\x8b" + seed.encode()
        sha = digest(corrupt)
        invalid.blobs[sha] = corrupt
        manifest = json.loads(invalid.manifest)
        manifest["layers"][0].update(digest=sha, size=len(corrupt))
        invalid.manifest = json.dumps(manifest).encode()
        hosts = []
        for model in (valid, invalid):
            server = Server(("127.0.0.1", 0), model)
            servers.append(server)
            threading.Thread(target=server.serve_forever, daemon=True).start()
            hosts.append(f"127.0.0.1:{server.server_port}")
        api("POST", "/asagiri/daemon-config",
            {"insecure-registries": original["insecure-registries"] + hosts})
        assert all(not ready(sha) for sha in valid.layers), "fixture layers must be cold"

        for host, label, succeeds in ((hosts[1], "invalid", False), (hosts[0], "valid", True)):
            name = host + "/pull-readiness-" + seed + "-" + label + ":latest"
            # A failed install may retain its verified downloads for a retry.
            names.append(name)
            raw = api("POST", "/images/create?fromImage=" + quote(name, safe=""))
            rows = [json.loads(line) for line in raw.splitlines() if line]
            errors = [row["error"] for row in rows if row.get("error")]
            complete = [row for row in rows if row.get("status", "").startswith("Status: Downloaded")]
            if not succeeds:
                assert errors and not complete, rows
                assert not ready(sha), "failed extraction left a completed layer"
                assert not any(row.get("status") == "Pull complete" for row in rows), rows
                print("PASS native extraction failure is returned by pull without success", flush=True)
                continue
            assert not errors and complete, rows
            assert all(ready(sha) for sha in valid.layers), "pull returned before native layers were ready"
            short_ids = {sha.removeprefix("sha256:")[:12] for sha in valid.layers}
            assert {row.get("id") for row in rows if row.get("status") == "Pull complete"} == short_ids
            body = json.loads(api("POST", "/containers/create",
                {"Image": name, "Cmd": ["true"], "HostConfig": {"NetworkMode": "none"}}, (201,)))
            containers.append(body["Id"])
            print("PASS pull installs all native layers and a later create succeeds after a failed pull", flush=True)
    finally:
        primary_failure = sys.exc_info()[0] is not None
        cleanup = []
        for kind, items in (("containers", containers), ("images", names)):
            for item in reversed(items):
                try:
                    api("DELETE", "/" + kind + "/" + quote(item, safe="") + "?force=true",
                        statuses=(200, 204, 404))
                except Exception as error:
                    cleanup.append(error)
                    print("Cleanup failed:", repr(error), file=sys.stderr)
        try:
            api("POST", "/asagiri/daemon-config", original)
        except Exception as error:
            cleanup.append(error)
            print("Configuration cleanup failed:", repr(error), file=sys.stderr)
        finally:
            for server in servers:
                server.shutdown()
                server.server_close()
        if cleanup and not primary_failure:
            raise RuntimeError("pull readiness fixture cleanup failed") from cleanup[0]


if __name__ == "__main__":
    check(sys.argv[1])
