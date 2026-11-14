#!/usr/bin/env python3
"""Exercise the real local browser in a disposable Linux engine, with optional Chromium."""
import base64
from concurrent.futures import ThreadPoolExecutor
import http.client
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlsplit
import uuid


class API:
    def __init__(self, url, password):
        self.origin = url.rstrip("/")
        self.host = urlsplit(url).netloc
        self.port = urlsplit(url).port
        self.authorization = "Basic " + base64.b64encode(("asagiri:" + password).encode()).decode()

    def request(self, method="GET", path="/", *, body=None, headers=None, authenticated=True):
        values = {"Authorization": self.authorization} if authenticated else {}
        if method == "POST":
            values.update({"Origin": self.origin, "Content-Type": "application/json"})
        values.update(headers or {})
        values = {key: value for key, value in values.items() if value is not None}
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=55)
        try:
            connection.request(method, path, body=body, headers=values)
            response = connection.getresponse()
            data = response.read(1024 * 1024)
            assert len(data) <= 512 * 1024
            return response.status, dict(response.getheaders()), data
        finally:
            connection.close()

    def tool(self, name, arguments=None, expected=200):
        status, _, body = self.request("POST", "/api/call", body=json.dumps({
            "name": name, "arguments": arguments or {},
        }).encode())
        assert status == expected, (name, status, body)
        result = json.loads(body)
        return result["result"] if status == 200 else result


