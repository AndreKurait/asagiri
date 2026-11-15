#!/usr/bin/env python3
"""Exercise benchmark charts, filters, links and mobile layout in real Chromium."""
import base64
import copy
from datetime import datetime, timedelta, timezone
import functools
from http.server import ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


demo = load("demo", "scripts/render-demo.py")
checks = load("checks", "tests/ci-performance.py")


def data():
    initial, _ = checks.fixture("linux-engine-v3")
    legacy, _ = checks.fixture()
    rows = []
    start = datetime(2026, 11, 14, 12, tzinfo=timezone.utc)
    for index in range(24):
        item = copy.deepcopy(legacy if index == 1 else initial)
        item.update(run_id=123 + index, commit=f"{index + 2:040x}", baseline=f"{index + 1:040x}",
                    measured_at=(start + timedelta(minutes=index)).isoformat())
        item["environment"]["memory_kib"] = 16078420 + (index % 2) * 4
        item["series"] = checks.history.series(item)
        item["metrics"]["run_ms"][1] = 10.5 + index / 20
        if index == 0:
            item["environment"]["cpu_model"] = "<img src=x onerror=globalThis.performanceInjected=1>"
            item["series"] = checks.history.series(item)
        item["regressions"] = checks.history.regressions(item["metrics"], item["limits"])
        item["status"] = "regression" if item["regressions"] else "within-limits"
        rows.append(checks.history.summary(item))
    return {"schema": 2, "repository": checks.history.REPO, "entries": rows}


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0),
        functools.partial(demo.QuietHandler, directory=str(ROOT / "target/site")))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    with tempfile.TemporaryDirectory(prefix="asagiri-performance-chrome-") as folder:
        process = subprocess.Popen([
            demo.CHROME, "--headless=new", "--disable-gpu", "--no-first-run",
            "--no-default-browser-check", "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=0", f"--user-data-dir={folder}", "about:blank",
        ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        browser = None
        try:
            portfile = Path(folder) / "DevToolsActivePort"
            deadline = time.monotonic() + 30
            while not portfile.exists():
                assert process.poll() is None and time.monotonic() < deadline, "Chromium did not start"
                time.sleep(0.1)
            port = portfile.read_text().splitlines()[0]
            from urllib.request import urlopen
            with urlopen(f"http://127.0.0.1:{port}/json", timeout=15) as response:
                pages = json.load(response)
            page = next(value for value in pages if value["type"] == "page")
            browser = demo.Browser(page["webSocketDebuggerUrl"])
            url = f"http://127.0.0.1:{server.server_port}/performance/"
            browser.load(url)
            browser.js("(async () => { globalThis.historyTest = await import('./history.js'); await historyTest.loaded; })()")
            browser.js('historyTest.renderHistory({schema:1,repository:"AndreKurait/asagiri",entries:[]})')
            assert browser.js("document.getElementById('performance').hidden")
            browser.js(f"historyTest.renderHistory({json.dumps(data())})")
            assert browser.js("document.querySelectorAll('#plot .point').length") == 22
            assert browser.js("document.querySelectorAll('#measurements tr').length") == 22
            assert browser.js("document.querySelectorAll('#series option').length") == 3
            assert browser.js("document.getElementById('sample-host').textContent.includes('4 CPUs')")
            assert browser.js("document.querySelector('#series').textContent.includes('<img')")
            assert browser.js("document.querySelector('.performance-controls img') === null && globalThis.performanceInjected === undefined")
            for metric in checks.history.metric_names("linux-engine-v3"):
                browser.js(f"document.getElementById('metric').value={json.dumps(metric)}; document.getElementById('metric').dispatchEvent(new Event('change'))")
                assert browser.js("Array.from(document.querySelectorAll('#plot .point')).every(p => Number.isFinite(Number(p.getAttribute('cy'))))")
            browser.js("document.getElementById('measurement').value='change'; document.getElementById('measurement').dispatchEvent(new Event('change'))")
            assert browser.js("document.getElementById('plot-unit').textContent.startsWith('%')")
            browser.js("document.querySelector('#plot .point').focus()")
            assert browser.js("document.querySelector('#measurements tr.selected') !== null")
            browser.js("document.getElementById('series').selectedIndex=1; document.getElementById('series').dispatchEvent(new Event('change'))")
            assert browser.js("document.querySelectorAll('#plot .point').length") == 1
            assert browser.js("document.getElementById('metric').value") == "run_ms"
            assert browser.js("document.querySelectorAll('#metric option:disabled').length") == 4
            assert browser.js("Number(document.querySelector('#plot .point').getAttribute('cx')) > 0 && Number(document.querySelector('#plot .point').getAttribute('cx')) < document.querySelector('#plot').viewBox.baseVal.width")
            browser.js("document.getElementById('series').selectedIndex=0; document.getElementById('series').dispatchEvent(new Event('change'))")
            browser.js("document.getElementById('metric').value='run_ms'; document.getElementById('measurement').value='head'; document.getElementById('metric').dispatchEvent(new Event('change'))")
            assert browser.js("""Array.from(document.querySelectorAll('#measurements a')).every(a =>
              /^https:\\/\\/github.com\\/AndreKurait\\/asagiri\\/(commit\\/[a-f0-9]{40}|actions\\/runs\\/\\d+\\/attempts\\/\\d+)$/.test(a.href))""")
            bad = data()
            bad["entries"][0]["path"] = "../../outside"
            assert browser.js(f"(() => {{ try {{ historyTest.validateHistory({json.dumps(bad)}); return false; }} catch {{ return true; }} }})()")
            bad = data()
            bad["entries"][0]["memory_mib"] = -1
            assert browser.js(f"(() => {{ try {{ historyTest.validateHistory({json.dumps(bad)}); return false; }} catch {{ return true; }} }})()")
            bad = data()
            bad["entries"][0]["metrics"].pop("copy_ms")
            assert browser.js(f"(() => {{ try {{ historyTest.validateHistory({json.dumps(bad)}); return false; }} catch {{ return true; }} }})()")
            for width, height, name in ((1440, 1050, "desktop"), (430, 960, "mobile")):
                browser.call("Emulation.setDeviceMetricsOverride", width=width, height=height,
                             deviceScaleFactor=1, mobile=width < 600)
                browser.js("new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))")
                browser.js("scrollTo(0, 0)")
                assert browser.js("document.documentElement.scrollWidth <= innerWidth")
                assert browser.js("Math.abs(document.querySelector('#plot').viewBox.baseVal.width - document.querySelector('#plot').getBoundingClientRect().width) < 1")
                if width < 600:
                    assert browser.js("document.querySelector('.performance-table').scrollWidth > document.querySelector('.performance-table').clientWidth")
                if os.environ.get("PERF_SCREENSHOTS"):
                    destination = Path(os.environ["PERF_SCREENSHOTS"])
                    destination.mkdir(parents=True, exist_ok=True)
                    image = browser.call("Page.captureScreenshot", format="png", captureBeyondViewport=False)
                    (destination / (name + ".png")).write_bytes(base64.b64decode(image["data"]))
            assert not browser.errors, browser.errors
            print("PASS Chromium benchmark plots, 15 metrics, mixed workload history, paired changes, safe labels, source links and mobile layout")
        finally:
            if browser is not None:
                browser.ws.close()
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=10)
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
