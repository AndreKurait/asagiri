#!/usr/bin/env python3
"""Exercise the stdio MCP client boundary inside a disposable Linux engine."""
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid


class MCP:
    def __init__(self, binary, environment):
        self.stderr = tempfile.TemporaryFile()
        self.process = subprocess.Popen([binary, "mcp"], env=environment, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.stderr)
        self.messages = queue.Queue(maxsize=32)
        self.counter = 0

        def read():
            try:
                while line := self.process.stdout.readline(512 * 1024 + 1):
                    assert len(line) <= 512 * 1024 and line.endswith(b"\n")
                    message = json.loads(line)
                    assert message["jsonrpc"] == "2.0"
                    self.messages.put(message)
            except BaseException as error:
                self.messages.put(error)

        threading.Thread(target=read, daemon=True).start()
        self.request("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                    "clientInfo": {"name": "asagiri-conformance", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, value):
        self.process.stdin.write(json.dumps(value).encode() + b"\n")
        self.process.stdin.flush()

    def begin(self, method, params):
        self.counter += 1
        self.send({"jsonrpc": "2.0", "id": self.counter, "method": method, "params": params})
        return self.counter

    def response(self, expected, timeout=10):
        deadline = time.monotonic() + timeout
        while True:
            value = self.messages.get(timeout=max(0.01, deadline - time.monotonic()))
            if isinstance(value, BaseException):
                raise value
            if "id" not in value:
                assert value["method"] == "notifications/tools/list_changed", value
                continue
            assert value["id"] == expected, value
            return value

    def request(self, method, params=None, timeout=10):
        return self.response(self.begin(method, params or {}), timeout)

    def tool(self, name, arguments=None, error=False):
        response = self.request("tools/call", {"name": name, "arguments": arguments or {}})
        assert "error" not in response, response
        result = response["result"]
        assert bool(result.get("isError")) == error, result
        if error:
            return result
        value = result["structuredContent"]
        assert json.loads(result["content"][0]["text"]) == value
        return value

    def close(self):
        self.process.stdin.close()
        try:
            assert self.process.wait(timeout=3) == 0
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait()
            self.stderr.close()


def check(path, image):
    assert sys.platform == "linux" and os.geteuid() == 0
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1" and Path("/.dockerenv").is_file()
    assert os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
    assert path == "/run/asagiri/docker.sock"
    binary = os.environ.get("ASAGIRI_MCP_CLI", "/opt/asagiri/asagiri")
    environment = dict(os.environ, ASAGIRI_SOCKET=path)
    environment.pop("DOCKER_CONTEXT", None)
    policy = Path.home() / ".asagiri/integrations/mcp.json"
    assert not policy.exists(), "use a fresh private engine without existing integration settings"
    containers = []
    session = None
    prefix = "asagiri-mcp-" + uuid.uuid4().hex[:10]
    sentinel = "ASAGIRI_TEST_METADATA_SECRET_" + uuid.uuid4().hex

    def cli(*args):
        result = subprocess.run([binary, "mcp", *args], env=environment, capture_output=True, text=True, timeout=10)
        assert result.returncode == 0, (args, result.stderr)
        return json.loads(result.stdout)

    def docker(*args):
        result = subprocess.run(["docker", "-H", "unix://" + path, *args],
                                env=environment, capture_output=True, text=True, timeout=60)
        assert result.returncode == 0, (args, result.stderr)
        return result.stdout.strip()

    try:
        assert cli("status") == {"enabled": False, "allowContainerChanges": False}
        session = MCP(binary, environment)
        assert session.request("tools/list")["result"]["tools"] == []
        session.tool("asagiri_status", error=True)
        cli("enable")
        assert policy.stat().st_mode & 0o777 == 0o600
        tools = session.request("tools/list")["result"]["tools"]
        assert len(tools) == 7 and all(t["annotations"]["readOnlyHint"] for t in tools)
        config = cli("config")["mcpServers"]["asagiri"]
        assert config["args"] == ["mcp"] and Path(config["command"]).is_absolute()
        assert session.tool("asagiri_status")["version"]["AsagiriVersion"]
        for suffix in ("first", "second"):
            cid = docker("create", "--name", prefix + "-" + suffix, "--env", "SECRET=" + sentinel,
                         "--label", "secret=" + sentinel, image, "sh", "-c",
                         "echo mcp-stdout; echo mcp-stderr >&2; sleep 120")
            containers.append(cid)
        selected = session.tool("asagiri_list_containers", {"limit": 1})
        assert len(selected["containers"]) == 1 and selected["truncated"]
        assert sentinel not in json.dumps(selected)
        inspected = session.tool("asagiri_inspect_container", {"container": containers[0]})
        assert inspected["state"]["Status"] == "created"
        assert sentinel not in json.dumps(inspected) and "Env" not in inspected["config"]
        for name, key in [("images", "images"), ("volumes", "volumes"), ("networks", "networks")]:
            assert isinstance(session.tool("asagiri_list_" + name)[key], list)
        session.tool("asagiri_start_container", {"container": containers[0]}, error=True)
        assert docker("inspect", "-f", "{{.State.Status}}", containers[0]) == "created"
        assert session.request("tools/call", {"name": "asagiri_exec", "arguments": {"container": containers[0]}})["error"]["code"] == -32602
        assert session.request("tools/call", {"name": "asagiri_status", "arguments": []})["error"]["code"] == -32602
        session.tool("asagiri_inspect_container", {"container": "x/../../info"}, error=True)
        session.tool("asagiri_list_containers", {"limit": 1000}, error=True)
        cli("allow-changes", "on")
        assert len(session.request("tools/list")["result"]["tools"]) == 10
        session.tool("asagiri_start_container", {"container": containers[0]})
        deadline = time.monotonic() + 5
        while True:
            logs = session.tool("asagiri_container_logs", {"container": containers[0]})
            if "mcp-stdout" in logs["logs"] and "mcp-stderr" in logs["logs"]:
                break
            assert time.monotonic() < deadline, logs
            time.sleep(0.05)
        session.tool("asagiri_container_logs", {"container": containers[0], "follow": True}, error=True)
        session.tool("asagiri_stop_container", {"container": containers[0], "timeout": 0})
        assert docker("inspect", "-f", "{{.State.Status}}", containers[0]) == "exited"
        session.tool("asagiri_restart_container", {"container": containers[0], "timeout": 0})
        assert docker("inspect", "-f", "{{.State.Status}}", containers[0]) == "running"
        cli("disable")
        assert session.request("tools/list")["result"]["tools"] == []
        session.tool("asagiri_restart_container", {"container": containers[0]}, error=True)
        assert cli("status") == {"enabled": False, "allowContainerChanges": False}
        session.close()
        session = None
        print("PASS MCP stdio discovery, metadata redaction, readonly default, actions and live revocation", flush=True)

        cli("enable")
        with tempfile.TemporaryDirectory(prefix=prefix) as directory:
            fake = str(Path(directory) / "engine.sock")
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(fake)
                listener.listen(8)
                listener.settimeout(10)
                session = MCP(binary, dict(environment, ASAGIRI_SOCKET=fake))
                pending = session.begin("tools/call", {"name": "asagiri_status"})
                peer, _ = listener.accept()
                with peer:
                    peer.settimeout(2)
                    request = b""
                    while not request.endswith(b"\r\n\r\n"):
                        request += peer.recv(2048)
                        assert len(request) <= 16384
                    assert b"X-Asagiri-Background: 1" in request
                    session.send({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": pending}})
                    assert peer.recv(2048) == b"", "cancellation must close the native engine connection"
                assert session.request("ping")["result"] == {}
                # A second hung call must finish without a cancellation from
                # the client. Exercise the real 45-second server deadline.
                started = time.monotonic()
                pending = session.begin("tools/call", {"name": "asagiri_status"})
                peer, _ = listener.accept()
                with peer:
                    assert peer.recv(2048)
                    response = session.response(pending, timeout=50)
                    assert 40 <= time.monotonic() - started < 50
                    assert ("timed out" in json.dumps(response)), response
                session.close()
                session = None
        print("PASS MCP cancellation closes stalled I/O and the request deadline is enforced", flush=True)
    finally:
        if session is not None:
            session.close()
        cli("disable")
        for cid in containers:
            docker("rm", "-f", cid)
        if policy.exists():
            policy.unlink()
        lock = policy.with_name("mcp.lock")
        if lock.exists():
            lock.unlink()
        if policy.parent.exists():
            policy.parent.rmdir()


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
