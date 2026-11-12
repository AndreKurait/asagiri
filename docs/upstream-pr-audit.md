# Upstream pull request audit — 2026-11-12

Reviewed every public pull request in `libracoder/kiri`, open and closed:
**133 total: 75 merged, 36 closed without merging, and 22 open**.
Upstream main is `4645536f68d4bf2f5e708bd8339a9f71f844c43a`.
Every merged PR's merge commit and the complete main history are ancestors of
Asagiri. The runtime revision for this audit is `6f003adfc179ed9bc7489d7c313c4080fc609578`.

[Machine-readable evidence](upstream-pr-audit.json) records every reviewed head,
merged commit, ancestry result and source-to-port mapping. [UPSTREAM.md](../UPSTREAM.md)
explains integration policy and platform limits. The review compared the
mechanically renamed main tree with Asagiri, then inspected the remaining PR
diffs and their superseding changes; ancestry alone was not used to dismiss
unmerged work.

## Changes from this audit

- Integrated all eleven #54 VM recovery commits through `508ba7ea8271`.
  The Asagiri follow-up waits for old-reader exit accounting before replacement,
  closes Linux accept threads during retirement, and preserves the existing
  stop, remove, startup-error and network behavior. `tests/vm-recovery.py`
  crashes only the agent of the private Linux test engine, checks persisted
  recovery without API-triggered boots, and verifies restart counts and data.
  The live race test also exposed a lifecycle lock held across unread stdin;
  `6f003ad` releases it after guest creation so kill/stop/remove can proceed.
  Native macOS VM recovery remains unverified; its real-VM test is opt-in.
- Integrated #116's `ba7fd94697d4` mirror fix and improved local tag provenance.
  Remote tags resolve all registry platforms; local same-repository tags stay
  usable for `FROM`. Buildx checks a real local build and the other architecture's
  ELF in an OCI export, including BusyBox hard links.
- Confirmed #193 is already covered by Asagiri's broader #163 route work and
  added the two-internal-networks restart case. Kept IPv6 and failover coverage.
- Retained existing equivalents for #60, #119, #137, #159 and #163. Closed
  prototypes are tied to their replacements below, rather than reintroduced.

## Deliberate exclusions

#89 still needs a complete BuildKit bridge. `session` increments `ACTIVE` before
fallible operations but decrements it only on success. `capture` buffers an entire
request before forwarding it, preventing bidirectional streaming. `send_all`
closes the stream for each response chunk and emits nothing for an empty body,
which also breaks trailers. The public diff does not connect the new server to
the socket accept loop and has no complete local-export path. The working
`docker-container` Buildx driver stays in use.

#55's image metadata fix is already covered by Asagiri's full-config inspect.
Its UI prototype forces an extra dialog on every run, guesses one exposed port,
uses a repeated default name, and predates localization. The existing container
rename, port editor and Docker create options remain available.

#69's language work is superseded by #95 and mirrors by #119. Its additional
`daemon.json`, insecure-registry and concurrency prototype is not silently
classified as a translation change: it would add another configuration surface
and change HTTP transport selection. It remains held for a dedicated design and
runtime review. Older Arabic prototypes #179/#180 are replaced by merged #182.

## Complete inventory

“Inherited” means the source head or recorded merge is in the public ancestry.
“Equivalent” means the behavior is already represented after rebranding or in a
broader Asagiri fix. “Ported” commits retain source attribution and signatures.
Runtime validation limits remain those in UPSTREAM.md; compiling on a platform
does not prove its native VM or WSL recovery was exercised.

