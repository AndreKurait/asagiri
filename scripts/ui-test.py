#!/usr/bin/env python3
"""Runs the window's page tests (tests/ui/*.js) in headless Chrome.

Each test file runs in the real page (crates/asagiri/src/ui/index.html, with its
icons) after tests/ui/fixture.js, and calls report() with lines that start
with PASS or FAIL. Exits non-zero if any test fails or a page errors. A
test's first lines may say `// parity: macos windows linux` (run as each
system and compare), `// lang: de` (the language it opens in), `// window:
760x480` (the window's size) or `// page: setup` (Windows' setup window,
crates/asagiri-win/src/setup.html, instead of the main window).

    python3 scripts/ui-test.py [tests/ui/switch.js ...]
"""
import html, json, os, re, subprocess, sys, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CHROME = os.environ.get("CHROME", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
UI = os.path.join(ROOT, "crates/asagiri/src/ui")


# The results, in the page; and to the page around it when a test runs in a
# frame of a given size (`// window:`).
REPORT = ('const report = (lines) => { if (window.parent !== window) parent.postMessage(lines.join("\\n"), "*");'
          ' document.body.insertAdjacentHTML("beforeend", `<pre id="results">${lines.join("\\n")}</pre>`); };')
CATCH = ('<script>window.onerror = (m, u, l) => { const s = `FAIL page error: ${m} (line ${l})`; if (window.parent !== window) parent.postMessage(s, "*");'
         ' document.body.insertAdjacentHTML("beforeend", `<pre id="results">${s}</pre>`); };</script>')


def strings():
    """lang/*.json packed as lang.rs packs them: the English keys once, each language its words in that order."""
    d = os.path.join(UI, "lang")
    files = {f[:-5]: json.load(open(os.path.join(d, f))) for f in sorted(os.listdir(d)) if f.endswith(".json")}
    keys = sorted(set().union(*files.values()))
    return {"en": keys, **{tag: [m.get(k) for k in keys] for tag, m in files.items()}}


def opening(pick, packed):
    """What assemble.rs splices in for `pick` on an en-US system: the keys and
    the one language the page opens in (a file's tag, or English)."""
    return {"en": packed["en"], **({pick: packed[pick]} if pick in packed and pick != "en" else {})}


def host(packed):
    """The host's half of langwords, after the fixture's recorder: it answers as
    lang.rs's words_js does, calling asagiriWords with that language's words, at
    once (a real host answers a moment later). hostHold = true keeps the answers
    until hostFlush(), for a test of the moment in between."""
    words = {t: w for t, w in packed.items() if t != "en"}
    return (f"const HOST_WORDS = {json.dumps(words, ensure_ascii=False)};\n"
            "let hostHold = false; const hostHeld = [];\n"
            "const hostFlush = () => hostHeld.splice(0).forEach((f) => f());\n"
            "{ const post = window.webkit.messageHandlers.asagiri.postMessage;\n"
            "  window.webkit.messageHandlers.asagiri.postMessage = (m) => { post(m); const o = JSON.parse(m);\n"
            "    if (o.cmd !== \"langwords\" || !HOST_WORDS[o.name]) return;\n"
            "    const answer = () => window.asagiriWords && asagiriWords({ [o.name]: HOST_WORDS[o.name] });\n"
            "    if (hostHold) hostHeld.push(answer); else answer(); }; }\n")


def header(test, name, default=None):
    """`// name: value` in one of a test's first lines."""
    for line in open(test).readlines()[:4]:
        if line.startswith(f"// {name}:"):
            return line.split(":", 1)[1].strip()
    return default


def lang_of(test):
    """A test runs in English, as a system set to US English would, unless
    one of its first lines says `// lang: zh-Hans`."""
    return header(test, "lang", "system")


def setup_page(test):
    """asagiri-win's setup window (crates/asagiri-win/src/setup.html), for a test whose
    first lines say `// page: setup`: as assemble.rs's words() builds it, with
    the language step and the words for the language it opens in. `sent`
    records what the page asks the host; host() answers langwords."""
    packed, pick = strings(), lang_of(test)
    src = (open(os.path.join(ROOT, "crates/asagiri-win/src/setup.html")).read()
           .replace("/*LANGSTEP*/", open(os.path.join(UI, "langstep.js")).read(), 1)
           .replace("/*LANG*/", f"window.ASAGIRI_LANG = {json.dumps({'os': ['en-US'], 'pick': pick})};", 1)
           .replace("/*STRINGS*/{}", json.dumps(opening(pick, packed), ensure_ascii=False), 1))
    # The page draws a host state on the next frame; headless Chrome's frames
    # don't keep pace with --virtual-time-budget (the dump can come first), so
    # a frame here is a task.
    fixture = ('window.requestAnimationFrame = (f) => setTimeout(() => f(performance.now()), 0);\n'
               'const sent = [];\n'
               'window.webkit = { messageHandlers: { asagiri: { postMessage: (m) => { const o = JSON.parse(m); sent.push({ type: o.cmd, ...o }); } } } };\n')
    i = src.index("<script>")
    src = src[:i] + CATCH + src[i:]
    return src[: src.rindex("</body>")] + f"<script>{REPORT}\n{fixture}\n{host(packed)}\n{open(test).read()}</script></body></html>"


def page(test, os_name=None):
    # As assemble.rs builds it: the icons and xterm.js spliced in, and the words
    # for the language it opens in; the rest come through langwords (host()).
    packed, pick = strings(), lang_of(test)
    xterm = lambda f: open(os.path.join(UI, "xterm", f)).read()
    index = (open(os.path.join(UI, "index.html")).read().replace("/*ICONS*/", open(os.path.join(UI, "icons.js")).read(), 1)
             .replace("/*LANGSTEP*/", open(os.path.join(UI, "langstep.js")).read(), 1)
             .replace("/*XTERM_CSS*/", xterm("xterm.css"), 1)
             .replace("/*XTERM_JS*/", "".join(xterm(f) + "\n" for f in ["xterm.js", "addon-fit.js", "addon-search.js", "addon-web-links.js"]), 1)
             .replace("/*LANG*/", f"window.ASAGIRI_LANG = {json.dumps({'os': ['en-US'], 'pick': pick})};", 1)
             .replace("/*STRINGS*/{}", json.dumps(opening(pick, packed), ensure_ascii=False), 1))
    if os_name and os_name != "macos":
        index = index.replace("<head>", f'<head><script>window.ASAGIRI_OS = "{os_name}";</script>', 1)
    fixture = open(os.path.join(ROOT, "tests/ui/fixture.js")).read()
    return index[: index.rindex("</body>")] + CATCH + f"<script>{REPORT}\n{fixture}\n{host(packed)}\n{open(test).read()}</script></body></html>"


def run(test, os_name=None):
    first = open(test).readline()
    if first.startswith("// parity:") and os_name is None:
        return parity(test, first.split(":", 1)[1].split())
    with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False) as f:
        f.write(setup_page(test) if header(test, "page") == "setup" else page(test, os_name))
    # `// window: 760x480`: the page in a frame that size (headless Chrome's
    # window is wider than 500 and has its own bar), which passes the results out.
    size, load = header(test, "window"), f.name
    if size:
        w, h = size.split("x")
        with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False) as fr:
            fr.write(f'<!doctype html><body style="margin:0"><iframe src="file://{f.name}" style="border:0;display:block;width:{w}px;height:{h}px"></iframe>'
                     '<script>addEventListener("message", (e) => { const p = document.createElement("pre"); p.id = "results"; p.textContent = e.data; document.body.append(p); });</script></body>')
        load = fr.name
    try:
        with tempfile.TemporaryDirectory(prefix="asagiri-ui-chrome-") as profile:
            dom = subprocess.run([CHROME, "--headless=new", "--disable-gpu", "--no-first-run",
                                  "--no-default-browser-check", f"--user-data-dir={profile}",
                                  "--virtual-time-budget=3000", "--window-size=1400,1000",
                                  "--dump-dom", f"file://{load}"],
                                 capture_output=True, text=True, timeout=120).stdout
    finally:
        os.unlink(f.name)
        if load != f.name: os.unlink(load)
    found = re.findall(r'<pre id="results">(.*?)</pre>', dom, re.S)
    return html.unescape(found[-1]).splitlines() if found else ["FAIL no results: the test didn't run"]


