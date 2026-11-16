#!/usr/bin/env python3
"""Failed starts retain their API error until a successful retry, including across daemon restart."""
import argparse
import base64
import http.client
import io
import json
import os
from pathlib import Path
import re
import socket
import tarfile
import time
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

    def request(method, route, body=None, statuses=(200,), raw=False, path_stat=False):
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
            if path_stat:
                return json.loads(base64.b64decode(response.getheader("X-Docker-Container-Path-Stat"), validate=True))
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
        assert state["Error"] == error and state["ExitCode"] == expected_exit, (state, error)
        assert text in error.lower(), error
        assert not state["Running"] and state["Pid"] == 0, state
        return error

    def put_entry(cid, directory, name, mode, content=b"", kind=tarfile.REGTYPE):
        path = Path(name)
        assert not path.is_absolute() and ".." not in path.parts
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as tar:
            for parent in reversed(path.parents):
                if str(parent) == ".":
                    continue
                item = tarfile.TarInfo(str(parent))
                item.type, item.mode = tarfile.DIRTYPE, 0o755
                tar.addfile(item)
            item = tarfile.TarInfo(str(path))
            item.type, item.mode = kind, mode
            item.size = len(content) if kind == tarfile.REGTYPE else 0
            tar.addfile(item, io.BytesIO(content) if item.size else None)
        request("PUT", "/containers/" + cid + "/archive?path=" + directory, archive.getvalue())

    def upload(cid, mode, content=None):
        if content is None:
            content = ("#!/bin/sh\nprintf '%s\\n' started-" + nonce + "\n").encode()
        put_entry(cid, "/tmp", Path(COMMAND).name, mode, content)

    def create(tty=False, init=False, command=None, env=None, user=None):
        body = {
            "Image": args.image, "Cmd": command if command is not None else [COMMAND],
            "Tty": tty, "Labels": {LABEL: nonce},
            "HostConfig": {"NetworkMode": "none", "Init": init, "RestartPolicy": {"Name": "no"}},
        }
        if env is not None:
            body["Env"] = env
        if user is not None:
            body["User"] = user
        return request("POST", "/containers/create", body, (201,))["Id"]

    def prepare(tty=False):
        cid = create(tty)
        assert inspect(cid)["Error"] == ""
        missing = failed(cid, 127, "no such file")
        upload(cid, 0o644)
        permission = failed(cid, 126, "permission denied")
        assert permission != missing, "a later failure must replace the earlier error"
        return cid, permission

    def successful(cid, marker):
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        status = request("POST", "/containers/" + cid + "/wait")["StatusCode"]
        state = inspect(cid)
        assert status == 0 and state["ExitCode"] == 0 and not state["Running"], state
        assert state["Error"] == "", state
        logs = request("GET", "/containers/" + cid + "/logs?stdout=1", raw=True)
        assert marker.encode() in logs, logs

    def recover(cid):
        upload(cid, 0o755)
        successful(cid, "started-" + nonce)
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

    def path_and_account_files():
        command = Path(COMMAND).name
        cid = create(command=[command], env=["PATH=/tmp/first:/tmp/later"])
        script = ("#!/bin/sh\nprintf '%s\\n' path-" + nonce + "\n").encode()
        put_entry(cid, "/tmp", "first/" + command, 0o644, script)
        failed(cid, 126, "permission denied")
        put_entry(cid, "/tmp", "later/" + command, 0o755, script)
        successful(cid, "path-" + nonce)
        print("PASS PATH permission failures use exit126 and a later executable still starts", flush=True)
        config = request("GET", "/images/" + args.image + "/json")["Config"]
        assert not any(item.startswith("HOME=") for item in config.get("Env") or []), "fixture must exercise HOME lookup"

        def prepare_fifo(cid, path):
            request("POST", "/containers/" + cid + "/start", statuses=(204,))
            # Archive upload materializes special entries as regular files.
            # Make the FIFO in the running fixture, then exercise a new start.
            exec_id = request("POST", "/containers/" + cid + "/exec", {
                "User": "0:0", "Cmd": ["/bin/sh", "-c", 'rm "$1" && mkfifo "$1"', "fifo-fixture", path],
            }, (201,))["Id"]
            request("POST", "/exec/" + exec_id + "/start", {"Detach": True, "Tty": False})
            deadline = time.monotonic() + 10
            while (state := request("GET", "/exec/" + exec_id + "/json"))["Running"]:
                assert time.monotonic() < deadline, state
                time.sleep(0.01)
            assert state["ExitCode"] == 0, state
            request("POST", "/containers/" + cid + "/stop?t=1", statuses=(204,))
            stat = request("HEAD", "/containers/" + cid + "/archive?path=" + path, path_stat=True)
            assert stat["mode"] & (1 << 25), stat  # Docker/Go ModeNamedPipe.

        for user, home in (("", "/root"), ("12345:12345", "/"), ("12345", "/")):
            marker = "home-" + nonce + ":"
            cid = create(user=user, command=["/bin/sh", "-c",
                "if [ -p /etc/passwd ]; then printf '%s%s\\n' " + marker + ' "$HOME"; else exec sleep 300; fi'])
            prepare_fifo(cid, "/etc/passwd")
            successful(cid, marker + home)
        regular = create(user="12345:12345", command=["/bin/sh", "-c", 'printf "%s\\n" "$HOME"'])
        put_entry(regular, "/etc", "passwd", 0o644, b"app:x:12345:12345::/home/from-passwd:/bin/sh\n")
        successful(regular, "/home/from-passwd")
        group = create(command=["/bin/sh", "-c",
            'if [ -p /etc/group ]; then printf "%s\\n" "$HOME"; else exec sleep 300; fi'])
        prepare_fifo(group, "/etc/group")
        successful(group, "/root")
        print("PASS FIFO passwd/group files do not block startup; default, numeric and regular HOME lookup stays correct", flush=True)

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
                path_and_account_files()
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
                    request("GET", "/containers/" + cid + "/json", statuses=(404,))
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