| PR | State | Reviewed head | Disposition and evidence |
| --- | --- | --- | --- |
| [#1](https://github.com/libracoder/kiri/pull/1) — Relicense under Apache-2.0; shorter README | closed | `6cbcfed9da88` | **superseded** — Apache-2.0, NOTICE and the reorganized documentation are inherited from current main; the old release branch is not reintroduced. |
| [#2](https://github.com/libracoder/kiri/pull/2) — Keep unpacked layers whole across VM stops | merged | `8be2db95ee89` | **inherited** — Merged upstream commit 76ccc11b8d7d is an ancestor of Asagiri. |
| [#3](https://github.com/libracoder/kiri/pull/3) — Commands end quietly when their output is closed, instead of crashing | merged | `c650a58051d5` | **inherited** — Merged upstream commit 325a3b756c0e is an ancestor of Asagiri. |
| [#4](https://github.com/libracoder/kiri/pull/4) — Release notes for 0.1.15 | merged | `8773e871c0e4` | **inherited** — Merged upstream commit 47a577b48815 is an ancestor of Asagiri. |
| [#5](https://github.com/libracoder/kiri/pull/5) — Keep the update switch off, send only new crashes, redact more secrets | merged | `a432e55e1329` | **inherited** — Merged upstream commit 5f666ad873ad is an ancestor of Asagiri. |
| [#6](https://github.com/libracoder/kiri/pull/6) — Add a screenshot of the Kiri window to the README | merged | `cf13a30778ab` | **inherited** — Merged upstream commit 028124b0c0c2 is an ancestor of Asagiri. |
| [#7](https://github.com/libracoder/kiri/pull/7) — Release notes for 0.1.16 | merged | `6f81335263ec` | **inherited** — Merged upstream commit 98ce1be845b2 is an ancestor of Asagiri. |
| [#8](https://github.com/libracoder/kiri/pull/8) — Crash reports go without your account and image names; redact more | merged | `8bb809665173` | **inherited** — Merged upstream commit 77c2911a2682 is an ancestor of Asagiri. |
| [#9](https://github.com/libracoder/kiri/pull/9) — Generated passwords: draw fresh unbiased random numbers for each choice | merged | `3c18c24a67f4` | **inherited** — Merged upstream commit c8252d104090 is an ancestor of Asagiri. |
| [#11](https://github.com/libracoder/kiri/pull/11) — Container DNS survives the Mac's VM resolver going down; a deleted Kiri hands docker build back | merged | `2e948cfa6727` | **inherited** — Merged upstream commit de47ac0d91e7 is an ancestor of Asagiri. |
| [#12](https://github.com/libracoder/kiri/pull/12) — Security: Build steps keep the Mac's DNS; tighter limits on the DNS relay | merged | `89a9e92d5bd9` | **inherited** — Merged upstream commit c1da681b2cf1 is an ancestor of Asagiri. |
| [#15](https://github.com/libracoder/kiri/pull/15) — UI: Settings › Resources — panes beside the tabs, same sliders on every system, no needless restart on Windows | merged | `c2ff5a95361f` | **inherited** — Merged upstream commit e155900dddc3 is an ancestor of Asagiri. |
| [#16](https://github.com/libracoder/kiri/pull/16) — Security: CI checks with cargo-deny, ShellCheck, zizmor and OpenSSF Scorecard | open | `ac65f43f58e4` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#17](https://github.com/libracoder/kiri/pull/17) — Containers' connections no longer break as others start and stop | merged | `268af06102a9` | **inherited** — Merged upstream commit d314d6b3c203 is an ancestor of Asagiri. |
| [#19](https://github.com/libracoder/kiri/pull/19) — Containers can listen on any port and ping without root, as on Docker | merged | `8c0ac1a5a5ce` | **inherited** — Merged upstream commit 4c3aaefdd502 is an ancestor of Asagiri. |
| [#20](https://github.com/libracoder/kiri/pull/20) — Guest kernel: the rest of netfilter, tc, socket diagnostics and the usual interfaces | merged | `49d0d9fc104e` | **inherited** — Merged upstream commit 5fb10d6b05ba is an ancestor of Asagiri. |
| [#21](https://github.com/libracoder/kiri/pull/21) — e2e: wait for nginx on the internal network; ping without NET_RAW works, as on Docker | merged | `706e98174560` | **inherited** — Merged upstream commit f1237085bfd5 is an ancestor of Asagiri. |
| [#22](https://github.com/libracoder/kiri/pull/22) — Linux: the window no longer flickers with WebKitGTK 2.54 | merged | `d6aea8390ff6` | **inherited** — Merged upstream commit 37c5cd53f15a is an ancestor of Asagiri. |
| [#23](https://github.com/libracoder/kiri/pull/23) — Linux: a privileged container no longer breaks the host's terminals | closed | `301781cdcec7` | **superseded** — Replaced by merged #28: private device nodes avoid sharing host devtmpfs; merely skipping ptmx writes was insufficient. |
| [#24](https://github.com/libracoder/kiri/pull/24) — Views open at the top, not where the last page was scrolled to | merged | `56a4ad8570fc` | **inherited** — Merged upstream commit 7a7a4cc44f9f is an ancestor of Asagiri. |
| [#28](https://github.com/libracoder/kiri/pull/28) — Linux: privileged containers get their own /dev, never the host's | merged | `1b95c3f324aa` | **inherited** — Merged upstream commit ffe980f0c817 is an ancestor of Asagiri. |
| [#31](https://github.com/libracoder/kiri/pull/31) — UI: window — built-in terminal panel | merged | `1bfbfb43ac33` | **inherited** — Merged upstream commit 07e78baad371 is an ancestor of Asagiri. |
| [#32](https://github.com/libracoder/kiri/pull/32) — Check for updates every 6 hours while Kiri runs | merged | `5b9a34e50c24` | **inherited** — Merged upstream commit d01c3edb2325 is an ancestor of Asagiri. |
| [#33](https://github.com/libracoder/kiri/pull/33) — Release notes for 0.1.18 | closed | `562216a8961c` | **superseded** — Superseded by merged #35 release notes; historical Kiri notes remain distinct from Asagiri releases. |
| [#34](https://github.com/libracoder/kiri/pull/34) — e2e: test the engine under test, never the installed Kiri | merged | `6afadd012100` | **inherited** — Merged upstream commit 41abfd20daf6 is an ancestor of Asagiri. |
| [#35](https://github.com/libracoder/kiri/pull/35) — Release notes for 0.1.18 | merged | `00490e0aefc5` | **inherited** — Merged upstream commit 39a36ddec248 is an ancestor of Asagiri. |
| [#36](https://github.com/libracoder/kiri/pull/36) — Kubernetes: pods can use the host's folders through hostPath | merged | `00c75411c591` | **inherited** — Merged upstream commit f7d2cff89b5d is an ancestor of Asagiri. |
| [#37](https://github.com/libracoder/kiri/pull/37) — Volumes over a bind get the image's files, as on Docker | merged | `8ffd8636c996` | **inherited** — Merged upstream commit 82e13b560b8c is an ancestor of Asagiri. |
| [#39](https://github.com/libracoder/kiri/pull/39) — Guest kernel: BPF JIT, perf events, kprobe events, BTF and SHA1 | merged | `2f29e1fd961f` | **inherited** — Merged upstream commit 4e32e842b04f is an ancestor of Asagiri. |
| [#43](https://github.com/libracoder/kiri/pull/43) — docker run -i: see the end of stdin every time | merged | `11bbd921126e` | **inherited** — Merged upstream commit 67c5584f215c is an ancestor of Asagiri. |
| [#46](https://github.com/libracoder/kiri/pull/46) — Mac: hide the Dock icon while the window is closed | merged | `385b2cff3d19` | **inherited** — Merged upstream commit 98616431e78f is an ancestor of Asagiri. |
| [#47](https://github.com/libracoder/kiri/pull/47) — Linux packages install beside Docker's own Compose and buildx | merged | `8de179d9ddc0` | **inherited** — Merged upstream commit 8685472df856 is an ancestor of Asagiri. |
| [#51](https://github.com/libracoder/kiri/pull/51) — Linux: revoke removed users' engine access | merged | `9e2375e4ac7d` | **inherited** — Merged upstream commit 64474185fb48 is an ancestor of Asagiri. |
| [#52](https://github.com/libracoder/kiri/pull/52) — Guest kernel: backport the upstream GRO frag_list fix behind the skb_segment BUG | merged | `ff4003faa78d` | **inherited** — Merged upstream commit f973293e5122 is an ancestor of Asagiri. |
| [#53](https://github.com/libracoder/kiri/pull/53) — Keep the guest console so an unexpected VM stop leaves its oops | open | `968cf67ed4e3` | **ported** — All console-retention commits ported; Asagiri also keeps secret redaction latched across read boundaries and truncated lines. |
| [#54](https://github.com/libracoder/kiri/pull/54) — Restart always and unless-stopped containers after the VM crashes | open | `508ba7ea8271` | **ported-and-improved** — All 11 recovery commits through 508ba7ea8271 ported; 43d8cf4 adds the reader-accounting barrier, cancelable Linux listeners, correct stop responses and private-engine crash tests. 6f003ad releases the lifecycle guard before stdin delivery after the live race test exposed a blocked kill. |
| [#55](https://github.com/libracoder/kiri/pull/55) — UI: Run asks for a container name and optional host port | closed | `0bcdc8e29c60` | **partly-equivalent-and-held** — Image inspect already preserves ExposedPorts, Labels, Healthcheck and StopSignal. Hold the UI prototype: it guesses the exposed port, forces a dialog on every run, uses colliding default names and predates localization. Existing rename/port editors and Docker create options remain available. |
| [#56](https://github.com/libracoder/kiri/pull/56) — UI: keep Images Actions visible when a tag is long | merged | `54ee598952b6` | **inherited** — Merged upstream commit 3fa32335cc68 is an ancestor of Asagiri. |
| [#58](https://github.com/libracoder/kiri/pull/58) — UI: Chinese language option for the main window and Settings | closed | `c700637fcb91` | **superseded** — Multilingual UI is inherited through merged #95; do not restore the earlier two-language prototype. |
| [#60](https://github.com/libracoder/kiri/pull/60) — Security: Linux, WSL: containers reach the internet and are firewalled on kernels before 6.3 | open | `6454f419a48a` | **equivalent** — Initial branch inherited. Its 6454f419 atomic create/delete/rebuild fallback is also present in replace_table, including IPv4/IPv6 tests and bridge isolation. |
| [#61](https://github.com/libracoder/kiri/pull/61) — Windows: the engine starts again after WSL goes away | open | `652cfdd99d6b` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#62](https://github.com/libracoder/kiri/pull/62) — CI: run the UI tests on every pull request | merged | `9610d272b0e3` | **inherited** — Merged upstream commit 41ecc592d1b3 is an ancestor of Asagiri. |
| [#63](https://github.com/libracoder/kiri/pull/63) — Guest kernel: per-process I/O accounting, so Typesense starts | merged | `07003a637a1d` | **inherited** — Merged upstream commit d4e361b5bac3 is an ancestor of Asagiri. |
| [#65](https://github.com/libracoder/kiri/pull/65) — Security: only root may load BPF programs in the guest kernel | merged | `f59555cf70b1` | **inherited** — Merged upstream commit 8295c185edd2 is an ancestor of Asagiri. |
| [#67](https://github.com/libracoder/kiri/pull/67) — docker push: RepoDigests names what the registry has, as with Docker | merged | `e21c7811fbe1` | **inherited** — Merged upstream commit 5981b2be32d3 is an ancestor of Asagiri. |
| [#69](https://github.com/libracoder/kiri/pull/69) — UI: multi-language window (System/Custom + mainstream locales) | closed | `fb6078507781` | **partly-superseded-and-held** — Multilingual UI is superseded by #95; the Images column sizing and regression test are inherited through #56; registry mirrors by the reviewed #119 implementation. Hold d01849287155 daemon.json/insecure-registry/concurrency prototype: its separate configuration API and HTTP transport behavior need dedicated design and runtime coverage. |
| [#70](https://github.com/libracoder/kiri/pull/70) — Builds: amd64 steps run through Rosetta on Apple silicon, not QEMU | merged | `f65c89f7278c` | **inherited** — Merged upstream commit 2af64d88689b is an ancestor of Asagiri. |
| [#71](https://github.com/libracoder/kiri/pull/71) — Security: run CodeQL on every pull request, forks included | merged | `82d3bdf19032` | **inherited** — Merged upstream commit 108dac0748f0 is an ancestor of Asagiri. |
| [#75](https://github.com/libracoder/kiri/pull/75) — Mac: the Finder mount never covers your files or another mount at ~/Kiri | merged | `5005b1d3ef0d` | **inherited** — Merged upstream commit 6327712d3cbd is an ancestor of Asagiri. |
| [#76](https://github.com/libracoder/kiri/pull/76) — Bind mounts: a new file without owner read can be created, as on Linux | merged | `7f3e38f5d3b6` | **inherited** — Merged upstream commit 2005cd59deb4 is an ancestor of Asagiri. |
| [#84](https://github.com/libracoder/kiri/pull/84) — Images: Confluent and Temporal images show their logos | merged | `25a6bac1048f` | **inherited** — Merged upstream commit b6b3044437e1 is an ancestor of Asagiri. |
| [#86](https://github.com/libracoder/kiri/pull/86) — feat(ui): add English and Simplified Chinese | closed | `3d7510ee8e3b` | **superseded** — English/Chinese prototype superseded by merged #95 multilingual UI. |
| [#87](https://github.com/libracoder/kiri/pull/87) — Docs: Linux engine start, the docker.sock link and IPv6 networks | merged | `5e6ee6af9932` | **inherited** — Merged upstream commit a705eb16a643 is an ancestor of Asagiri. |
| [#89](https://github.com/libracoder/kiri/pull/89) — Built-in builder: relay buildx's gRPC to buildkitd in the guest | open | `2dd9bde86300` | **held** — The built-in gRPC builder is incomplete: missing accept-loop routing/local export support, unbounded request buffering, premature END_STREAM, no empty-body terminator, and ACTIVE counter leaks on error. Retain the tested docker-container Buildx driver. |
| [#95](https://github.com/libracoder/kiri/pull/95) — UI: Settings → Appearance — Language, in 15 languages, chosen on first start | merged | `94b229d08316` | **inherited** — Merged upstream commit 0b518b898cbc is an ancestor of Asagiri. |
| [#96](https://github.com/libracoder/kiri/pull/96) — USB passthrough (beta): USB devices in containers on macOS, Windows and Linux | merged | `8f5ff75741dd` | **inherited** — Merged upstream commit f28d227d709f is an ancestor of Asagiri. |
| [#100](https://github.com/libracoder/kiri/pull/100) — Docs: README, features and development in Brazilian Portuguese | open | `a6fa9453b34b` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#101](https://github.com/libracoder/kiri/pull/101) — Windows: setup reaches a proxy on Windows' loopback, and says why a pull failed | merged | `90475150727c` | **inherited** — Merged upstream commit a7499c205879 is an ancestor of Asagiri. |
| [#102](https://github.com/libracoder/kiri/pull/102) — Engine API: report the real kernel on Linux | merged | `98f0e37429a8` | **inherited** — Merged upstream commit 6f8908478b34 is an ancestor of Asagiri. |
| [#103](https://github.com/libracoder/kiri/pull/103) — docker load: images saved by podman and older Docker load again | merged | `81461d7deb45` | **inherited** — Merged upstream commit 9bd6b8c62b2e is an ancestor of Asagiri. |
| [#104](https://github.com/libracoder/kiri/pull/104) — Copy into containers: accept gzip archives, so DDEV starts | merged | `57d43c9a60f5` | **inherited** — Merged upstream commit 9c8c260355f9 is an ancestor of Asagiri. |
| [#105](https://github.com/libracoder/kiri/pull/105) — vm: run arm64 images on Intel Macs through qemu | closed | `8b39ed3e5ed5` | **superseded** — Replaced by #116; the newer emulation implementation is ported with a read-only emulator share and a Linux-arm64-only platform exception. |
| [#106](https://github.com/libracoder/kiri/pull/106) — Non-root containers own their stdio, so php-fpm and nginx can open their logs | merged | `dfc88f817b9d` | **inherited** — Merged upstream commit 4418009e3f9f is an ancestor of Asagiri. |
| [#107](https://github.com/libracoder/kiri/pull/107) — VM clock: keep it on the Mac's time after the Mac sleeps | merged | `5fac053f12a0` | **inherited** — Merged upstream commit ae9b81b8c8e1 is an ancestor of Asagiri. |
| [#108](https://github.com/libracoder/kiri/pull/108) — machine shell: reach the same engine as the other machine commands | merged | `217e847b0c17` | **inherited** — Merged upstream commit f0ada7a193c2 is an ancestor of Asagiri. |
| [#109](https://github.com/libracoder/kiri/pull/109) — CI: let runs on main finish instead of cancelling each other | merged | `46107a625210` | **inherited** — Merged upstream commit b94a40bff6c6 is an ancestor of Asagiri. |
| [#110](https://github.com/libracoder/kiri/pull/110) — Domain router: a container that publishes port 80 or 443 gets it | merged | `6661b97aee8b` | **inherited** — Merged upstream commit 73969cb70bfd is an ancestor of Asagiri. |
| [#111](https://github.com/libracoder/kiri/pull/111) — kiri machine: shell works on Linux hosts and on Windows | merged | `db80a775d8da` | **inherited** — Merged upstream commit 3274468d9750 is an ancestor of Asagiri. |
| [#113](https://github.com/libracoder/kiri/pull/113) — docker system df: real sizes for images, containers and volumes | merged | `128eea119a09` | **inherited** — Merged upstream commit 8e400bf111bd is an ancestor of Asagiri. |
| [#114](https://github.com/libracoder/kiri/pull/114) — Start: send a container's layers again if the engine lost them | merged | `08b38c4a1ca2` | **inherited** — Merged upstream commit 89e6fc91cb6f is an ancestor of Asagiri. |
| [#115](https://github.com/libracoder/kiri/pull/115) — UI: Settings → Domains — choose the ports container names answer on | merged | `a850ed59c11c` | **inherited** — Merged upstream commit 7d8d0b4c1248 is an ancestor of Asagiri. |
| [#116](https://github.com/libracoder/kiri/pull/116) — Intel Macs: run arm64 images through qemu, and leave a registry's tags to the registry | open | `ba7fd94697d4` | **ported-and-improved** — Both source commits ported. 3b3f0f2 adds registry-owned remote tags; b6bf9ac records local tag provenance so local same-repository tags remain usable. Cross-platform OCI export checks the BusyBox ELF, and local FROM is built and run. Optional Intel QEMU remains outside release bundles pending complete corresponding-source packaging. |
| [#117](https://github.com/libracoder/kiri/pull/117) — Windows: setup carries on after a failed engine step | merged | `8b4a8664295e` | **inherited** — Merged upstream commit c21387373730 is an ancestor of Asagiri. |
| [#119](https://github.com/libracoder/kiri/pull/119) — Images: registry mirrors for Docker Hub, in Settings and the CLI | open | `cd0a5ab38e43` | **ported-and-improved** — Mirror/settings/setup commits ported. fc739e4 credential isolation is already enforced with complete manifest/config/layer header tests; active Buildx execs survive mirror settings changes. |
| [#120](https://github.com/libracoder/kiri/pull/120) — A language picked from the menu shows everywhere at once | merged | `78b1291ead2d` | **inherited** — Merged upstream commit a86f903cbe2e is an ancestor of Asagiri. |
| [#122](https://github.com/libracoder/kiri/pull/122) — Reclaim unused blocks from the macOS VM data disk | open | `676c46d2f1ae` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#124](https://github.com/libracoder/kiri/pull/124) — kiri api: fix handling of empty array for command | open | `d50ca5b9621a` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#126](https://github.com/libracoder/kiri/pull/126) — Attach and exec: answer non-upgrading SDKs with HTTP 200 | closed | `682183915d04` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#131](https://github.com/libracoder/kiri/pull/131) — Containers: expose Docker's marker for SDK detection | open | `3c03e8b5dacd` | **inherited-and-improved** — Current head inherited, with source 11f698a also ported so container changes to /.dockerenv remain visible in diff/commit. |
| [#132](https://github.com/libracoder/kiri/pull/132) — Archives: keep file types out of tar permission bits | closed | `a8625407fb6d` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#133](https://github.com/libracoder/kiri/pull/133) — Linux: let container users reach the daemon through its socket group | closed | `5e06482d35e6` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#135](https://github.com/libracoder/kiri/pull/135) — HTTP: handle upload continue requests and chunk trailers | closed | `51025d186fc5` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#136](https://github.com/libracoder/kiri/pull/136) — API: decode escaped image paths before routing | closed | `5b2beb4ae600` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#137](https://github.com/libracoder/kiri/pull/137) — Archives: preserve directory-contents paths for docker cp | open | `4a038f2eb183` | **equivalent** — Initial branch inherited. Both newer commits 781b8fdc2ddf and 4a038f2eb183 produce empty normalized patches; archive path and /proc regression coverage is present. |
| [#138](https://github.com/libracoder/kiri/pull/138) — API: reject requests outside the advertised version range | closed | `bdf6615b9c55` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#139](https://github.com/libracoder/kiri/pull/139) — API: preserve success status codes for empty responses | closed | `8afec6db1952` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#140](https://github.com/libracoder/kiri/pull/140) — Containers: enforce and safely update cgroup resource limits | closed | `2849e2915377` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#141](https://github.com/libracoder/kiri/pull/141) — Containers: validate restart policies before applying changes | closed | `2ef46b19e5f0` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#142](https://github.com/libracoder/kiri/pull/142) — Containers: finish stopping before answering lifecycle requests | open | `87596e95ee28` | **inherited-and-improved** — Head inherited; stop waits for the requested run, preserves HTTP status semantics, and sleep attempts all containers. Recovery integration preserves those behaviors. |
| [#143](https://github.com/libracoder/kiri/pull/143) — feat: support Docker interactive attach and WebSocket streams | closed | `7a9b4cc20010` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#144](https://github.com/libracoder/kiri/pull/144) — usb: don't poison PENDING when a device is already attached | merged | `0c9dedd1cf0d` | **inherited** — Merged upstream commit 1b639b01e498 is an ancestor of Asagiri. |
| [#145](https://github.com/libracoder/kiri/pull/145) — Release notes for 0.1.19 | closed | `86a830a28f52` | **superseded** — Superseded by merged #147 notes for Kiri 0.2.0. |
| [#146](https://github.com/libracoder/kiri/pull/146) — USB: each device's details, where it shows up inside Linux, and Copied on every copy button | merged | `b071bce364b6` | **inherited** — Merged upstream commit 5a01049f90c3 is an ancestor of Asagiri. |
| [#147](https://github.com/libracoder/kiri/pull/147) — Release notes for 0.2.0 | merged | `5d3162a7e27a` | **inherited** — Merged upstream commit c6c404ecdf1e is an ancestor of Asagiri. |
| [#148](https://github.com/libracoder/kiri/pull/148) — README: the 15 languages Kiri speaks | merged | `0c2b25232105` | **inherited** — Merged upstream commit d7d282e499e5 is an ancestor of Asagiri. |
| [#149](https://github.com/libracoder/kiri/pull/149) — Containers: deliver a stop or kill that arrives while the container starts | open | `5b8b2b83f4c4` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#150](https://github.com/libracoder/kiri/pull/150) — Networks: keep containers apart on a bridge network with enable_icc off | closed | `3e1fc16c738b` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#151](https://github.com/libracoder/kiri/pull/151) — Networks: apply a bridge network's MTU option to its bridge and containers | closed | `b2e46d0099d1` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#152](https://github.com/libracoder/kiri/pull/152) — Networks: report each endpoint's MAC address, endpoint ID and DNS names in inspect | closed | `f5df5beed447` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#153](https://github.com/libracoder/kiri/pull/153) — Archive: keep /proc and /sys empty when a link leads to them | closed | `f0b52705cc94` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#154](https://github.com/libracoder/kiri/pull/154) — Seccomp: apply a custom profile given with --security-opt seccomp=<profile.json> | closed | `225a7cef91e8` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#155](https://github.com/libracoder/kiri/pull/155) — CI: speed check on every pull request | merged | `cbdcda850c54` | **inherited** — Merged upstream commit dfe3ffa5f5a4 is an ancestor of Asagiri. |
| [#156](https://github.com/libracoder/kiri/pull/156) — CI: speed check compares against the main each pull request merges into | merged | `f45f8a4360b1` | **inherited** — Merged upstream commit a5aeb7de06dc is an ancestor of Asagiri. |
| [#157](https://github.com/libracoder/kiri/pull/157) — Networks: show no gateway for endpoints of internal networks | closed | `d0d303b56f5c` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#158](https://github.com/libracoder/kiri/pull/158) — Containers: give CPU shares the weight Docker 29 gives them | closed | `dac5bcb86593` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#159](https://github.com/libracoder/kiri/pull/159) — CI: name the engine socket in the speed check | open | `94c7c6f7a72d` | **equivalent** — Earlier work inherited; 94c7c6f7a72d socket selection is present as ASAGIRI_SOCKET/KIRI_SOCKET selection in the performance runner. |
| [#160](https://github.com/libracoder/kiri/pull/160) — Networks: resolve a joined network's names in a container from the default bridge | closed | `bc74803d7b8b` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#161](https://github.com/libracoder/kiri/pull/161) — CI: speed check rebuilds the change instead of keeping main's binary | merged | `5c0045e27d1e` | **inherited** — Merged upstream commit 32eaa0a963d4 is an ancestor of Asagiri. |
| [#162](https://github.com/libracoder/kiri/pull/162) — Networks: keep each network's interface when another network leaves a container | closed | `9bb4972ad0e7` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#163](https://github.com/libracoder/kiri/pull/163) — Networks: route a container through a network it joins after an internal one | closed | `ec6afd954707` | **equivalent** — Earlier branch inherited. e087e2b6bc19/ec6afd954707 behavior and live tests are present, retaining later DNS, MTU, macvlan, ipvlan, interface and route-failover fixes. |
| [#164](https://github.com/libracoder/kiri/pull/164) — Networks: macvlan networks without a parent | closed | `c05ed4b23d0c` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#165](https://github.com/libracoder/kiri/pull/165) — Networks: ipvlan networks without a parent | closed | `d721d9b6ef1b` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#166](https://github.com/libracoder/kiri/pull/166) — Containers: do not remove a container that a start has just taken | open | `4f41910c6e63` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#167](https://github.com/libracoder/kiri/pull/167) — Contributing: issue and pull request templates, and at most 3 open pull requests | merged | `bb5da5eadb45` | **inherited** — Merged upstream commit a649319b5338 is an ancestor of Asagiri. |
| [#169](https://github.com/libracoder/kiri/pull/169) — Containers: keep the exit code of a container that removed itself | closed | `afcbd72afa06` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#171](https://github.com/libracoder/kiri/pull/171) — Linux: the window no longer goes blank on hover with WebKitGTK 2.54 | open | `7190e95c09a5` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#172](https://github.com/libracoder/kiri/pull/172) — Linux packages: NetworkManager leaves Kiri's interfaces alone | open | `c2b4454847a4` | **inherited** — Reviewed public head is an ancestor of Asagiri; related live or platform checks remain in CI. |
| [#173](https://github.com/libracoder/kiri/pull/173) — Stop: the daemon stays up, so launchd stops restarting it every 10 s and Start works | closed | `79c6745668e3` | **inherited-and-superseded** — Public head inherited; final Stop/Start lifecycle also arrives through merged #181. |
| [#174](https://github.com/libracoder/kiri/pull/174) — kiri on the PATH finds the guest kernel | merged | `058d8bc9c98f` | **inherited** — Merged upstream commit 5ce108bf1025 is an ancestor of Asagiri. |
| [#175](https://github.com/libracoder/kiri/pull/175) — Mounts: /etc paths work on a Mac, and a linked source is followed | closed | `f8ab3c12e3db` | **inherited-and-superseded** — Public head inherited; final macOS linked-mount resolution also arrives through merged #178. |
| [#176](https://github.com/libracoder/kiri/pull/176) — Ports: "already allocated" names the program that has the port | merged | `43fa96afdbaa` | **inherited** — Merged upstream commit 80a185d8d4cd is an ancestor of Asagiri. |
| [#177](https://github.com/libracoder/kiri/pull/177) — Logs: select and copy the text | open | `ee05b04b05a7` | **inherited-and-ported** — Log selection is present by content; ee05b04b05a7 keyboard-copy follow-up is ported as 1fc204f. |
| [#178](https://github.com/libracoder/kiri/pull/178) — Mounts: /etc paths work on a Mac, and a linked source is followed | merged | `4b7136ef201c` | **inherited** — Merged upstream commit acfae6e11bb8 is an ancestor of Asagiri. |
| [#179](https://github.com/libracoder/kiri/pull/179) — UI: Settings → Appearance — Arabic, the window's first right-to-left language | closed | `aa6b5d63039f` | **superseded** — Replaced by #180 then merged #182, which includes the later sanitization and UI test fixes. |
| [#180](https://github.com/libracoder/kiri/pull/180) — UI: Settings → Appearance — Arabic, the window's first right-to-left language | closed | `e98c21cb3e89` | **superseded** — Replaced by merged #182; keep the final Arabic/RTL implementation and fixes. |
| [#181](https://github.com/libracoder/kiri/pull/181) — Stop: the daemon stays up, so launchd stops restarting it every 10 s and Start works | merged | `ff7fb2a852e0` | **inherited** — Merged upstream commit 544b5ad91f1a is an ancestor of Asagiri. |
| [#182](https://github.com/libracoder/kiri/pull/182) — UI: Settings → Appearance — Arabic, the window's first right-to-left language | merged | `52a81e2706db` | **inherited** — Merged upstream commit ab43ec6fe1f7 is an ancestor of Asagiri. |
| [#183](https://github.com/libracoder/kiri/pull/183) — Containers: a start failure names its real cause and gets Docker's exit code | merged | `7582166cef47` | **inherited** — Merged upstream commit 41744d4dabe9 is an ancestor of Asagiri. |
| [#184](https://github.com/libracoder/kiri/pull/184) — Linux: stopping the engine removes its bridges and nftables table | merged | `8ffcac940d1e` | **inherited** — Merged upstream commit 63ddbf25de8a is an ancestor of Asagiri. |
| [#185](https://github.com/libracoder/kiri/pull/185) — Terminal: a resize that macOS puts back is set again | merged | `6bc477f0b9d2` | **inherited** — Merged upstream commit d15c8c7849fe is an ancestor of Asagiri. |
| [#186](https://github.com/libracoder/kiri/pull/186) — Ports: say once why the VM's NAT network is unreachable | merged | `6b0cb4dc25ef` | **inherited** — Merged upstream commit 4645536f68d4 is an ancestor of Asagiri. |
| [#192](https://github.com/libracoder/kiri/pull/192) — Improve macOS bind-mount paths and sequential reads | open | `6d60e28b145a` | **ported** — 6d60e28b145a ported as a833170: component-aware /opt and /usr/local Mac mounts and best-effort 1 MiB virtiofs read-ahead. Upstream Mac throughput figures have not been remeasured here. |
| [#193](https://github.com/libracoder/kiri/pull/193) — Give secondary external networks an IPv4 default route | open | `8d6368bdcc2c` | **equivalent-and-expanded** — Covered by #163 implementation: secondary IPv4/IPv6 defaults without duplicates, restart and route failover. 43d8cf4 additionally checks two internal networks before and after restart. The DHCP route helper keeps its existing behavior. |
