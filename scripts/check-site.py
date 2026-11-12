#!/usr/bin/env python3
"""Check the built site's local links, assets and demo payload."""
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1] / "target/site"


class Links(HTMLParser):
    def __init__(self):
        super().__init__()
        self.urls = []

    def handle_starttag(self, tag, attrs):
        values = dict(attrs)
        for attr in ("href", "src"):
            if values.get(attr):
                self.urls.append(values[attr])


errors = []
pages = list(ROOT.rglob("*.html"))
for page in pages:
    parser = Links()
    parser.feed(page.read_text())
    for url in parser.urls:
        parts = urlsplit(url)
        if parts.scheme or parts.netloc or not parts.path:
            continue
        target = (page.parent / unquote(parts.path)).resolve()
        if not target.is_relative_to(ROOT):
            errors.append(f"{page.relative_to(ROOT)}: link escapes site: {url}")
        elif not target.exists():
            errors.append(f"{page.relative_to(ROOT)}: missing {url}")
        elif target.is_dir() and not (target / "index.html").is_file():
            errors.append(f"{page.relative_to(ROOT)}: directory has no index: {url}")
demo = (ROOT / "demo/window.html").read_text()
for required in ("ASAGIRI_DEMO_READY", "demoCommand", "asagiri.on", "sample data"):
    if required not in demo:
        errors.append(f"demo payload missing {required}")
if errors:
    raise SystemExit("\n".join(errors))
print(f"PASS local links and assets in {len(pages)} pages; interactive demo is assembled")
