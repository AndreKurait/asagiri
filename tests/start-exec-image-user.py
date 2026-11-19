#!/usr/bin/env python3
"""Check immediate non-root exec, image volume data and working-directory setup."""
import argparse
from datetime import datetime, timezone
import hashlib
import http.client
import json
import os
from pathlib import Path
import socket
import struct
import time
from urllib.parse import quote, urlencode
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=120)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image", help="already local Alpine image; never pulled by this fixture")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--reference-docker", action="store_true")
    mode.add_argument("--native-asagiri", action="store_true",
                      help="explicitly allow owned, labelled fixtures on a native Asagiri engine")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if not (args.reference_docker or args.native_asagiri):
        assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
        assert os.environ.get("ASAGIRI_DATA_DIR") == "/test-store" and Path("/.dockerenv").is_file()
        assert args.socket == os.environ.get("ASAGIRI_SOCKET")
    assert Path(args.socket).is_socket(), args.socket
    token = "asagiri-ready-" + uuid.uuid4().hex
    labels = {"io.asagiri.test": "start-exec-image-user", "io.asagiri.test.run": token}
    tag = token + ":latest"
    owned = {"container": [], "volume": [], "image": []}
    record = {
        "schema": "asagiri.start-exec-image-user.v1", "status": "running",
        "started_at": datetime.now(timezone.utc).isoformat(),
        "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "reference_docker": args.reference_docker, "native_asagiri": args.native_asagiri,
        "payload_bytes": 128 * 1024**2, "cases": [], "working_directory_cases": [],
        "cleanup": [], "owned": owned,
    }
    digest = hashlib.sha256()
    for _ in range(128):
        digest.update(bytes(1024**2))
    payload_sha = digest.hexdigest()
    record["payload_sha256"] = payload_sha

    def persist():
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            temporary = args.output.with_name(args.output.name + ".tmp")
            temporary.write_text(json.dumps(record, indent=2) + "\n")
            temporary.replace(args.output)

    def request(method, path, data=None, expected=(200,)):
        connection = Connection(args.socket)
        try:
            connection.request(method, "/v1.47" + path,
                               None if data is None else json.dumps(data).encode(),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            body = response.read(1024**2 + 1)
            assert len(body) <= 1024**2, "unexpectedly large fixture response"
            assert response.status in expected, (method, path, response.status, body[:1000])
            return response.status, body
        finally:
            connection.close()

    def value(method, path, data=None, expected=(200,)):
        return json.loads(request(method, path, data, expected)[1])

    def inspect_path(kind, name):
        return "/" + kind + "s/" + quote(name, safe="") + ("" if kind == "volume" else "/json")

    def register(kind, name):
        request("GET", inspect_path(kind, name), expected=(404,))
        owned[kind].append(name)
        persist()

    def create(name, image, command, user, mounts=(), *, workdir=None, readonly=False, tmpfs=None):
        register("container", name)
        body = {
            "Image": image, "Entrypoint": [], "Cmd": command,
            "Labels": labels, "HostConfig": {"NetworkMode": "none", "Mounts": list(mounts)},
        }
        if user is not None:
            body["User"] = user
        if workdir is not None:
            body["WorkingDir"] = workdir
        if readonly:
            body["HostConfig"]["ReadonlyRootfs"] = True
        if tmpfs:
            body["HostConfig"]["Tmpfs"] = tmpfs
        return value("POST", "/containers/create?" + urlencode({"name": name}), body, (201,))["Id"]

    def unframe(payload):
        output = bytearray()
        while payload:
            assert len(payload) >= 8 and payload[0] in (1, 2) and payload[1:4] == bytes(3), "invalid exec frame"
            length = struct.unpack(">I", payload[4:8])[0]
            assert len(payload) >= 8 + length, "truncated exec frame"
            output.extend(payload[8:8 + length])
            payload = payload[8 + length:]
        return output.decode()

    def execute(cid, explicit_user):
        command = 'id; test "$(id -u)" = 1000; test "$(id -un)" = dev; ' \
                  'test "$(pwd)" = /image/default/work; ' \
                  'test "$(stat -c %u:%g:%a .)" = 0:0:755; ' \
                  'test "$(stat -c %s /payload/blob)" = 134217728; ' \
                  f'test "$(sha256sum /payload/blob)" = "{payload_sha}  /payload/blob"; ' \
                  'printf "READY_IMAGE_USER\\n"'
        body = {"Cmd": ["sh", "-ec", command], "AttachStdout": True, "AttachStderr": True}
        if explicit_user:
            body["User"] = "dev"
        started = time.monotonic_ns()
        eid = value("POST", "/containers/" + cid + "/exec", body, (201,))["Id"]
        created = value("GET", "/exec/" + eid + "/json")
        assert created["Running"] is False and type(created["ExitCode"]) is int and created["ExitCode"] == 0, created
        text = unframe(request("POST", "/exec/" + eid + "/start", {"Detach": False, "Tty": False})[1])
        deadline = time.monotonic() + 5
        while True:
            state = value("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "exec did not finish after its output closed"
            time.sleep(0.01)
        assert state["ExitCode"] == 0 and text.endswith("READY_IMAGE_USER\n"), (state, text)
        assert "uid=1000(dev)" in text, text
        return {"explicit_user": explicit_user, "exit_code": 0, "output": text,
                "exec_started_ns": started, "exec_finished_ns": time.monotonic_ns()}

    def run_exec(cid, command, *, workdir=None):
        body = {"Cmd": ["sh", "-ec", command], "User": "0",
                "AttachStdout": True, "AttachStderr": True}
        if workdir is not None:
            body["WorkingDir"] = workdir
        eid = value("POST", "/containers/" + cid + "/exec", body, (201,))["Id"]
        status, raw = request("POST", "/exec/" + eid + "/start",
                              {"Detach": False, "Tty": False}, expected=(200, 400, 500))
        text = unframe(raw) if status == 200 else raw.decode()
        deadline = time.monotonic() + 5
        while True:
            state = value("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                return status, state, text
            assert time.monotonic() < deadline, "working-directory exec did not finish"
            time.sleep(.01)

    def workdirs(committed, base_id):
        cases = [
            ("numeric-user", base_id, "1000:1000", False, None),
            ("image-user", committed, None, False, None),
            ("image-user-readonly", committed, None, True, None),
            ("root-readonly", base_id, "0", True, None),
            ("image-user-tmpfs", committed, None, True, {"/workspace": "rw,nosuid,nodev,size=1m"}),
        ]
        for name, image_id, user, readonly, tmpfs in cases:
            directory = "/workspace/nested/work" if tmpfs else "/var/working-dir/" + name + "/nested"
            uid = 0 if user == "0" else 1000
            command = 'test "$(id -u)" = ' + str(uid) + '; test "$(pwd)" = ' + directory + '; ' \
                      'test "$(stat -c %u:%g:%a .)" = 0:0:755; printf "WORKDIR_READY\\n"; exec sleep 120'
            container_name = token + "-cwd-" + name
            cid = create(container_name, image_id, ["sh", "-ec", command], user,
                         workdir=directory, readonly=readonly, tmpfs=tmpfs)
            request("POST", "/containers/" + cid + "/start", expected=(204,))
            # The acknowledgement alone does not establish that the command
            # survived its user switch. The command must print its marker.
            deadline = time.monotonic() + 5
            while True:
                text = unframe(request("GET", "/containers/" + cid + "/logs?stdout=1&stderr=1")[1])
                state = value("GET", "/containers/" + cid + "/json")["State"]
                if "WORKDIR_READY\n" in text:
                    assert state["Running"], (name, state, text)
                    break
                assert state["Running"] and time.monotonic() < deadline, (name, state, text)
                time.sleep(.01)
            record["working_directory_cases"].append({
                "name": name, "status": "passed", "container": cid,
                "working_directory": directory, "uid": uid, "directory_owner_mode": "0:0:755",
                "readonly_rootfs": readonly, "tmpfs": bool(tmpfs), "output": text,
            })
            if name == "image-user":
                missing = "/exec-must-not-create-" + uuid.uuid4().hex
                status, state, text = run_exec(cid, "printf SHOULD_NOT_RUN", workdir=missing)
                assert status != 200 or state["ExitCode"] not in (None, 0), (status, state, text)
                assert "SHOULD_NOT_RUN" not in text, text
                assert missing in text or "working directory" in text or "cwd" in text, text
                record["working_directory_cases"].append({
                    "name": "missing-exec-workdir-rejected", "status": "passed",
                    "http_status": status, "exit_code": state["ExitCode"], "output": text,
                })
                status, state, text = run_exec(cid, "test ! -e " + missing + "; pwd", workdir="/tmp")
                assert status == 200 and state["ExitCode"] == 0 and text == "/tmp\n", (status, state, text)
                record["working_directory_cases"].append({
                    "name": "existing-exec-workdir-and-no-created-path", "status": "passed",
                    "http_status": status, "exit_code": state["ExitCode"], "output": text,
                })
            persist()
            remove("container", container_name)
        assert len(record["working_directory_cases"]) == 7

    def remove(kind, name):
        status, raw = request("GET", inspect_path(kind, name), expected=(200, 404))
        if status == 200:
            details = json.loads(raw)
            observed_labels = details.get("Labels") if kind == "volume" else details.get("Config", {}).get("Labels")
            assert observed_labels == labels, ("refusing to remove an unowned fixture", kind, name)
            if kind == "image":
                assert set(details.get("RepoTags") or ()) <= {tag}, "image acquired another owner's tag"
            request("DELETE", "/" + kind + "s/" + quote(name, safe="") +
                    ("?force=1" if kind == "container" else ""), expected=(200, 204))
        request("GET", inspect_path(kind, name), expected=(404,))
        owned[kind].remove(name)
        record["cleanup"].append({"kind": kind, "name": name, "inspect_status": 404, "passed": True})
        persist()

    try:
        version = value("GET", "/version")
        record["engine"] = version
        assert (version.get("Platform", {}).get("Name") == "Asagiri") is not args.reference_docker, version
        base = value("GET", inspect_path("image", args.image))
        base_id = base["Id"]
        record["base_image"] = {key: base.get(key) for key in ("Id", "Architecture", "Os", "RootFS")}
        seed_name = token + "-seed"
        seed = create(seed_name, base_id, ["sh", "-ec",
                      "adduser -D -u 1000 dev; mkdir /payload; "
                      "dd if=/dev/zero of=/payload/blob bs=1M count=128"], "0")
        request("POST", "/containers/" + seed + "/start", expected=(204,))
        waited = value("POST", "/containers/" + seed + "/wait?condition=not-running")
        assert waited["StatusCode"] == 0, waited
        register("image", tag)
        committed = value("POST", "/commit?" + urlencode({"container": seed, "repo": token, "tag": "latest"}), {
            "User": "dev", "Labels": labels, "Cmd": ["sleep", "120"], "Entrypoint": [],
            "WorkingDir": "/image/default/work",
        }, (201,))["Id"]
        assert committed != base_id
        image = value("GET", inspect_path("image", tag))
        assert image["Id"] == committed and image["Config"]["User"] == "dev"
        assert image["Config"]["WorkingDir"] == "/image/default/work"
        assert image["Config"]["Labels"] == labels
        record["fixture_image"] = {key: image.get(key) for key in ("Id", "Architecture", "Os", "RootFS")}
        remove("container", seed_name)
        for iteration in range(3):
            volume, name = token + "-volume-" + str(iteration), token + "-container-" + str(iteration)
            register("volume", volume)
            value("POST", "/volumes/create", {"Name": volume, "Driver": "local", "Labels": labels}, (201,))
            cid = create(name, committed, ["sleep", "120"], None,
                         [{"Type": "volume", "Source": volume, "Target": "/payload"}])
            start_ns = time.monotonic_ns()
            request("POST", "/containers/" + cid + "/start", expected=(204,))
            returned_ns = time.monotonic_ns()
            # No sleep, inspect, log poll or workload-readiness wait before exec.
            explicit = execute(cid, True)
            inherited = execute(cid, False)
            record["cases"].append({
                "round": iteration + 1, "volume": volume, "container": cid,
                "start_duration_ms": (returned_ns - start_ns) / 1e6,
                "start_to_exec_ms": (explicit["exec_started_ns"] - returned_ns) / 1e6,
                "execs": [explicit, inherited], "passed": True,
            })
            if iteration == 0:
                missing_user = "missing-" + uuid.uuid4().hex
                eid = value("POST", "/containers/" + cid + "/exec", {
                    "Cmd": ["id"], "User": missing_user, "AttachStderr": True,
                }, (201,))["Id"]
                status, error = request("POST", "/exec/" + eid + "/start", {"Detach": False, "Tty": False},
                                        expected=(200, 400, 500))
                # An attached exec may already have sent HTTP 200 before
                # reporting a startup failure on its stderr stream.
                text = unframe(error) if status == 200 else error.decode()
                assert missing_user in text and "user" in text.lower(), text
                deadline = time.monotonic() + 5
                while True:
                    state = value("GET", "/exec/" + eid + "/json")
                    if not state["Running"]:
                        break
                    assert time.monotonic() < deadline, "missing-user exec is still running"
                    time.sleep(0.01)
                if status == 200:
                    assert state["ExitCode"] not in (None, 0), (state, text)
                record["missing_user_control"] = {
                    "http_status": status, "exit_code": state["ExitCode"],
                    "running": state["Running"], "error": text, "rejected": True,
                }
            remove("container", name)
            remove("volume", volume)
        workdirs(committed, base_id)
        remove("image", tag)
        request("GET", inspect_path("image", committed), expected=(404,))
        assert value("GET", inspect_path("image", args.image))["Id"] == base_id
        record["base_image_preserved"] = True
        record["fixture_image_removed_by_id"] = True
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error))
        raise
    finally:
        for kind in ("container", "volume", "image"):
            for name in list(owned[kind]):
                try:
                    remove(kind, name)
                except Exception as error:
                    record["status"] = "failed"
                    record["cleanup"].append({"kind": kind, "name": name, "passed": False, "error": repr(error)})
        persist()
        print(json.dumps(record), flush=True)
    assert record["status"] == "passed", record
    print("PASS three fresh 128 MiB volumes: immediate explicit/inherited image-user exec and owned cleanup", flush=True)


if __name__ == "__main__":
    main()