def browser_ui(chrome, api, password, cli, docker, cid, prefix):
    """Use Chromium's real Basic-auth challenge, JavaScript, and DOM."""
    import websocket
    with tempfile.TemporaryDirectory(prefix="asagiri-browser-chrome-") as profile:
        log = tempfile.TemporaryFile()
        process = subprocess.Popen([
            chrome, "--headless=new", "--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage",
            "--disable-background-networking", "--no-first-run", "--no-default-browser-check",
            "--remote-debugging-address=127.0.0.1", "--remote-debugging-port=0",
            "--user-data-dir=" + profile, "about:blank",
        ], stdout=log, stderr=log)
        ws = None
        try:
            active = Path(profile) / "DevToolsActivePort"
            deadline = time.monotonic() + 30
            while not active.exists():
                assert process.poll() is None, "Chromium exited before DevTools became ready"
                assert time.monotonic() < deadline
                time.sleep(0.05)
            port, endpoint = active.read_text().splitlines()[:2]
            ws = websocket.create_connection(f"ws://127.0.0.1:{port}{endpoint}", timeout=15, suppress_origin=True)
            counter = 0
            session = None
            errors = []
            challenges = 0

            def send(method, params=None, *, target=True):
                nonlocal counter
                counter += 1
                message = {"id": counter, "method": method, "params": params or {}}
                if target and session:
                    message["sessionId"] = session
                ws.send(json.dumps(message))
                return counter

            def call(method, params=None, *, target=True):
                nonlocal challenges
                wanted = send(method, params, target=target)
                while True:
                    event = json.loads(ws.recv())
                    if event.get("method") == "Fetch.authRequired":
                        challenges += 1
                        assert event["params"]["request"]["url"].startswith(api.origin + "/")
                        send("Fetch.continueWithAuth", {"requestId": event["params"]["requestId"],
                             "authChallengeResponse": {"response": "ProvideCredentials", "username": "asagiri", "password": password}})
                    elif event.get("method") == "Fetch.requestPaused":
                        send("Fetch.continueRequest", {"requestId": event["params"]["requestId"]})
                    elif event.get("method") == "Runtime.exceptionThrown":
                        errors.append(event["params"]["exceptionDetails"].get("text"))
                    elif event.get("id") == wanted:
                        assert "error" not in event, event
                        return event.get("result", {})

            target = call("Target.createTarget", {"url": "about:blank"}, target=False)["targetId"]
            session = call("Target.attachToTarget", {"targetId": target, "flatten": True}, target=False)["sessionId"]
            call("Runtime.enable")
            call("Page.enable")
            call("Fetch.enable", {"handleAuthRequests": True})
            call("Emulation.setDeviceMetricsOverride", {"width": 1360, "height": 900, "deviceScaleFactor": 1, "mobile": False})
            call("Page.navigate", {"url": api.origin + "/"})

            def evaluate(expression):
                value = call("Runtime.evaluate", {"expression": expression, "returnByValue": True, "awaitPromise": True})
                assert "exceptionDetails" not in value, value
                return value["result"].get("value")

            def wait(expression):
                deadline = time.monotonic() + 20
                while not evaluate(expression):
                    assert time.monotonic() < deadline, (expression, evaluate("document.body.innerText"))
                    time.sleep(0.05)

            wait("document.querySelector('#connection')?.textContent === 'Connected' && document.querySelectorAll('#rows tr').length >= 2")
            assert challenges > 0, "Use the real browser authentication challenge"
            assert evaluate("document.title") == "Asagiri · Local browser"
            assert evaluate("document.querySelector('.change') === null")
            assert prefix in evaluate("document.querySelector('#rows').textContent")
            for view in ("images", "volumes", "networks", "containers"):
                evaluate(f"document.querySelector('[data-view={view}]').click()")
                wait("document.querySelector('#refresh').disabled === false")
                assert evaluate("document.querySelector('#error').hidden"), evaluate("document.querySelector('#error').textContent")
                assert evaluate("document.querySelector('nav [aria-current]').dataset.view") == view
            # Logs contain real container output shaped like active HTML.
            evaluate("Array.from(document.querySelectorAll('#rows tr')).find(r => r.innerText.includes('-first')).querySelectorAll('button')[1].click()")
            wait("document.querySelector('#detail').open")
            assert "<img" in evaluate("document.querySelector('#detail-content').textContent")
            assert evaluate("document.querySelector('#detail-content img') === null && globalThis.browserInjected === undefined")
            evaluate("document.querySelector('#close-detail').click()")
            cli("allow-changes", "on")
            evaluate("document.querySelector('#refresh').click()")
            wait("document.querySelector('#refresh').disabled === false")
            assert evaluate("document.querySelector('.change') !== null")
            evaluate("Array.from(document.querySelectorAll('#rows tr')).find(r => r.innerText.includes('-first')).querySelector('.change').click()")
            wait("document.querySelector('#refresh').disabled === false")
            assert docker("inspect", "-f", "{{.State.Status}}", cid) == "exited"
            evaluate("Array.from(document.querySelectorAll('#rows tr')).find(r => r.innerText.includes('-first')).querySelector('.change').click()")
            wait("document.querySelector('#refresh').disabled === false")
            assert docker("inspect", "-f", "{{.State.Status}}", cid) == "running"
            screenshots = Path(os.environ.get("ASAGIRI_TEST_ARTIFACTS", "/tmp/asagiri-browser-artifacts"))
            screenshots.mkdir(parents=True, exist_ok=True)
            for width, height, name in ((1360, 900, "desktop"), (430, 900, "mobile")):
                call("Emulation.setDeviceMetricsOverride", {"width": width, "height": height, "deviceScaleFactor": 1, "mobile": width < 600})
                assert evaluate("document.documentElement.scrollWidth <= innerWidth")
                if width < 600:
                    assert evaluate("document.querySelector('.table-scroll').scrollWidth > document.querySelector('.table-scroll').clientWidth")
                    assert evaluate("Array.from(document.querySelectorAll('.badge')).every(b => b.getBoundingClientRect().height < 30)")
                    assert evaluate("document.querySelector('#rows tr td:nth-child(2)').getBoundingClientRect().width >= 100")
                image = call("Page.captureScreenshot", {"format": "png", "captureBeyondViewport": False})
                (screenshots / ("browser-" + name + ".png")).write_bytes(base64.b64decode(image["data"]))
            assert not errors, errors
            print("PASS Chromium Basic authentication, real resource views, bounded log rendering, container controls and responsive layouts", flush=True)
        finally:
            if ws is not None:
                ws.close()
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            log.close()


