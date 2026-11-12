#!/usr/bin/env python3
"""Wait for the deployed commit, then verify public pages and their assets."""
import argparse
from html.parser import HTMLParser
import json
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urljoin, urlsplit
from urllib.request import Request, urlopen


class Assets(HTMLParser):
    def __init__(self):
        super().__init__()
        self.urls = []

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag in ("img", "script", "iframe") and attrs.get("src"):
            self.urls.append(attrs["src"])
        if tag == "link" and attrs.get("href"):
            self.urls.append(attrs["href"])


def fetch(url):
    request = Request(url, headers={"User-Agent": "asagiri-pages-verifier", "Cache-Control": "no-cache"})
    with urlopen(request, timeout=30) as response:
        if response.status != 200:
            raise RuntimeError(f"{url}: HTTP {response.status}")
        return response.read()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base_url")
    parser.add_argument("commit")
    parser.add_argument("--timeout", type=int, default=300)
    args = parser.parse_args()
    base = args.base_url.rstrip("/") + "/"
    if urlsplit(base).scheme != "https":
        parser.error("the deployed site must use HTTPS")
    deadline = time.monotonic() + args.timeout
    while True:
        try:
            info = json.loads(fetch(urljoin(base, f"deployment.json?commit={args.commit}")))
            if info.get("commit") == args.commit:
                break
        except (HTTPError, URLError, TimeoutError, ValueError):
            pass
        if time.monotonic() >= deadline:
            raise SystemExit(f"Pages did not serve commit {args.commit} within {args.timeout}s")
        time.sleep(5)
    urls = set()
    for path in ("", "demo/", "demo/window.html"):
        url = urljoin(base, path)
        html = fetch(url).decode()
        if "Asagiri" not in html:
            raise SystemExit(f"{url}: missing Asagiri content")
        assets = Assets()
        assets.feed(html)
        for asset in assets.urls:
            target = urljoin(url, asset)
            if urlsplit(target).netloc == urlsplit(base).netloc:
                if not target.startswith(base):
                    raise SystemExit(f"{url}: asset escapes the project path: {asset}")
                urls.add(target)
    for url in sorted(urls):
        if not fetch(url):
            raise SystemExit(f"{url}: empty asset")
    print(f"PASS HTTPS, commit {args.commit}, 3 public pages, and {len(urls)} local assets")


if __name__ == "__main__":
    main()
