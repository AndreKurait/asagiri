#!/usr/bin/env python3
"""Wait for the deployed commit, then verify public pages and their assets."""
import argparse
import hashlib
from html.parser import HTMLParser
import json
from pathlib import Path
import re
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urljoin, urlsplit
from urllib.request import Request, urlopen
import xml.etree.ElementTree as ET


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


def available(url):
    request = Request(url, method="HEAD", headers={"User-Agent": "asagiri-pages-verifier"})
    with urlopen(request, timeout=30) as response:
        if response.status != 200:
            raise RuntimeError(f"{url}: HTTP {response.status}")


def verify_downloads(base):
    root = Path(__file__).resolve().parents[2]
    installer = fetch(urljoin(base, "install.sh")).decode()
    key = fetch(urljoin(base, "keys/asagiri-rpm.asc"))
    if key != (root / "packaging/keys/asagiri-rpm.asc").read_bytes():
        raise SystemExit("The deployed RPM key differs from the reviewed source")
    if "key_sha256=" + hashlib.sha256(key).hexdigest() not in installer.splitlines():
        raise SystemExit("The deployed installer does not pin the deployed RPM key")
    index = json.loads(fetch(urljoin(base, "releases.json")))
    if index["repository"] != "AndreKurait/asagiri" or set(index["channels"]) != {"latest", "stable", "preview"}:
        raise SystemExit("Invalid public download index")
    checked = set()
    for channel, release in index["channels"].items():
        if release is None:
            continue
        version = release["version"]
        if (not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-dev\.[1-9]\d*)?", version)
                or not re.fullmatch(r"[a-f0-9]{40}", release["commit"])
                or release["tag"] != "v" + version):
            raise SystemExit("Invalid public release identity")
        release_url = f"https://github.com/AndreKurait/asagiri/releases/tag/v{version}"
        if release["release_url"] != release_url:
            raise SystemExit("The download index links to another release")
        for arch in ("arm64", "x86_64"):
            item = release["downloads"]["macos-" + arch]
            prefix = f"https://github.com/AndreKurait/asagiri/releases/download/v{version}/Asagiri-{version}-{arch}"
            if (item["url"] != prefix + ".dmg" or item["archive_url"] != prefix + ".tar.gz"
                    or any(not re.fullmatch(r"[a-f0-9]{64}", item[field])
                           for field in ("sha256", "archive_sha256"))):
                raise SystemExit("Invalid Mac download URL or digest")
            text = fetch(urljoin(base, f"downloads/{channel}/macos-{arch}.txt")).decode()
            expected = f"version {version}\nurl {item['archive_url']}\nsha256 {item['archive_sha256']}\n"
            if text != expected:
                raise SystemExit("The command installer and download page select different Mac builds")
            for url in (item["url"], item["archive_url"]):
                if url not in checked:
                    available(url)
                    checked.add(url)
        for arch in ("x86_64", "aarch64"):
            repository = urljoin(base, f"rpm/{channel}/{arch}/")
            metadata = ET.fromstring(fetch(urljoin(repository, "repodata/repomd.xml")))
            namespace = {"r": "http://linux.duke.edu/metadata/repo"}
            entries = metadata.findall("r:data", namespace)
            if not entries:
                raise SystemExit("The RPM repository has no package metadata")
            for entry in entries:
                location = entry.find("r:location", namespace).attrib["href"]
                url = urljoin(repository, location)
                if not url.startswith(repository + "repodata/"):
                    raise SystemExit("RPM metadata escapes its repository")
                checksum = entry.find("r:checksum", namespace)
                if checksum.attrib["type"] != "sha256" or hashlib.sha256(fetch(url)).hexdigest() != checksum.text:
                    raise SystemExit("Published RPM metadata failed its checksum verification")
            provenance = json.loads(fetch(urljoin(repository, "provenance.json")))
            fingerprint = (root / "packaging/keys/asagiri-rpm.fingerprint").read_text().strip()
            if (provenance["source"] != release["commit"] or provenance["release"] != release_url
                    or provenance["rpm_signing_fingerprint"] != fingerprint):
                raise SystemExit("The RPM repository does not identify the selected source and key")
            rpm_version = version.replace("-", "~", 1)
            expected_names = {f"{name}-{rpm_version}-1.{arch}.rpm" for name in ("asagiri", "asagiri-lite")}
            packages = {item["name"]: item for item in provenance["packages"]}
            if set(packages) != expected_names:
                raise SystemExit("The RPM repository is missing a package")
            for name, package in packages.items():
                if any(not re.fullmatch(r"[a-f0-9]{64}", package[field])
                       for field in ("sha256", "original_release_sha256")):
                    raise SystemExit("Invalid RPM provenance digest")
                available(urljoin(repository, "Packages/" + name))
            name = f"asagiri-{rpm_version}-1.{arch}.rpm"
            item = release["downloads"]["linux-" + arch]
            if (item["url"] != urljoin(repository, "Packages/" + name)
                    or item["sha256"] != packages[name]["sha256"]):
                raise SystemExit("The download index differs from the signed RPM repository")
    print("PASS installer, pinned RPM key, release index, and all available download channels")


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
    for path in ("", "install/", "demo/", "demo/window.html"):
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
    verify_downloads(base)
    print(f"PASS HTTPS, commit {args.commit}, 4 public pages, and {len(urls)} local assets")


if __name__ == "__main__":
    main()