def check(path, image):
    assert sys.platform == "linux" and os.geteuid() == 0 and Path("/.dockerenv").is_file()
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1" and os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
    assert path == "/run/asagiri/docker.sock"
    binary = os.environ.get("ASAGIRI_MCP_CLI", "/opt/asagiri/asagiri")
    environment = dict(os.environ, ASAGIRI_SOCKET=path)
    environment.pop("DOCKER_CONTEXT", None)
    policy = Path.home() / ".asagiri/integrations/web.json"
    assert not policy.exists(), "Use a fresh private engine without existing browser settings"
    prefix = "asagiri-browser-" + uuid.uuid4().hex[:10]
    password = "browser-fixture-" + uuid.uuid4().hex
    secret = "METADATA_SECRET_" + uuid.uuid4().hex
    containers = []
    server = None
    stderr = tempfile.TemporaryFile()

    def cli(*args, input=None, success=True):
        result = subprocess.run([binary, "web", *args], env=environment, capture_output=True,
                                text=True, input=input, timeout=10)
        assert (result.returncode == 0) == success, (args, result.stderr)
        return json.loads(result.stdout) if success else result.stderr

    def docker(*args):
        result = subprocess.run(["docker", "-H", "unix://" + path, *args], env=environment,
                                capture_output=True, text=True, timeout=60)
        assert result.returncode == 0, (args, result.stderr)
        return result.stdout.strip()

    def start(extra=None):
        process = subprocess.Popen([binary, "web"], env={**environment, **(extra or {})},
                                   stdout=subprocess.PIPE, stderr=stderr, text=True)
        try:
            assert select.select([process.stdout], [], [], 10)[0], "Browser server did not report its URL"
            line = process.stdout.readline().strip()
            assert line.startswith("Asagiri browser: http://127.0.0.1:"), line
            return process, API(line.split("Asagiri browser: ", 1)[1], password)
        except BaseException:
            process.terminate()
            process.wait(timeout=10)
            raise

    try:
        assert cli("status") == {"enabled": False, "allowContainerChanges": False, "passwordConfigured": False}
        cli("enable", success=False)
        cli(success=False)
        cli("password", "--password-stdin", input="short\n", success=False)
        cli("password", "--password-stdin", input=password + "\n")
        assert policy.stat().st_mode & 0o777 == 0o600
        assert password not in policy.read_text()
        assert not (policy.parent / "mcp.json").exists()
        server, api = start()
        status, headers, body = api.request(authenticated=False)
        assert status == 401 and headers["www-authenticate"].startswith("Basic "), (status, headers)
        assert b"Room to build" not in body
        assert api.request(headers={"Authorization": "Basic " + base64.b64encode(b"asagiri:wrong password").decode()})[0] == 401
        status, headers, body = api.request()
        assert status == 200 and b"Asagiri" in body
        assert headers["cache-control"] == "no-store" and headers["x-content-type-options"] == "nosniff"
        assert "frame-ancestors 'none'" in headers["content-security-policy"]
        assert "access-control-allow-origin" not in headers
        assert api.request(headers={"Host": "rebinding.example"})[0] == 421
        assert api.request(headers={"Origin": "https://other.example"})[0] == 403
        assert api.request(headers={"Sec-Fetch-Site": "same-site"})[0] == 403
        request = json.dumps({"name": "asagiri_status"}).encode()
        assert api.request("POST", "/api/call", body=request, headers={"Origin": None})[0] == 403
        assert api.request("POST", "/api/call", body=request, headers={"Origin": "http://127.0.0.1:1"})[0] == 403
        assert api.request("POST", "/api/call", body=request, headers={"Content-Type": "text/plain"})[0] == 415
        assert api.request("POST", "/api/call", body=b"x" * (64 * 1024 + 1))[0] == 413
        connection = http.client.HTTPConnection("127.0.0.1", api.port, timeout=10)
        try:
            connection.request("POST", "/api/call", body=iter([b"x" * 32768, b"x" * 32769]),
                               headers={"Authorization": api.authorization, "Origin": api.origin, "Content-Type": "application/json"},
                               encode_chunked=True)
            assert connection.getresponse().status == 413
        finally:
            connection.close()
        assert api.request(path="/api/call")[0] == 405
        assert api.request(path="/containers/json")[0] == 404
        assert api.request(headers={"X-Large": "x" * (32 * 1024)})[0] == 431
        with socket.create_connection(("127.0.0.1", api.port), timeout=10) as slow:
            slow.sendall(f"GET / HTTP/1.1\r\nHost: {api.host}\r\n".encode())
            # No terminating header line: Hyper must close the slow peer
            # using its configured header deadline, not wait indefinitely.
            assert slow.recv(1024) == b""
        with socket.create_connection(("127.0.0.1", api.port), timeout=10) as slow:
            slow.sendall((f"POST /api/call HTTP/1.1\r\nHost: {api.host}\r\nOrigin: {api.origin}\r\n"
                          f"Authorization: {api.authorization}\r\nContent-Type: application/json\r\n"
                          "Content-Length: 100\r\n\r\n{").encode())
            assert slow.recv(4096).startswith(b"HTTP/1.1 408")
        for header, value, expected in (("Host", api.host, 421), ("Origin", api.origin, 403), ("Authorization", api.authorization, 401)):
            connection = http.client.HTTPConnection("127.0.0.1", api.port, timeout=10)
            try:
                connection.putrequest("GET", "/", skip_host=True)
                for key, content in (("Host", api.host), ("Authorization", api.authorization)):
                    connection.putheader(key, content)
                if header == "Origin":
                    connection.putheader(header, value)
                connection.putheader(header, value)
                connection.endheaders()
                assert connection.getresponse().status == expected, header
            finally:
                connection.close()
        tools = json.loads(api.request(path="/api/tools")[2])
        assert len(tools["tools"]) == 7 and not tools["permissions"]["allowContainerChanges"]
        assert api.tool("asagiri_status")["version"]["AsagiriVersion"]
        for suffix in ("first", "second"):
            containers.append(docker("create", "--name", prefix + "-" + suffix,
                "--env", "SECRET=" + secret, "--label", "secret=" + secret, image, "sh", "-c",
                "echo browser-stdout; echo browser-stderr >&2; printf '%s\\n' '<img src=x onerror=globalThis.browserInjected=1>'; sleep 120"))
        listed = api.tool("asagiri_list_containers")
        assert secret not in json.dumps(listed)
        api.tool("asagiri_start_container", {"container": containers[0]}, expected=400)
        assert docker("inspect", "-f", "{{.State.Status}}", containers[0]) == "created"
        api.tool("asagiri_exec", expected=400)
        cli("allow-changes", "on")
        assert len(json.loads(api.request(path="/api/tools")[2])["tools"]) == 10
        api.tool("asagiri_start_container", {"container": containers[0]})
        deadline = time.monotonic() + 5
        while "browser-stdout" not in api.tool("asagiri_container_logs", {"container": containers[0]})["logs"]:
            assert time.monotonic() < deadline
            time.sleep(0.05)
        assert secret not in json.dumps(api.tool("asagiri_inspect_container", {"container": containers[0]}))
        api.tool("asagiri_stop_container", {"container": containers[0], "timeout": 0})
        api.tool("asagiri_restart_container", {"container": containers[0], "timeout": 0})
        cli("allow-changes", "off")
        if chrome := os.environ.get("ASAGIRI_TEST_BROWSER"):
            browser_ui(chrome, api, password, cli, docker, containers[0], prefix)
        cli("disable")
        assert api.request()[0] == 401
        assert cli("status") == {"enabled": False, "allowContainerChanges": False, "passwordConfigured": True}
        cli("enable")
        assert not json.loads(api.request(path="/api/tools")[2])["permissions"]["allowContainerChanges"]
        replacement = "rotated-browser-" + uuid.uuid4().hex
        cli("password", "--password-stdin", input=replacement + "\n")
        assert api.request()[0] == 401
        password = replacement
        assert API(api.origin, password).request()[0] == 200
        server.terminate()
        server.wait(timeout=10)
        server = None
        print("PASS browser denial, password verification/rotation, Host/Origin checks, request limits, independent permissions and real container actions", flush=True)

        with tempfile.TemporaryDirectory(prefix=prefix) as directory:
            fake = str(Path(directory) / "engine.sock")
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(fake)
                listener.listen(8)
                listener.settimeout(10)
                server, api = start({"ASAGIRI_SOCKET": fake})
                peers, calls = [], []
                started = time.monotonic()
                with ThreadPoolExecutor(max_workers=4) as pool:
                    try:
                        for _ in range(4):
                            calls.append(pool.submit(api.tool, "asagiri_status", expected=400))
                            peer, _ = listener.accept()
                            peers.append(peer)
                            peer.settimeout(2)
                            request = b""
                            while not request.endswith(b"\r\n\r\n"):
                                request += peer.recv(2048)
                                assert len(request) <= 16384
                        assert api.request()[0] == 503, "The four-worker bound must include stalled engine calls"
                        for peer in peers[1:]:
                            peer.close()
                        for future in calls[1:]:
                            future.result(timeout=5)
                        result = calls[0].result(timeout=50)
                        assert "timed out" in result["error"], result
                        assert 40 <= time.monotonic() - started < 50
                        assert peers[0].recv(2048) == b""
                        assert api.request()[0] == 200
                    finally:
                        for peer in peers:
                            peer.close()
        print("PASS browser worker bound, stalled engine deadline, connection cleanup and recovery", flush=True)
    finally:
        if server is not None:
            server.terminate()
            server.wait(timeout=10)
        stderr.close()
        cli("disable")
        for cid in containers:
            docker("rm", "-f", cid)
        for filename in ("web.json", "web.lock"):
            file = policy.parent / filename
            if file.exists():
                file.unlink()
        if policy.parent.exists():
            policy.parent.rmdir()


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
