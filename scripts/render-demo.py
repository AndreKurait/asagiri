#!/usr/bin/env python3
"""Regenerate the UI screenshots, demo animation, and social image.

    uv run --with Pillow --with websocket-client python scripts/render-demo.py
    uv run --with websocket-client python scripts/render-demo.py --check

Uses an isolated headless Chrome profile, a loopback-only temporary web
server, and the built app demo. It never starts the native container engine.
CHROME can select a Chromium-compatible browser executable.
"""
import base64
import argparse
import functools
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time
from urllib.request import Request, urlopen

import websocket

ROOT = Path(__file__).resolve().parents[1]
IMAGES = ROOT / "docs/images"
CHROME = os.environ.get("CHROME") or shutil.which("google-chrome") or shutil.which("chromium") or "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"


class QuietHandler(SimpleHTTPRequestHandler):
    def log_message(self, *_):
        pass


class Browser:
    def __init__(self, url):
        self.ws = websocket.create_connection(url, timeout=30, suppress_origin=True)
        self.sequence = 0
        self.errors = []
        self.call("Page.enable")
        self.call("Runtime.enable")

    def call(self, method, **params):
        self.sequence += 1
        self.ws.send(json.dumps({"id": self.sequence, "method": method, "params": params}))
        while True:
            response = json.loads(self.ws.recv())
            if response.get("method") == "Runtime.exceptionThrown":
                self.errors.append(response["params"]["exceptionDetails"])
            if response.get("id") != self.sequence:
                continue
            if "error" in response:
                raise RuntimeError(f"{method}: {response['error']}")
            return response.get("result", {})

    def js(self, expression):
        result = self.call("Runtime.evaluate", expression=expression, awaitPromise=True, returnByValue=True)
        if "exceptionDetails" in result:
            raise RuntimeError(result["exceptionDetails"])
        return result.get("result", {}).get("value")

    def load(self, url, width=1440, height=940):
        self.errors.clear()
        self.call("Emulation.setDeviceMetricsOverride", width=width, height=height, deviceScaleFactor=1, mobile=False)
        self.call("Page.navigate", url=url)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if self.js(f"location.href === {json.dumps(url)} && document.readyState === 'complete'"):
                break
            time.sleep(0.1)
        else:
            raise TimeoutError(url)
        self.js("document.fonts.ready.then(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))))")

    def screenshot(self, path):
        if self.errors:
            raise RuntimeError(self.errors)
        png = self.call("Page.captureScreenshot", format="png", captureBeyondViewport=False)
        path.write_bytes(base64.b64decode(png["data"]))
        print(f"Wrote {path.relative_to(ROOT)}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="check the demo and layouts without rewriting images")
    parser.add_argument("--base-url", help="check an already deployed site (including its project path)")
    args = parser.parse_args()
    check = args.check
    if args.base_url and not check:
        parser.error("--base-url requires --check")
    if not check:
        from PIL import Image
    if not args.base_url:
        subprocess.run(["python3", str(ROOT / "scripts/build-site.py")], check=True)
    if not check:
        IMAGES.mkdir(exist_ok=True)
    server = None
    if args.base_url:
        origin = args.base_url.rstrip("/")
    else:
        server = ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(QuietHandler, directory=str(ROOT / "target/site")))
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        origin = f"http://127.0.0.1:{server.server_port}"
    with tempfile.TemporaryDirectory(prefix="asagiri-demo-chrome-") as profile:
        process = subprocess.Popen([
            CHROME, "--headless=new", "--disable-gpu", "--no-first-run",
            "--no-default-browser-check", "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=0", f"--user-data-dir={profile}", "about:blank",
        ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            portfile = Path(profile) / "DevToolsActivePort"
            deadline = time.monotonic() + 30
            while not portfile.exists():
                if process.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("Chrome did not start its local debugging endpoint")
                time.sleep(0.1)
            port = portfile.read_text().splitlines()[0]
            with urlopen(Request(f"http://127.0.0.1:{port}/json/new?about:blank", method="PUT")) as response:
                target = json.load(response)
            browser = Browser(target["webSocketDebuggerUrl"])
            def capture(name):
                assert not browser.errors, browser.errors
                if not check:
                    browser.screenshot(IMAGES / name)
            frames = []
            for name, query in [
                ("asagiri-window.png", "view=containers&theme=dark"),
                ("asagiri-window-light.png", "view=containers&theme=light"),
                ("asagiri-images.png", "view=images&theme=dark"),
            ]:
                browser.load(f"{origin}/demo/window.html?{query}")
                assert browser.js("window.ASAGIRI_DEMO_READY === true"), "Demo did not initialize"
                assert browser.js("document.querySelectorAll('#results').length === 0"), "Demo page raised an error"
                expected = "light" if "theme=light" in query else "dark"
                assert browser.js("getComputedStyle(document.documentElement).colorScheme") == expected, "Demo theme did not apply"
                capture(name)
                if not check:
                    frame = Image.open(IMAGES / name).convert("RGB")
                    frame.thumbnail((1080, 705))
                    frames.append(frame)
            if not check:
                frames[0].save(IMAGES / "asagiri-demo.gif", save_all=True, append_images=frames[1:], duration=[2600, 2200, 2600], loop=0, optimize=True)
                print("Wrote docs/images/asagiri-demo.gif", flush=True)
            # Exercise the native bridge replacement and state changes.
            browser.load(f"{origin}/demo/window.html")
            browser.js("demoCommand({cmd:'container', id:'a1', action:'stop'})")
            assert browser.js("ui.containers.find(c=>c.Id==='a1').State === 'exited'")
            browser.js("demoCommand({cmd:'container', id:'a1', action:'start'})")
            assert browser.js("ui.containers.find(c=>c.Id==='a1').State === 'running'")
            browser.js("openDetail('a1'); demoInspect('a1')")
            assert browser.js("ui.detail.info.Config.Image === 'nginx:alpine'")
            browser.js("viewContainer('a1', 'logs')")
            browser.js("demoCommand({cmd:'logs', id:'a1'})")
            assert browser.js("logEl.textContent.includes('Ready for your next idea.')")
            browser.js("show('images')")
            assert browser.js("ui.view === 'images' && ui.images.length === 6")
            print("PASS demo navigation, stop/start, inspect, and logs", flush=True)
            browser.js("prefs.tab='mirrors'; show('settings')")
            tabs = browser.js("[...document.querySelectorAll('[data-stab]')].map(e=>e.dataset.stab)")
            for tab in tabs:
                browser.js(f"document.querySelector('[data-stab={tab}]').click()")
                assert browser.js("!!document.querySelector('#content .settings')"), tab
                assert not browser.errors, browser.errors
            browser.js("document.querySelector('[data-stab=mirrors]').click()")
            browser.js("{ const field=document.getElementById('mirror-url'); field.value='https://mirror.example.invalid';"
                       "field.dispatchEvent(new Event('input',{bubbles:true}));"
                       "document.querySelector('[data-mirroradd]').click(); document.querySelector('[data-savemirrors]').click(); }")
            assert browser.js("document.querySelectorAll('[data-mirrordel]').length===1 && document.querySelector('[data-savemirrors]').disabled")
            browser.js("document.querySelector('[data-mirrordel]').click(); document.querySelector('[data-savemirrors]').click()")
            assert browser.js("document.querySelectorAll('[data-mirrordel]').length===0 && document.querySelector('[data-savemirrors]').disabled")
            browser.js("prefs.tab='appearance'; show('settings')")
            browser.js("document.querySelector('[data-theme-pick=light]').click()")
            assert browser.js("getComputedStyle(document.documentElement).colorScheme === 'light'")
            browser.js("document.querySelector('[data-theme-pick=dark]').click()")
            assert browser.js("getComputedStyle(document.documentElement).colorScheme === 'dark'")
            browser.js("{ const lang=document.getElementById('pref-lang'); lang.value='ar'; lang.dispatchEvent(new Event('change',{bubbles:true})); }")
            assert browser.js("document.documentElement.lang==='ar' && document.documentElement.dir==='rtl'")
            browser.js("{ const lang=document.getElementById('pref-lang'); lang.value='en'; lang.dispatchEvent(new Event('change',{bubbles:true})); }")
            assert browser.js("document.documentElement.lang==='en' && document.documentElement.dir==='ltr'")
            print("PASS all demo settings tabs, mirror edits, themes, and Arabic/English language switching", flush=True)
            # Copy the fresh screenshots before capturing the landing page.
            if not check:
                subprocess.run(["python3", str(ROOT / "scripts/build-site.py")], check=True)
            browser.load(origin + "/", 1440, 1100)
            assert browser.js("document.documentElement.scrollWidth <= innerWidth"), "Desktop layout overflows"
            browser.js("document.querySelector('[data-platform=linux]').click()")
            assert "cargo xtask linux" in browser.js("document.querySelector('#build-command').textContent")
            browser.js("document.querySelector('[data-platform=windows]').click()")
            assert "cargo xtask windows-dev" in browser.js("document.querySelector('#build-command').textContent")
            browser.js("document.querySelector('[data-platform=macos]').click()")
            capture("asagiri-site-desktop.png")
            browser.load(origin + "/", 390, 844)
            assert browser.js("document.documentElement.scrollWidth <= innerWidth"), "Mobile layout overflows"
            capture("asagiri-site-mobile.png")
            print("PASS site build tabs and desktop/mobile layout", flush=True)
            browser.load(origin + "/install/", 1440, 1100)
            assert browser.js("document.documentElement.scrollWidth <= innerWidth"), "Desktop install layout overflows"
            browser.js("document.querySelector('[data-os=mac]').click()")
            assert browser.js("!document.getElementById('mac').hidden && document.getElementById('amazon').hidden")
            browser.js("navigator.clipboard.writeText = async text => { window.installCopied = text; }; document.querySelector('[data-copy=mac-command]').click()")
            assert browser.js("window.installCopied") == "curl -fsSL https://andrekurait.github.io/asagiri/install.sh | sh"
            browser.js("document.getElementById('channel').value='preview'; document.getElementById('channel').dispatchEvent(new Event('change'))")
            assert "--channel preview" in browser.js("document.getElementById('mac-command').textContent")
            browser.js("document.querySelector('[data-os=amazon]').click()")
            assert browser.js("document.getElementById('mac').hidden && !document.getElementById('amazon').hidden")
            assert "--channel preview --no-start" in browser.js("document.getElementById('image-command').textContent")
            browser.js("document.getElementById('channel').value='latest'; document.getElementById('channel').dispatchEvent(new Event('change'))")
            if not args.base_url:
                browser.screenshot(ROOT / "target/install-desktop.png")
            browser.load(origin + "/install/", 390, 844)
            assert browser.js("document.documentElement.scrollWidth <= innerWidth"), "Mobile install layout overflows"
            browser.js("document.querySelector('[data-os=amazon]').click()")
            assert browser.js("document.documentElement.scrollWidth <= innerWidth"), "Amazon Linux install layout overflows"
            if not args.base_url:
                browser.screenshot(ROOT / "target/install-mobile.png")
            assert not browser.errors, browser.errors
            print("PASS installer platform/channel selection, copy commands, and desktop/mobile layouts", flush=True)
            browser.load(origin + "/social.html", 1200, 630)
            capture("social-card.png")
            browser.ws.close()
        finally:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            if server:
                server.shutdown()
                server.server_close()
    if not check:
        subprocess.run(["python3", str(ROOT / "scripts/build-site.py")], check=True)


if __name__ == "__main__":
    main()
