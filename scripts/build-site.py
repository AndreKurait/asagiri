#!/usr/bin/env python3
"""Build the static site and its app demo into target/site (no network needed).

The demo uses the same HTML, icons, terminal and translations as the native
app, with site/demo/fixture.js supplying clearly labelled sample data.
"""
import argparse
import importlib.util
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parents[1]


def demo_page():
    spec = importlib.util.spec_from_file_location("ui_test", ROOT / "scripts/ui-test.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    page = module.page(str(ROOT / "site/demo/fixture.js"))
    # ui-test's fixture seeds sharing and installs a recorder. The demo
    # fixture replaces that data and implements the browser bridge.
    test_fixture = (ROOT / "tests/ui/fixture.js").read_text()
    page = page.replace(test_fixture, "const sent = []; window.webkit = { messageHandlers: { asagiri: { postMessage() {} } } };", 1)
    return page


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=ROOT / "target/site")
    args = parser.parse_args()
    out = args.out.resolve()
    if out == ROOT or ROOT in out.parents and not (ROOT / "target") in out.parents:
        parser.error("choose a build directory under target/ or outside the repository")
    out.mkdir(parents=True, exist_ok=True)
    shutil.copytree(ROOT / "site", out, dirs_exist_ok=True, ignore=shutil.ignore_patterns("fixture.js"))
    assets = out / "assets"
    assets.mkdir(exist_ok=True)
    for path in (ROOT / "brand/logo").glob("*.svg"):
        shutil.copy2(path, assets / path.name)
    shutil.copy2(ROOT / "brand/logo/asagiri-favicon.svg", assets / "favicon.svg")
    for path in (ROOT / "docs/images").iterdir():
        if path.is_file():
            shutil.copy2(path, assets / path.name)
    (out / "demo/window.html").write_text(demo_page())
    (out / ".nojekyll").write_text("")
    print(f"Built website and interactive demo: {out}")


if __name__ == "__main__":
    main()
