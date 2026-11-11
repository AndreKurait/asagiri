#!/usr/bin/env python3
"""Port an upstream commit by comparing its two rebranded trees.

Usage: scripts/port-upstream.sh [--no-commit] COMMIT

Fetch the source commit first. A clean index and working tree are required.
Conflicts are left for review; no reset, checkout or push is performed.
"""
import argparse
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile


def git(root, *args, input=None, check=True):
    return subprocess.run(
        ["git", "-C", str(root), *args], input=input,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=check,
    )


def text(root, *args):
    return git(root, *args).stdout.decode().strip()


def tree(source, revision, stage, rename):
    # Archives contain only the requested commit, never ignored work files.
    archive = git(source, "archive", "--format=tar", revision).stdout
    import io
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        tar.extractall(stage, filter="data")
    git(stage, "add", "-A")
    subprocess.run([sys.executable, str(rename), str(stage)], check=True)
    git(stage, "add", "-A")
    return text(stage, "write-tree")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-commit", action="store_true", help="stage the port for review without committing")
    parser.add_argument("commit", help="one reviewed, non-merge upstream commit")
    args = parser.parse_args()
    root = Path(text(Path.cwd(), "rev-parse", "--show-toplevel"))
    for flags in [("diff", "--quiet"), ("diff", "--cached", "--quiet")]:
        if git(root, *flags, check=False).returncode:
            parser.error("commit or stash tracked changes before porting")
    commit = text(root, "rev-parse", "--verify", f"{args.commit}^{{commit}}")
    parents = text(root, "rev-list", "--parents", "-n", "1", commit).split()
    if len(parents) != 2:
        parser.error("port individual non-merge commits, in their original order")
    rename = root / "scripts/rebrand.py"
    with tempfile.TemporaryDirectory(prefix="asagiri-port-") as temp:
        stage = Path(temp)
        git(stage, "init", "-q")
        before = tree(root, parents[1], stage, rename)
        for entry in stage.iterdir():
            if entry.name == ".git":
                continue
            if entry.is_dir() and not entry.is_symlink():
                shutil.rmtree(entry)
            else:
                entry.unlink()
        after = tree(root, commit, stage, rename)
        patch = git(stage, "diff", "--binary", "--full-index", before, after, "--").stdout
        if not patch:
            print("No changes after rebranding.")
            return 0
        # Supply the transformed base blobs for git apply's three-way merge.
        # Only public source objects enter the destination's object store.
        objects = set()
        for row in text(stage, "diff", "--raw", "--no-abbrev", "--no-renames", before, after).splitlines():
            for oid in row.split()[2:4]:
                if set(oid) != {"0"}:
                    objects.add(oid)
        pack = git(stage, "pack-objects", "--stdout", input=("\n".join(sorted(objects)) + "\n").encode()).stdout
        git(root, "index-pack", "--stdin", input=pack)
        applied = git(root, "apply", "--3way", "--index", "-", input=patch, check=False)
        if applied.returncode:
            sys.stderr.write(applied.stderr.decode())
            print(f"Resolve the conflicts and commit with Upstream-Commit: {commit}.", file=sys.stderr)
            return applied.returncode
    if git(root, "diff", "--cached", "--quiet", check=False).returncode == 0:
        print("This change is already present.")
        return 0
    author = text(root, "show", "-s", "--format=%an <%ae>", commit)
    subject = text(root, "show", "-s", "--format=%s", commit)
    message = (f"port: {subject}\n\n"
               f"Upstream-Repository: https://github.com/libracoder/kiri\n"
               f"Upstream-Commit: {commit}\n"
               f"Co-authored-by: {author}\n")
    if args.no_commit:
        print(f"Staged {commit[:12]} by {author}. Review git diff --cached before committing.\n{message}")
    else:
        result = git(root, "commit", "-S", "-s", "--author", author, "-F", "-", input=message.encode(), check=False)
        sys.stdout.write(result.stdout.decode())
        sys.stderr.write(result.stderr.decode())
        return result.returncode
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as exc:
        sys.stderr.write(exc.stderr.decode() if exc.stderr else str(exc))
        raise SystemExit(exc.returncode)
