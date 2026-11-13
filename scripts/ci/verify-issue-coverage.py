#!/usr/bin/env python3
"""Check every audited issue has explicit test coverage or a stated gap."""
import argparse
from collections import Counter
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true", help="refresh the readable inventory")
    args = parser.parse_args()
    audit = json.loads((ROOT / "docs/upstream-issue-audit.json").read_text())
    inventory = json.loads((ROOT / "docs/issue-verification.json").read_text())
    expected = {item["number"]: item for item in audit["issues"]}
    rows = inventory["issues"]
    assert len(rows) == len(expected) == audit["count"], "Missing or duplicate issue coverage"
    assert {row["issue"] for row in rows} == set(expected), "Issue inventory differs from upstream audit"
    statuses = {"automated", "documentation", "partial", "platform-gap",
                "known-limitation", "deferred-feature", "informational"}
    for row in rows:
        issue = expected[row["issue"]]
        assert row["body_sha256"] == issue["body_sha256"], f"Re-review changed issue #{row['issue']}"
        assert row["coverage"] in statuses
        assert row["title"] == issue["title"]
        if row["coverage"] in {"automated", "documentation"}:
            assert row["checks"], f"Missing check for #{row['issue']}"
        else:
            assert row["remaining"], f"Missing verification gap for #{row['issue']}"
        for check in row["checks"]:
            path = (ROOT / check).resolve()
            assert path.is_relative_to(ROOT) and path.is_file(), f"Missing check: {check}"

    text = """# Verification of upstream issue reports

Every audited issue is listed below, including closed reports and feature requests.
**Automated** means a reproducible check exists; it is not a claim that every
platform has passed it. **Partial** and **platform-gap** explicitly retain work
that has not been verified. Browser tests cannot prove native window, Rosetta,
WSL sleep, or Mac VM behavior. Informational and withdrawn reports have no
invented passing test.

Run `python3 scripts/ci/verify-issue-coverage.py` to check this inventory against
the complete issue audit and the checked-in test paths.

| Issue | Coverage | Checks | Remaining verification |
| --- | --- | --- | --- |
"""
    for row in rows:
        number = row["issue"]
        title = row["title"].replace("|", "\\|")
        checks = "<br>".join(f"[{path}](../{path})" for path in row["checks"]) or "—"
        gap = row["remaining"].replace("|", "\\|") or "See the listed regression checks."
        text += f"| [#{number}](https://github.com/libracoder/kiri/issues/{number}) {title} | {row['coverage']} | {checks} | {gap} |\n"
    document = ROOT / "docs/issue-verification.md"
    if args.write:
        document.write_text(text)
    else:
        assert document.read_text() == text, "Run verify-issue-coverage.py --write"
    counts = Counter(row["coverage"] for row in rows)
    print(f"PASS all {len(rows)} issue reports have a checked coverage disposition: {dict(counts)}")
    print("Native/platform gaps and deferred features remain unverified; this check does not mark them fixed.")


if __name__ == "__main__":
    main()
