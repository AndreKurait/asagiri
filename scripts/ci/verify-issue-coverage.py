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
    tasks = json.loads((ROOT / "docs/upstream-audit-tasks.json").read_text())
    pulls = json.loads((ROOT / "docs/upstream-pr-audit.json").read_text())
    expected = {item["number"]: item for item in audit["issues"]}
    rows = inventory["issues"]
    assert len(rows) == len(expected) == audit["count"], "Missing or duplicate issue coverage"
    assert {row["issue"] for row in rows} == set(expected), "Issue inventory differs from upstream audit"
    statuses = {"automated", "documentation", "partial", "platform-gap",
                "known-limitation", "deferred-feature", "informational"}
    work = tasks["tasks"]
    assert len(work) == tasks["count"] == len({item["key"] for item in work}), "Duplicate or missing tracking cases"
    assert tasks["new_issues"] + tasks["existing_issue_mappings"] == len(work)
    primary_keys = {item["key"] for item in work}
    supplemental = tasks.get("additional_source_findings", [])
    assert not primary_keys.intersection(item["key"] for item in supplemental)
    fork_issues = set()
    issue_targets = {}
    for item in work + supplemental:
        target = item["tracking"]
        assert target["repository"] == "AndreKurait/asagiri"
        assert isinstance(target["issue"], int) and target["issue"] > 0
        assert target["url"] == f"https://github.com/AndreKurait/asagiri/issues/{target['issue']}"
        fork_issues.add(target["issue"])
        number = item["upstream_number"]
        if item["kind"] == "issue":
            assert number in expected and item["body_sha256"] == expected[number]["body_sha256"]
            assert item["upstream_url"] == f"https://github.com/libracoder/kiri/issues/{number}"
        else:
            assert item["kind"] == "pull"
            matches = [row for row in pulls["pulls"] if row["number"] == number]
            assert len(matches) == 1 and matches[0]["head"] == item["head"]
            assert matches[0]["patch_sha256"] == item["patch_sha256"]
            assert item["upstream_url"] == f"https://github.com/libracoder/kiri/pull/{number}"
            if item["key"] in primary_keys:
                assert matches[0].get("tracking") == target, f"PR #{number} points to a different Asagiri task"
        if item["key"] in primary_keys:
            linked_issues = ([number] if item["kind"] == "issue" else []) + item.get("related_issues", [])
            for issue in linked_issues:
                assert issue in expected, f"Unknown related issue #{issue}"
                assert issue_targets.get(issue, target) == target, f"Ambiguous task for issue #{issue}"
                issue_targets[issue] = target
    assert pulls["counts"]["total"] == len(pulls["pulls"]) == len({row["number"] for row in pulls["pulls"]})
    actual_states = Counter(row["state"] for row in pulls["pulls"])
    assert set(actual_states) <= {"merged", "closed", "open"}, "Unrecognized PR state"
    assert pulls["counts"]["merged"] == actual_states["merged"]
    assert pulls["counts"]["closed_unmerged"] == actual_states["closed"]
    assert pulls["counts"]["open"] == actual_states["open"]
    for row in rows:
        issue = expected[row["issue"]]
        assert row["body_sha256"] == issue["body_sha256"], f"Re-review changed issue #{row['issue']}"
        assert row["coverage"] in statuses
        assert row["title"] == issue["title"]
        if row["coverage"] in {"automated", "documentation"}:
            assert row["checks"], f"Missing check for #{row['issue']}"
        else:
            assert row["remaining"], f"Missing verification gap for #{row['issue']}"
            if row["coverage"] != "informational":
                assert row.get("tracking", {}).get("issue") in fork_issues, f"Untracked verification gap for #{row['issue']}"
        if target := row.get("tracking"):
            assert target["issue"] in fork_issues
            assert target["url"] == f"https://github.com/AndreKurait/asagiri/issues/{target['issue']}"
            assert target == issue_targets.get(row["issue"]), f"Issue #{row['issue']} points to a different Asagiri task"
        if row["issue"] in issue_targets:
            assert row.get("tracking") == issue_targets[row["issue"]], f"Missing task for issue #{row['issue']}"
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
the complete issue audit, checked-in test paths, PR source identities and
[published verification tasks](upstream-audit-tasks.md). A linked issue is work
to verify, not a claim that its report is fixed.

| Issue | Coverage | Checks | Remaining verification | Asagiri task |
| --- | --- | --- | --- | --- |
"""
    for row in rows:
        number = row["issue"]
        title = row["title"].replace("|", "\\|")
        checks = "<br>".join(f"[{path}](../{path})" for path in row["checks"]) or "—"
        gap = row["remaining"].replace("|", "\\|") or "See the listed regression checks."
        if row.get("candidate_update"):
            gap += " " + row["candidate_update"].replace("|", "\\|")
        target = row.get("tracking")
        link = f"[#{target['issue']}]({target['url']})" if target else "—"
        text += f"| [#{number}](https://github.com/libracoder/kiri/issues/{number}) {title} | {row['coverage']} | {checks} | {gap} | {link} |\n"
    document = ROOT / "docs/issue-verification.md"
    if args.write:
        document.write_text(text)
    else:
        assert document.read_text() == text, "Run verify-issue-coverage.py --write"
    counts = Counter(row["coverage"] for row in rows)
    print(f"PASS all {len(rows)} issue reports have a checked coverage disposition: {dict(counts)}")
    print(f"PASS {len(work)} linked verification cases and {len(pulls['pulls'])} PR source records")
    print("Native/platform gaps and deferred features remain unverified; this check does not mark them fixed.")


if __name__ == "__main__":
    main()
