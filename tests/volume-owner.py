#!/usr/bin/env python3
"""Compare tmpfs volume ownership, image copy-up and restarts through the API."""
import argparse
from datetime import datetime, timezone
import http.client
import json
import os
from pathlib import Path
import socket
import struct
import tempfile
import time
from urllib.parse import urlencode
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=45)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


SCRIPT = r'''printf 'OWNER '; stat -c '%u:%g %a' "$1"
printf 'COUNT '; find "$1" -mindepth 1 -maxdepth 1 | wc -l
if [ -f "$1/repositories" ]; then printf 'REPOS '; sha256sum "$1/repositories"; fi
awk -v target="$1" '$2 == target {print "MOUNT " $0}' /proc/mounts
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image", help="already local Alpine image; this fixture never pulls")
    parser.add_argument("--reference-docker", action="store_true")
    parser.add_argument("--missing-only", action="store_true", help="run the original missing-directory reproduction")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    record = {"schema": "asagiri.volume-owner.v1", "status": "running",
              "started_at": datetime.now(timezone.utc).isoformat(), "image": args.image,
              "reference_docker": args.reference_docker, "cases": [], "cleanup": [], "failures": []}
    containers, volumes, temporary = [], [], None
    observations = {}
    prefix = "asagiri-owner-" + uuid.uuid4().hex

    def request(method, path, data=None, expected=(200,)):
        connection = Connection(args.socket)
        try:
            connection.request(method, "/v1.47" + path, None if data is None else json.dumps(data).encode(),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            body = response.read(4 * 1024**2 + 1)
            assert len(body) <= 4 * 1024**2, "unexpectedly large fixture response"
            assert response.status in expected, (method, path, response.status, body[:1000])
            return body
        finally:
            connection.close()

    def create(name, target, mounts):
        containers.append(name)
        value = json.loads(request("POST", "/containers/create?" + urlencode({"name": name}), {
            "Image": args.image, "Entrypoint": [], "Cmd": ["sh", "-ec", SCRIPT, "sh", target],
            "Labels": {"io.asagiri.test": "volume-owner"},
            "HostConfig": {"NetworkMode": "none", "Mounts": mounts},
        }, (201,)))
        return value["Id"]

    def observe(cid):
        request("POST", "/containers/" + cid + "/start", expected=(204,))
        wait = json.loads(request("POST", "/containers/" + cid + "/wait?condition=not-running"))
        wanted = observations.get(cid, 0) + 1
        deadline = time.monotonic() + 5
        while True:
            payload = request("GET", "/containers/" + cid + "/logs?stdout=1&stderr=1")
            text = bytearray()
            while payload:
                assert len(payload) >= 8 and payload[1:4] == b"\0\0\0", "invalid Docker log frame"
                size = struct.unpack(">I", payload[4:8])[0]
                assert len(payload) >= 8 + size, "truncated Docker log frame"
                text.extend(payload[8:8 + size])
                payload = payload[8 + size:]
            output = text.decode()
            assert wait["StatusCode"] == 0, (wait, output)
            lines = output.splitlines()
            starts = [index for index, line in enumerate(lines) if line.startswith("OWNER ")]
            if len(starts) == wanted and any(line.startswith("COUNT ") for line in lines[starts[-1]:]):
                break
            assert time.monotonic() < deadline, ("missing output from this start", wanted, output)
            time.sleep(0.05)
        observations[cid] = wanted
        lines = lines[starts[-1]:]
        owner = next(line[6:].split() for line in lines if line.startswith("OWNER "))
        uid, gid = map(int, owner[0].split(":"))
        return {"owner": [uid, gid, int(owner[1], 8)],
                "count": int(next(line[6:] for line in lines if line.startswith("COUNT "))),
                "repositories_sha256": next((line.split()[1] for line in lines if line.startswith("REPOS ")), None),
                "mounts": [line[6:] for line in lines if line.startswith("MOUNT ")]}

    def remove_container(name):
        request("DELETE", "/containers/" + name + "?force=1", expected=(204, 404))
        containers.remove(name)
        record["cleanup"].append({"kind": "container", "id": name, "passed": True})

    try:
        version = json.loads(request("GET", "/version"))
        record["engine"] = version
        assert (version.get("Platform", {}).get("Name") == "Asagiri") is not args.reference_docker
        json.loads(request("GET", "/images/" + args.image + "/json"))
        baseline = {}
        if not args.missing_only:
            for name, target in [("empty", "/tmp"), ("contents", "/etc/apk")]:
                container = prefix + "-baseline-" + name
                baseline[name] = observe(create(container, target, []))
                remove_container(container)
            assert baseline["empty"]["count"] == 0 and baseline["contents"]["repositories_sha256"], baseline
            temporary = tempfile.TemporaryDirectory(prefix=prefix + "-bind-")
            host = Path(temporary.name)
            (host / "keep.txt").write_text("host contents\n")
            (host / "apk").mkdir()
            (host / "apk" / "host-only.txt").write_text("must not populate the volume\n")
            before = host.stat()
            host_metadata = (before.st_uid, before.st_gid, before.st_mode)
        record["baseline"] = baseline
        cases = [("missing", "/" + prefix + "/deep", False, None, None)]
        if not args.missing_only:
            cases += [
                ("image-empty", "/tmp", False, "empty", None),
                ("image-contents", "/etc/apk", False, "contents", None),
                ("nocopy", "/etc/apk", True, None, None),
                ("nested-missing", "/" + prefix + "-host/deep", False, None, "/" + prefix + "-host"),
                ("nested-image", "/etc/apk", False, "contents", "/etc"),
            ]
        for name, target, nocopy, source, bind_target in cases:
            value = {"name": name, "rounds": [], "passed": False}
            record["cases"].append(value)
            volume = prefix + "-" + name
            volumes.append(volume)
            try:
                request("POST", "/volumes/create", {"Name": volume, "Driver": "local",
                    "DriverOpts": {"type": "tmpfs", "device": "tmpfs", "o": "size=1m,uid=10001,gid=10050,mode=2750"},
                    "Labels": {"io.asagiri.test": "volume-owner"}}, (201,))
                mounts = [] if bind_target is None else [{"Type": "bind", "Source": temporary.name, "Target": bind_target}]
                mounts.append({"Type": "volume", "Source": volume, "Target": target, "VolumeOptions": {"NoCopy": nocopy}})
                container = prefix + "-container-" + name
                cid = create(container, target, mounts)
                expected = baseline[source] if source else {"owner": [10001, 10050, 0o2750], "count": 0, "repositories_sha256": None}
                for iteration in range(2):
                    observed = observe(cid)
                    passed = all(observed[key] == expected[key] for key in ("owner", "count", "repositories_sha256"))
                    value["rounds"].append({"iteration": iteration, "observed": observed, "expected": expected, "passed": passed})
                    if not passed: record["failures"].append(f"{name} iteration {iteration}: metadata or contents differ")
                remove_container(container)
                value["passed"] = all(result["passed"] for result in value["rounds"])
                print("PASS" if value["passed"] else "FAIL", name, "two starts", flush=True)
            except Exception as error:
                value["error"] = repr(error)
                record["failures"].append(name + ": " + repr(error))
        if temporary:
            after = host.stat()
            assert (after.st_uid, after.st_gid, after.st_mode) == host_metadata, "host bind root metadata changed"
            assert (host / "keep.txt").read_text() == "host contents\n"
            assert (host / "apk" / "host-only.txt").read_text() == "must not populate the volume\n"
            assert not (host / "apk" / "repositories").exists(), "image contents copied into the host bind"
            for path in [host, host / "apk", host / "deep"]:
                assert not any(name.endswith("asagiri.mountpoint") for name in os.listxattr(path)), (path, os.listxattr(path))
            record["host_bind_preserved"] = True
        record["status"] = "failed" if record["failures"] else "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error))
        raise
    finally:
        for kind, identifiers in (("container", containers), ("volume", volumes)):
            for identifier in identifiers:
                try:
                    request("DELETE", "/" + kind + "s/" + identifier + "?force=1", expected=(204, 404))
                    record["cleanup"].append({"kind": kind, "id": identifier, "passed": True})
                except Exception as error:
                    record["cleanup"].append({"kind": kind, "id": identifier, "passed": False, "error": repr(error)})
                    record["status"] = "failed"
        if temporary: temporary.cleanup()
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(record, indent=2) + "\n")
        print(json.dumps(record), flush=True)
    assert record["status"] == "passed", record["failures"]
    print("PASS", len(record["cases"]), "tmpfs volume ownership API cases", flush=True)


if __name__ == "__main__":
    main()
