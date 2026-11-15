#!/usr/bin/env python3
"""Exercise macOS host.docker.internal with IPv4 and IPv6 loopback servers.

Usage: host-loopback.py SOCKET [--docker PATH] [--output FILE]
Uses only two disposable containers, loopback listeners and a pinned image.
Upstream reproduction: https://github.com/libracoder/kiri/issues/258
"""

import argparse
import http.server
import json
import socket
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path

IMAGE = "alpine:3.21@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab"


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = self.server.marker.encode()
        self.server.requests.append(self.path)
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, family, port, marker):
        self.address_family = family
        address = "::1" if family == socket.AF_INET6 else "127.0.0.1"
        self.marker, self.requests = marker, []
        super().__init__((address, port), Handler, bind_and_activate=False)
        try:
            if family == socket.AF_INET6:
                self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
            self.server_bind()
            self.server_activate()
        except BaseException:
            self.server_close()
            raise
        self.worker = threading.Thread(target=self.serve_forever, daemon=True)
        self.worker.start()

    def close(self):
        self.shutdown()
        self.server_close()
        self.worker.join(timeout=3)
        assert not self.worker.is_alive(), "loopback server did not stop"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("--docker", default="docker")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("this test exercises the macOS VM's host loopback relay")
    docker = [args.docker, "--host", "unix://" + str(Path(args.socket).resolve())]
    report = {"image": IMAGE, "checks": [], "containers": [], "cleanup": []}
    containers, servers = [], []
    token = "asagiri-loopback-" + uuid.uuid4().hex

    def command(*words, timeout=45):
        start = time.monotonic()
        result = subprocess.run(docker + list(words), text=True, capture_output=True, timeout=timeout)
        return {"argv": list(words), "exit_code": result.returncode, "stdout": result.stdout,
                "stderr": result.stderr, "seconds": time.monotonic() - start}

    def fetch(phase, expected):
        for network, cid in containers:
            for host in ("host.docker.internal", "loopback-test"):
                result = command("exec", cid, "wget", "-qO-", "-T", "4",
                                 f"http://{host}:{port}/{phase}")
                result.update(phase=phase, network=network, host=host)
                result["passed"] = ((result["exit_code"] == 0 and result["stdout"] == expected)
                                    if expected is not None else
                                    (result["exit_code"] != 0 and not result["stdout"]))
                report["checks"].append(result)
                print("PASS" if result["passed"] else "FAIL", phase, network, host, flush=True)

    try:
        version = command("version", "--format", "{{json .Server}}")
        assert version["exit_code"] == 0, version
        report["server"] = json.loads(version["stdout"])
        v4 = Server(socket.AF_INET, 0, token + "-ipv4")
        servers.append(v4)
        port = v4.server_address[1]
        report["port"] = port
        for network in ("bridge", "host"):
            result = command("run", "-d", "--pull=never", "--network", network,
                             "--name", token + "-" + network,
                             "--label", "dev.asagiri.test=host-loopback",
                             "--add-host", "loopback-test:host-gateway", IMAGE, "sleep", "180")
            report["containers"].append(result)
            assert result["exit_code"] == 0, result
            containers.append((network, result["stdout"].strip()))
        fetch("ipv4-only", v4.marker)
        v6 = Server(socket.AF_INET6, port, token + "-ipv6")
        servers.append(v6)
        fetch("both-prefer-ipv4", v4.marker)
        v4.close()
        servers.remove(v4)
        fetch("ipv6-only", v6.marker)
        assert v4.requests, "IPv4 positive control received no HTTP request"
        report["ipv4_requests"] = v4.requests
        report["ipv6_requests"] = v6.requests
        v6.close()
        servers.remove(v6)
        fetch("neither-listening", None)
    finally:
        for _, cid in containers:
            report["cleanup"].append(command("rm", "-f", cid))
        for server in servers:
            server.close()
        if args.output:
            args.output.write_text(json.dumps(report, indent=2) + "\n")
    assert len(report["checks"]) == 16 and all(c["passed"] for c in report["checks"]), report
    assert all(c["exit_code"] == 0 for c in report["cleanup"]), report["cleanup"]


if __name__ == "__main__":
    main()
