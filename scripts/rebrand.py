#!/usr/bin/env python3
"""rebrand.py [--dry-run] [ROOT]

Rewrites a Kiri source tree as Asagiri: the name in every tracked text file, and in file and folder names.
The same input always gives the same output, so an upstream commit can be carried over by running this on
the tree before it and the tree after it and applying the difference (see scripts/port-upstream.sh).

Left as they are:
- upstream's own addresses (runkiri.com, runkiri.cloud, github.com/libracoder/kiri): they are not ours;
- upstream's other addresses (kiri.canestack.com, kiri@canestack.com);
- Indonesian words that only contain the letters ("kirim", "dikirim", "terkirim"), in the Indonesian
  translation alone: elsewhere "kirim" is a name of ours (a macvlan link);
- LICENSE, this script, and the files listed in KEEP.
"""
import os
import re
import subprocess
import sys

KEEP = {
    "LICENSE", "NOTICE", "README.md", "README.pt-BR.md", "CONTRIBUTING.md",
    "RELEASES.md", "SECURITY.md", "CODE_OF_CONDUCT.md", "UPSTREAM.md",
    "packaging/README.md", "docs/sharing.md", "brand/generate.py",
    "scripts/rebrand.py", "scripts/port-upstream.sh", "scripts/port_upstream.py",
}
# Text that must come through unchanged, each replaced by a marker while the names are rewritten.
PROTECT = re.compile(r"runkiri|libracoder/kiri|kiri(?=[.@]canestack)", re.IGNORECASE)
INDONESIAN = re.compile(r"runkiri|libracoder/kiri|kiri(?=[.@]canestack)|[A-Za-z]*kirim\b", re.IGNORECASE)
NAMES = (("KIRI", "ASAGIRI"), ("Kiri", "Asagiri"), ("kiri", "asagiri"))


def rebrand(text, path=""):
    held = []
    protect = INDONESIAN if path.endswith("/lang/id.json") else PROTECT

    def hold(m):
        held.append(m.group(0))
        return "\x00%d\x00" % (len(held) - 1)

    text = protect.sub(hold, text)
    for old, new in NAMES:
        text = text.replace(old, new)
    return re.sub("\x00(\\d+)\x00", lambda m: held[int(m.group(1))], text)


def git(root, *args):
    return subprocess.run(["git", "-C", root, *args], check=True, capture_output=True).stdout


def main():
    args = [a for a in sys.argv[1:] if a != "--dry-run"]
    dry = "--dry-run" in sys.argv[1:]
    root = args[0] if args else "."
    files = [f for f in git(root, "ls-files", "-z").decode().split("\0") if f]
    changed = moved = 0
    moved_paths = []
    for f in files:
        if f in KEEP or f.startswith(("docs/releases/", "bench/", "site/")):
            continue
        path = f"{root}/{f}"
        try:
            with open(path, "rb") as fh:
                raw = fh.read()
        except (FileNotFoundError, IsADirectoryError):
            continue  # a link that leads nowhere, or a submodule
        if b"\0" not in raw[:8192]:
            try:
                old = raw.decode("utf-8")
            except UnicodeDecodeError:
                old = None
            if old is not None:
                new = rebrand(old, f)
                if new != old:
                    changed += 1
                    if not dry:
                        with open(path, "wb") as fh:
                            fh.write(new.encode("utf-8"))
        to = rebrand(f)
        if to != f:
            moved += 1
            if dry:
                print(f"{f} -> {to}")
            else:
                destination = f"{root}/{to}"
                if os.path.lexists(destination):
                    raise RuntimeError(f"refusing to replace an existing path: {to}")
                os.makedirs(destination.rsplit("/", 1)[0], exist_ok=True)
                os.rename(path, destination)
                moved_paths.extend((f, to))
                try:  # the folders the file was in, where it was the last thing in them
                    os.removedirs(f"{root}/{f}".rsplit("/", 1)[0])
                except OSError:
                    pass
    if moved_paths:
        # Stage all renames in one call instead of spawning git per file.
        subprocess.run(
            ["git", "-C", root, "add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"],
            input=("\0".join(moved_paths) + "\0").encode(), check=True,
        )
    print(f"{changed} files rewritten, {moved} renamed", file=sys.stderr)


if __name__ == "__main__":
    main()
