# Contributing to Asagiri

Issues and pull requests are welcome in AndreKurait/asagiri. Describe the
problem, what changes for users, and how you verified it. Discuss larger
features in an issue before doing substantial work.

Keep at most **two ready pull requests and two drafts** open per person. The
PR limit workflow converts an excess ready PR to a draft when there is room;
otherwise it closes the excess PR. Bots are excluded. Reopen it when there is
room. This rule also applies to maintainers.

## Development and checks

See [development](docs/development.md) for platform prerequisites. Keep the
existing source style; do not run a workspace-wide formatter over unrelated
code. Run the checks appropriate to your change:

```sh
cargo test --locked
cargo clippy --locked
python3 scripts/i18n-check.py
python3 scripts/ui-test.py
python3 tests/kernel-config-regressions.py
python3 scripts/ci/verify-issue-coverage.py
```

Linux engine tests that create namespaces or device nodes need the privileged
container used by `.github/workflows/platforms.yml`. Run conformance tests only
against a disposable engine. `scripts/ci/live-tests.sh <binary-directory>` starts
one without publishing any ports and removes it afterwards.

CI builds Linux, macOS, and Windows on every pull request. Include relevant
live tests for behavior changes and preserve platform parity in the UI.
Performance is checked against the base branch. Do not loosen the performance
limits to make a regression pass. Explain platform limitations in the PR.

## Commits and attribution

Use signed commits and add your sign-off with `git commit -S -s`. A sign-off
certifies that you have the right to submit the contribution under the
[Developer Certificate of Origin 1.1](https://developercertificate.org/).
Contributions are licensed under this project's Apache-2.0 license; Asagiri
does not require a separate contributor license agreement.

Preserve upstream authorship when porting a change. Use
`scripts/port-upstream.sh`; [UPSTREAM.md](UPSTREAM.md) describes the workflow.
Record review fixes separately from the original port. Do not submit private
or undisclosed security work in a public pull request.

If AI tools helped, describe their use in the PR template. You are responsible
for reviewing the result and its tests. Follow the [code of conduct](CODE_OF_CONDUCT.md).
Security reports go through [SECURITY.md](SECURITY.md), not public issues.