def parity(test, systems):
    """A feature ships on every OS at once: the page's ITEM lines must match
    across `systems`, except items tests/ui/parity-allow.txt gives a reason for."""
    allow = {}
    for line in open(os.path.join(ROOT, "tests/ui/parity-allow.txt")):
        if line.strip() and not line.startswith("#"):
            item, _, why = line.partition(" | ")
            allow[item.strip()] = why.strip()
    have, out = {}, []
    for s in systems:
        lines = run(test, s)
        out += [f"{x} ({s})" for x in lines if x.startswith("FAIL")]
        have[s] = {x[5:] for x in lines if x.startswith("ITEM ")}
    every = set().union(*have.values())
    for item in sorted(every):
        missing = [s for s in systems if item not in have[s]]
        if not missing:
            continue
        if item in allow:
            out.append(f"PASS {item}: not on {', '.join(missing)} ({allow[item]})")
        else:
            out.append(f"FAIL {item}: missing on {', '.join(missing)}. Ship it there too, or add it to tests/ui/parity-allow.txt with the reason")
    out += [f"FAIL {item}: allowed in parity-allow.txt but on every OS now; remove the line" for item in sorted(allow) if all(item in have[s] for s in systems)]
    out.append(f"PASS {len(every)} items checked on {', '.join(systems)}")
    return out


def main():
    tests = sys.argv[1:] or sorted(os.path.join(ROOT, "tests/ui", f) for f in os.listdir(os.path.join(ROOT, "tests/ui")) if f.endswith(".js") and f != "fixture.js")
    failed = 0
    for test in tests:
        print(os.path.relpath(test, ROOT))
        for line in run(test):
            print("  " + line)
            failed += line.startswith("FAIL")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
