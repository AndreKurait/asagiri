#!/usr/bin/env python3
"""Failed starts retain their API error until a successful retry, including across daemon restart."""
import argparse
import http.client
import io
import json
import os
from pathlib import Path
import re
import socket
import tarfile
import uuid

LABEL = "asagiri.test.start-error"
COMMAND = "/tmp/start-error-command"


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(args):
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1", "requires an explicitly disposable engine"
    nonce = uuid.uuid4().hex
    owned = []
    retain = False

    def request(method, route, body=None, statuses=(200,), raw=False):
        connection = Connection(args.socket)
        try:
            payload = body if isinstance(body, bytes) else None if body is None else json.dumps(body)
            content_type = "application/x-tar" if isinstance(body, bytes) else "application/json"
            connection.request(method, "/v1.47" + route, payload, {"Content-Type": content_type})
            response = connection.getresponse()
            data = response.read()
            if route == "/containers/create" and response.status == 201:
                owned.append(json.loads(data)["Id"])
            assert response.status in statuses, (method, route, response.status, data)
            return data if raw else json.loads(data) if data else None
        finally:
            connection.close()

    def inspect(cid):
        value = request("GET", "/containers/" + cid + "/json")
        assert value["Id"] == cid and value["Config"]["Labels"][LABEL] == nonce, value
        return value["State"]

    def failed(cid, expected_exit, text):
        error = request("POST", "/containers/" + cid + "/start", statuses=(400,))["message"]
        state = inspect(cid)
        assert text in error.lower(), error
        assert state["Error"] == error and state["ExitCode"] == expected_exit, (state, error)
        assert not state["Running"] and state["Pid"] == 0, state
        return error

    def upload(cid, mode, content=None):
        if content is None:
            content = ("#!/bin/sh\nprintf '%s\\n' started-" + nonce + "\n").encode()
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as tar:
            item = tarfile.TarInfo(Path(COMMAND).name)
            item.size, item.mode = len(content), mode
            tar.addfile(item, io.BytesIO(content))
        request("PUT", "/containers/" + cid + "/archive?path=/tmp", archive.getvalue())

    def create(tty=False, init=False):
        return request("POST", "/containers/create", {
            "Image": args.image, "Cmd": [COMMAND], "Tty": tty, "Labels": {LABEL: nonce},
            "HostConfig": {"NetworkMode": "none", "Init": init, "RestartPolicy": {"Name": "no"}},
        }, (201,))["Id"]

    def prepare(tty=False):
        cid = create(tty)
        assert inspect(cid)["Error"] == ""
        missing = failed(cid, 127, "no such file")
        upload(cid, 0o644)
        permission = failed(cid, 126, "permission denied")
        assert permission != missing, "a later failure must replace the earlier error"
        return cid, permission

    def recover(cid):
        upload(cid, 0o755)
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        status = request("POST", "/containers/" + cid + "/wait")["StatusCode"]
        state = inspect(cid)
        assert status == 0 and state["ExitCode"] == 0 and not state["Running"], state
        assert state["Error"] == "", state
        logs = request("GET", "/containers/" + cid + "/logs?stdout=1", raw=True)
        assert ("started-" + nonce).encode() in logs, logs
        # A fast process exit must not keep the previous failed-start error.
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        assert request("POST", "/containers/" + cid + "/wait")["StatusCode"] == 0
        assert inspect(cid)["Error"] == ""

    def post_start_controls():
        cid = create()
        for content in (b"echo no-shebang\n", b"#!/missing-interpreter\n"):
            upload(cid, 0o755, content)
            request("POST", "/containers/" + cid + "/start", statuses=(204,))
            assert request("POST", "/containers/" + cid + "/wait")["StatusCode"] == 255
            assert inspect(cid)["Error"] == ""
        # Docker starts tini itself successfully. A missing program is then
        # tini's exit, and a script without a shebang is run through /bin/sh.
        supervised = create(init=True)
        request("POST", "/containers/" + supervised + "/start", statuses=(204,))
        assert request("POST", "/containers/" + supervised + "/wait")["StatusCode"] == 127
        assert inspect(supervised)["Error"] == ""
        upload(supervised, 0o755, b"echo init-no-shebang\n")
        request("POST", "/containers/" + supervised + "/start", statuses=(204,))
        assert request("POST", "/containers/" + supervised + "/wait")["StatusCode"] == 0
        assert inspect(supervised)["Error"] == ""

    try:
        if args.verify_restart:
            saved = json.loads(args.verify_restart.read_text())
            nonce = saved["nonce"]
            assert re.fullmatch("[0-9a-f]{32}", nonce)
            for cid in (saved["failed"], saved["recovered"]):
                assert re.fullmatch("[0-9a-f]{64}", cid)
                inspect(cid)  # Establish the saved label before adopting cleanup.
                owned.append(cid)
            state = inspect(saved["failed"])
            assert state["Error"] == saved["error"] and state["ExitCode"] == 126, state
            assert not state["Running"] and state["Pid"] == 0
            state = inspect(saved["recovered"])
            assert state["Error"] == "" and state["ExitCode"] == 0 and not state["Running"], state
            recover(saved["failed"])
            print("PASS failed and cleared start errors survived a new daemon; same-container retry succeeded", flush=True)
        else:
            cid, error = prepare()
            if args.prepare_restart:
                recovered, _ = prepare(tty=True)
                recover(recovered)
                with args.prepare_restart.open("x") as output:
                    json.dump({"nonce": nonce, "failed": cid, "recovered": recovered, "error": error}, output)
                    output.write("\n")
                retain = True
                print("PASS owned failed and recovered containers prepared for daemon restart", flush=True)
            else:
                recover(cid)
                terminal, _ = prepare(tty=True)
                recover(terminal)
                post_start_controls()
                print("PASS pipe/TTY failed starts expose the latest error; successful and fast-exit retries clear it", flush=True)
                print("PASS post-start exec failures and init supervision keep their process-exit semantics", flush=True)
    finally:
        errors = []
        if not retain:
            for cid in reversed(owned):
                try:
                    state = inspect(cid)
                    if state["Running"]:
                        request("POST", "/containers/" + cid + "/stop?t=1", statuses=(204,))
                    request("DELETE", "/containers/" + cid + "?v=1", statuses=(204,))
                except Exception as error:
                    errors.append((cid, repr(error)))
        assert not errors, ("ordinary cleanup failed", errors)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image", nargs="?", default="alpine:3.21")
    phase = parser.add_mutually_exclusive_group()
    phase.add_argument("--prepare-restart", type=Path)
    phase.add_argument("--verify-restart", type=Path)
    check(parser.parse_args())
