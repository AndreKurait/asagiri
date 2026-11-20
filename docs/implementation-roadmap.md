# Asagiri implementation roadmap

See the [roadmap worklist](roadmap-worklist.md) for the pull request queue, issue inventory, and verification evidence.

GitHub queue checked on 2026-11-20. There were 15 open pull requests and 177 open issues.

See the [work inventory](roadmap-work-inventory.md) for the full GitHub snapshot.

## Work sequence

1. Confirm the quota workflow removal lands and audit PRs closed by the bot.
2. Drain the pending PR queue, resolving CI failures and confirming merges on `main`.
3. Work through the issue backlog using the reported entry points and platforms. Close issues after recording resolution evidence and keep active roadmap items open.
4. Expand Verus coverage one behavior at a time. Verify the production path, remove unit tests covered by the proof, and record the remaining test count and CI results.

## 1. Finish the merge queue

The main ruleset required the retired `limit` check. Removed that check from ruleset `27075130` so the remaining CI checks govern merges.

Audited the quota bot’s historical comments. PRs #35, #279, #282, and #283 are merged. PRs #284, #287, and #289 are open again. The sole closed record, #6, is the completed disposable quota test.

Check required jobs, fix failures, update branches after earlier merges, and confirm each change landed on `main`. Validate the linked issue against the merged behavior before closing it.

- [x] Support Docker top process formats and memory columns. All nine required checks passed. Merged as `fff93d86155367c7e4ee3fa8fa849d8609a9c4fa`. [#281](https://github.com/AndreKurait/asagiri/pull/281)
- [x] Include unpublished exposed ports in running endpoint inspection. Live fixture passed on disposable ARM64 Linux. GitHub merged the change as `6f93083d6948b2e147ec5b9da008acdf33dd8f7b`. [#282](https://github.com/AndreKurait/asagiri/pull/282)
- [x] Honor HTTP/1.0 default closure and explicit keep-alive. Live ARM64 Linux and macOS HTTP fixtures passed. The isolated macOS daemon at `202b4626` also passed 33 upload framing cases and 10 image load cases. All nine required checks passed and GitHub merged `1e94949d`. Native Windows end-to-end qualification remains open. [#283](https://github.com/AndreKurait/asagiri/pull/283)
- [ ] Support volumes-from and preserve shared anonymous volumes — auto-merge enabled. Corrected Docker reference fixture passed on Docker 28.0.4. The live volume ownership fixture also passed on disposable ARM64 Asagiri. [#284](https://github.com/AndreKurait/asagiri/pull/284)
- [x] Handle repeated container actions and build history removal. Merged as `59171936`. [#287](https://github.com/AndreKurait/asagiri/pull/287)
- [ ] Flag unusually long short translations in changed locales — blocked, auto-merge enabled. [#288](https://github.com/AndreKurait/asagiri/pull/288)
- [ ] Add Serbian Latin to the desktop and installer — blocked, auto-merge enabled. [#289](https://github.com/AndreKurait/asagiri/pull/289)
- [x] Removed the per-person pull request limit. All nine required checks passed, and the workflow is absent from `main` at `ac4a738f`. [#290](https://github.com/AndreKurait/asagiri/pull/290)
- [ ] Verify health probe exit codes and the startup grace boundary — blocked, auto-merge enabled. [#291](https://github.com/AndreKurait/asagiri/pull/291)
- [x] Prove arbitrary HTTP number padding and retire sampled value tests. Required CI passed and GitHub merged `cfbeab94`. The [merged-main runtime proof](verification/runtime-proof-cfbeab94.json) passed. [#292](https://github.com/AndreKurait/asagiri/pull/292)
- [x] Accept streamed gzip archives on image load. All nine required checks passed. Merged as `7100bdff`. [#293](https://github.com/AndreKurait/asagiri/pull/293)
- [ ] Support OCI image layout archives in docker load — blocked, auto-merge enabled. [#294](https://github.com/AndreKurait/asagiri/pull/294)

- [ ] Verify each Pages publication by ID. Eleven local tests and the hosted Pages build passed (run 44638959682). Live deployment verification remains pending. Auto-merge enabled. [#295](https://github.com/AndreKurait/asagiri/pull/295)

- [x] Verify Docker exec inspection after VM loss. The signed macOS fixture passed twice within one run. Merged on `main` as `d74d0120`. [#296](https://github.com/AndreKurait/asagiri/pull/296)

- [ ] Verify container lifecycle generation checks. Clean pinned Verus passed both production contracts and rejected all four incorrect variants at `c5c4e60c`. The live macOS fixture passed 3,008 concurrent inspections, restart cancellation, and automatic removal. Hosted CI and merge remain pending. [#299](https://github.com/AndreKurait/asagiri/pull/299)

- [ ] Refresh embedded DNS after renaming a running container. The paired macOS daemon and Linux guest passed all seven live DNS scenarios. Hosted Linux and merge remain pending. [#300](https://github.com/AndreKurait/asagiri/pull/300)

- [ ] Serve Windows setup and desktop documents from memory. Windows cross-compilation passed. The hosted fixture exercises a document above 2 MiB, each supported language, and origin restrictions. [#301](https://github.com/AndreKurait/asagiri/pull/301)

- [ ] Retire sampled address-gap assertions. PR #302 removes 262,144 sampled range queries and keeps production adapter, iterator consumption, and allocation coverage. Clean Verus verified three obligations and rejected six broken variants. All 20 network unit tests passed. The isolated macOS fixture passed dense dual-stack allocation, static and auxiliary reservations, exhaustion, rollback, and reuse. Hosted CI remains pending. [#302](https://github.com/AndreKurait/asagiri/pull/302)

- [ ] Expand Docker reference coverage for mixed none networks. Auto-merge is enabled. Follow-up semantics qualification is tracked in issue #265. [#298](https://github.com/AndreKurait/asagiri/pull/298)
- [ ] Verify default bridge reachability when Docker starts first. Auto-merge is enabled. The hosted fixture is part of the qualification for issue #234. [#303](https://github.com/AndreKurait/asagiri/pull/303)
- [x] Merge the initial roadmap as `bd80ab7a`. Checklist maintenance continues as work lands. [#297](https://github.com/AndreKurait/asagiri/pull/297)

## 2. Resolve the issue backlog

For each report, identify its acceptance criteria, exercise the affected entry point on the reported platform, and record the result. Close resolved or superseded issues with evidence. Keep active roadmap work open with a concrete next step. The groups below describe the work and do not change GitHub labels.

### Roadmap and formal verification (3)

- [ ] Track Kiri compatibility and expanded Verus verification [#78](https://github.com/AndreKurait/asagiri/issues/78)
- [ ] Verify HTTP framing, connection closure, and numeric parsing with Verus [#80](https://github.com/AndreKurait/asagiri/issues/80)
- [ ] Verify lifecycle cancellation and layer preparation ownership. PR #299 proves a single counter advance invalidates the captured generation, and both callers require equality under the same lifecycle lock. Whole-counter wraparound, lock and process behavior, and layer readiness remain outside those contracts. The live lifecycle checks retain their independent coverage. [#81](https://github.com/AndreKurait/asagiri/issues/81)

### Upstream change reviews (12)

- [ ] Review and verify Kiri PR #514: Windows: cancel in-flight terminal opens on close or replacement [#243](https://github.com/AndreKurait/asagiri/issues/243)
- [ ] Review and verify Kiri PR #506: Security: import runs only an official image as its volume helper [#244](https://github.com/AndreKurait/asagiri/issues/244)
- [ ] Review and verify Kiri PR #502: docker load: read the rest of the body before answering [#245](https://github.com/AndreKurait/asagiri/issues/245)
- [ ] Review and verify Kiri PR #501: Compose gets its own section [#246](https://github.com/AndreKurait/asagiri/issues/246)
- [ ] Review and verify Kiri PR #493: image: fix jsonfile::sync PermissionDenied (os error 5) on Windows [#247](https://github.com/AndreKurait/asagiri/issues/247)
- [ ] Review and verify Kiri PR #472: scan: kiri scan <image>, the scanner the window's tab uses [#248](https://github.com/AndreKurait/asagiri/issues/248)
- [ ] Review and verify Kiri PR #421: Fix stalled image pull cancellation [#251](https://github.com/AndreKurait/asagiri/issues/251)
- [ ] Review and verify Kiri PR #419: update: check the tarball's member names before unpacking [#252](https://github.com/AndreKurait/asagiri/issues/252)
- [ ] Review and verify Kiri PR #401: UI:Add Azerbaijani language support [#253](https://github.com/AndreKurait/asagiri/issues/253)
- [ ] Review and verify Kiri PR #397: Fix macOS search field keyboard shortcuts [#255](https://github.com/AndreKurait/asagiri/issues/255)
- [ ] Review and verify Kiri PR #314: xtask windows-dev: pick a real WSL distro on Windows [#260](https://github.com/AndreKurait/asagiri/issues/260)
- [ ] Review and verify Kiri PR #119: Images: registry mirrors for Docker Hub, in Settings and the CLI [#261](https://github.com/AndreKurait/asagiri/issues/261)

### Upstream issue qualification (150)

- [ ] Verify Kiri #14: Guest kernel: missing nftables expressions (reject, queue, ct label, socket, tproxy, log, limit, quota). The installed macOS v0.1.13 guest (Linux 6.18.58) passed [all 56 kernel probes](verification/guest-kernel-macos-0.1.13.txt), including the reported expressions, using signed Alpine packages staged locally. Other guest architectures remain unverified. [#91](https://github.com/AndreKurait/asagiri/issues/91)
- [ ] Verify Kiri #25: macOS: Allow hiding the Dock icon while Kiri runs in the background [#92](https://github.com/AndreKurait/asagiri/issues/92)
- [ ] Verify Kiri #27: Container DNS fails on Macs with Cisco Umbrella [#93](https://github.com/AndreKurait/asagiri/issues/93)
- [ ] Verify Kiri #38: Cilium does not start in kind: the guest kernel needs BPF JIT, perf events and BTF [#94](https://github.com/AndreKurait/asagiri/issues/94)
- [ ] Verify Kiri #40: macOS: the Finder mount hides an existing ~/Kiri folder and force-unmounts other mounts [#95](https://github.com/AndreKurait/asagiri/issues/95)
- [ ] Verify Kiri #41: Crash on Typesense startup (SIGSEGV): kernel missing CONFIG_TASK_IO_ACCOUNTING (/proc/self/io) [#96](https://github.com/AndreKurait/asagiri/issues/96)
- [ ] Verify Kiri #44: Bridge-network containers have no outbound connectivity on Windows/WSL 2 (no MASQUERADE rule for bridge subnets) [#97](https://github.com/AndreKurait/asagiri/issues/97)
- [ ] Verify Kiri #45: silly question, is it needed Docker installed so kiri works? Windows release CI runs silent MSI installation and removal on x64 and ARM64. Qualify first-run WSL setup and fresh macOS onboarding without Docker installed. [#98](https://github.com/AndreKurait/asagiri/issues/98)
- [ ] Verify Kiri #48: Gaps against OrbStack: engine doesn't restart after update, docker build fails (buildkit layer not unpacked), no inotify on bind mounts, no default seccomp [#99](https://github.com/AndreKurait/asagiri/issues/99)
- [ ] Verify Kiri #49: Built-in builder: Serve Docker's build endpoints (/session, /grpc) so buildx works with its docker driver [#100](https://github.com/AndreKurait/asagiri/issues/100)
- [ ] Verify Kiri #50: macOS: the VM stops several times a day under load (guest kernel BUG in skb_segment on ip_forward); --restart always containers stay down [#101](https://github.com/AndreKurait/asagiri/issues/101)
- [ ] Verify Kiri #57: Builds for a foreign platform use the local image of the same tag and ignore --platform (Invalid ELF image for this architecture) [#102](https://github.com/AndreKurait/asagiri/issues/102)
- [ ] Verify Kiri #59: Allow running linux/arm64 containers on Intel Macs via QEMU user-mode emulation (currently hard-rejected with "Kiri does not emulate Arm") [#103](https://github.com/AndreKurait/asagiri/issues/103)
- [ ] Verify Kiri #66: bun install aborts under linux/amd64 emulation on Apple Silicon (QEMU SIGABRT), works on OrbStack [#104](https://github.com/AndreKurait/asagiri/issues/104)
- [ ] Verify Kiri #72: Bind mounts (macOS): open(O_CREAT|O_WRONLY, 0200) fails with EACCES, breaking Node's native fs.cpSync [#105](https://github.com/AndreKurait/asagiri/issues/105)
- [ ] Verify Kiri #73: Control Kiri from browser. [#106](https://github.com/AndreKurait/asagiri/issues/106)
- [ ] Verify Kiri #74: Create an MCP server to allow agents to work smoother [#107](https://github.com/AndreKurait/asagiri/issues/107)
- [ ] Verify Kiri #85: docker load / import always fail (400): "archive is missing <config>.json" though the file is in the tar — cannot ingest local images [#108](https://github.com/AndreKurait/asagiri/issues/108)
- [ ] Verify Kiri #91: Setup failure is undiagnosable: "the test container did not run" hides the real cause [#109](https://github.com/AndreKurait/asagiri/issues/109)
- [ ] Verify Kiri #92: `setup.json` `done:false` leaves Kiri silently dead — no retry, engine never starts [#110](https://github.com/AndreKurait/asagiri/issues/110)
- [ ] Verify Kiri #93: After long idle / Windows sleep, `docker` fails with "Kiri is stopped: press Start" and the bridge is not rebuilt [#111](https://github.com/AndreKurait/asagiri/issues/111)
- [ ] Verify Kiri #99: Getting issue with DDEV [#112](https://github.com/AndreKurait/asagiri/issues/112)
- [ ] Verify Kiri #123: macOS: VM data.img retains host disk allocation after guest files are deleted [#113](https://github.com/AndreKurait/asagiri/issues/113)
- [ ] Verify Kiri #125: Docker network compatibility: bridge options and IPv4/IPv6 drivers [#114](https://github.com/AndreKurait/asagiri/issues/114)
- [ ] Verify Kiri #130: Container security: custom seccomp profiles without weakening Kiri host isolation [#115](https://github.com/AndreKurait/asagiri/issues/115)
- [ ] Verify Kiri #168: Release or suspend WebView2 memory when window is closed to tray (Lightweight background mode) [#116](https://github.com/AndreKurait/asagiri/issues/116)
- [ ] Verify Kiri #170: 请看提交的报告 [#117](https://github.com/AndreKurait/asagiri/issues/117)
- [ ] Verify Kiri #187: macOS: window traffic-light buttons shift upward after switching themes [#118](https://github.com/AndreKurait/asagiri/issues/118)
- [ ] Verify Kiri #190: Windows: the window's usage figure counts unrelated processes (PID reuse when building the process tree) [#119](https://github.com/AndreKurait/asagiri/issues/119)
- [ ] Verify Kiri #199: Every linux/amd64 container crashes with "rosetta error: unhandled auxillary vector type 29" (SIGTRAP) on Apple Silicon [#120](https://github.com/AndreKurait/asagiri/issues/120)
- [ ] Verify Kiri #202: USB device detaches by itself after ~8 minutes idle (ESP32-S3) [#121](https://github.com/AndreKurait/asagiri/issues/121)
- [ ] Verify Kiri #203: Proposal for a Kiri MCP server (As suggested in Issue #74) [#122](https://github.com/AndreKurait/asagiri/issues/122)
- [ ] Verify Kiri #209: Windows: docker build hangs when a RUN step needs network (BuildKit has no usable DNS) [#123](https://github.com/AndreKurait/asagiri/issues/123)
- [ ] Verify Kiri #210: macOS: containers report hundreds of GB of block reads while the guest has no page cache [#124](https://github.com/AndreKurait/asagiri/issues/124)
- [ ] Verify Kiri #218: Windows: publishing a port on a host IP (-p IP:port:port) fails with 'Address not available (os error 99)'; 0.0.0.0 is loopback-only [#125](https://github.com/AndreKurait/asagiri/issues/125)
- [ ] Verify Kiri #226: Colima vs Kiri vs Podman [#126](https://github.com/AndreKurait/asagiri/issues/126)
- [ ] Verify Kiri #228: Linux: `docker network create` without an explicit subnet always fails with "no free subnet left" (breaks kind) [#127](https://github.com/AndreKurait/asagiri/issues/127)
- [ ] Verify Kiri #245: docker cp can't read files inside a named volume (breaks Supabase CLI 3 restart) [#128](https://github.com/AndreKurait/asagiri/issues/128)
- [ ] Verify Kiri #247: Bridge containers have no outbound TCP/UDP when host FORWARD policy is DROP (no accept or MASQUERADE rules for kiri0) [#129](https://github.com/AndreKurait/asagiri/issues/129)
- [ ] Verify Kiri #248: Removing a network leaks its DNS listener: next network on the same subnet fails with 'dns bind: Address in use' [#130](https://github.com/AndreKurait/asagiri/issues/130)
- [ ] Verify Kiri #250: macOS: launching Kiri from Finder/Dock exits silently (open returns 0), and the log records command="" instead of what failed [#131](https://github.com/AndreKurait/asagiri/issues/131)
- [ ] Verify Kiri #251: AppImage or Flatpak Version [#132](https://github.com/AndreKurait/asagiri/issues/132)
- [ ] Verify Kiri #252: Linux: 0.2.0 .deb fails to install on Ubuntu 22.04 (requires libc6 >= 2.39) [#133](https://github.com/AndreKurait/asagiri/issues/133)
- [ ] Verify Kiri #257: GET /distribution/{name}/json is missing, so image-update checks in Docker tools fail [#134](https://github.com/AndreKurait/asagiri/issues/134)
- [ ] Verify Kiri #261: Linux: bridge containers can't reach Tailscale/VPN networks (NAT only on default-route NIC, bridges in no firewalld zone) [#135](https://github.com/AndreKurait/asagiri/issues/135)
- [ ] Verify Kiri #266: kiri import --list fails on a fresh machine when the default source context does not exist. On macOS, `asagiri import --list` with an empty home and Docker config exited 1 with `nothing to import from`. Windows Docker Desktop migration remains unverified. [#136](https://github.com/AndreKurait/asagiri/issues/136)
- [ ] Verify Kiri #268: [Bug] Docker image/container import fails on Windows [#137](https://github.com/AndreKurait/asagiri/issues/137)
- [ ] Verify Kiri #269: Windows: expose the engine socket to other WSL distros (WSL integration) [#138](https://github.com/AndreKurait/asagiri/issues/138)
- [ ] Verify Kiri #277: Windows: containers fail on a stock WSL2 — Kiri Linux boots a hybrid cgroup tree, so kiri-init exits 1 [#139](https://github.com/AndreKurait/asagiri/issues/139)
- [ ] Verify Kiri #281: [Windows] RTL languages (Arabic and Persian) cause blank UI after restart [#140](https://github.com/AndreKurait/asagiri/issues/140)
- [ ] Verify Kiri #288: Dockerfile build fails with 101: Network is unreachable [#141](https://github.com/AndreKurait/asagiri/issues/141)
- [ ] Verify Kiri #294: Embedded DNS answers from the first-joined network, so Compose service aliases resolve to another project's container. The Docker and Asagiri live DNS-order checks passed in [Linux CI](https://github.com/AndreKurait/asagiri/actions/runs/44605224160/job/135847175095). Check explicit gateway priority, Swarm, and application-level behavior before closure. [#142](https://github.com/AndreKurait/asagiri/issues/142)
- [ ] Verify Kiri #297: Windows: setup fails with WSL_E_WSL_OPTIONAL_COMPONENT_REQUIRED - the "Subsystem for Linux" optional component is never enabled [#143](https://github.com/AndreKurait/asagiri/issues/143)
- [ ] Verify Kiri #303: Trust the system certificate store for registries (corporate root CA, UnknownIssuer) [#144](https://github.com/AndreKurait/asagiri/issues/144)
- [ ] Verify Kiri #310: Files changed on the Mac send no inotify events into containers (hot reload doesn't fire) [#145](https://github.com/AndreKurait/asagiri/issues/145)
- [ ] Verify Kiri #311: Linux: immediate docker exec fails with unknown user after starting a container with a fresh named volume [#146](https://github.com/AndreKurait/asagiri/issues/146)
- [ ] Verify Kiri #315: Support macOS 13 (Ventura): 0.2.1 x86_64 app requires macOS 14 even though the binary targets 10.12 [#147](https://github.com/AndreKurait/asagiri/issues/147)
- [ ] Verify Kiri #316: guest stopped before connecting: Some("the agent exited: exit status: 1") [#148](https://github.com/AndreKurait/asagiri/issues/148)
- [ ] Verify Kiri #319: Mac: containers can't reach hosts behind a split-tunnel VPN [#149](https://github.com/AndreKurait/asagiri/issues/149)
- [ ] Verify Kiri #323: Install through Homebrew. The November 15 Homebrew receipt confirms Asagiri 0.1.5 was installed from `andrekurait/asagiri`. Homebrew fetched the cask successfully. The cask declares a quit action and has no zap stanza. Clean install and uninstall remain unverified. [#150](https://github.com/AndreKurait/asagiri/issues/150)
- [ ] Verify Kiri #324: Linux: a signal can reach a reused process ID after a container exits [#151](https://github.com/AndreKurait/asagiri/issues/151)
- [ ] Verify Kiri #326: Adjust swap memory [#152](https://github.com/AndreKurait/asagiri/issues/152)
- [ ] Verify Kiri #329: Support insecure-registries (plain-HTTP and self-signed private registries), and read Docker's daemon.json [#153](https://github.com/AndreKurait/asagiri/issues/153)
- [ ] Verify Kiri #330: Linux: containers have no DNS on the default bridge; Compose/BuildKit builds can't resolve a digest-pinned base; bun install gets ConnectionRefused [#154](https://github.com/AndreKurait/asagiri/issues/154)
- [ ] Verify Kiri #336: Windows: would you consider a libkrun/WHP VM backend instead of a WSL 2 distro? [#155](https://github.com/AndreKurait/asagiri/issues/155)
- [ ] Verify Kiri #337: macOS 26: Cannot move the window [#156](https://github.com/AndreKurait/asagiri/issues/156)
- [ ] Verify Kiri #338: Can't execute commands properly inside built in container terminal [#157](https://github.com/AndreKurait/asagiri/issues/157)
- [ ] Verify Kiri #340: Unable to copy & paste anything from/to textfield in setting panel [#158](https://github.com/AndreKurait/asagiri/issues/158)
- [ ] Verify Kiri #342: Manage compose yamls in Kiri dashboard [#159](https://github.com/AndreKurait/asagiri/issues/159)
- [ ] Verify Kiri #344: macOS 14 (Apple silicon): every linux/amd64 container aborts with Rosetta "Failed to find vdso DT_HASH" [#160](https://github.com/AndreKurait/asagiri/issues/160)
- [ ] Verify Kiri #345: kiri import copies a volume while a running container is writing to it, and reports done [#161](https://github.com/AndreKurait/asagiri/issues/161)
- [ ] Verify Kiri #346: `/info` returns an empty `SystemTime`, which breaks attached `docker compose up` and `docker compose logs -f` on Compose 5.7+ [#162](https://github.com/AndreKurait/asagiri/issues/162)
- [ ] Verify Kiri #349: Setup fails with "no account named <user>" for directory (non-/etc/passwd) users [#163](https://github.com/AndreKurait/asagiri/issues/163)
- [ ] Verify Kiri #350: macOS: while the VM runs, host traffic loses the DF bit (pf 'scrub ... no-df' on the primary NIC), breaking large TLS 1.3 ClientHellos on some networks [#164](https://github.com/AndreKurait/asagiri/issues/164)
- [ ] Verify Kiri #354: UI: squircle corners, rounded font and flat neutral palette (proposal) [#165](https://github.com/AndreKurait/asagiri/issues/165)
- [ ] Verify Kiri #355: Settings → Storage: choose where Kiri keeps images, containers and volumes (for example an external drive) [#166](https://github.com/AndreKurait/asagiri/issues/166)
- [ ] Verify Kiri #356: macOS on Apple silicon: `--gpus all` as a private channel to a GPU server on the Mac [#167](https://github.com/AndreKurait/asagiri/issues/167)
- [ ] Verify Kiri #358: e2e has no checks for healthchecks, Compose depends_on conditions, up --wait or up --build. All eight live Compose cases passed in [Linux CI](https://github.com/AndreKurait/asagiri/actions/runs/44605224160/job/135847175095), including build/wait and healthy, completed, and failed dependency conditions. Run the fixture on native macOS and Windows WSL before closure. [#168](https://github.com/AndreKurait/asagiri/issues/168)
- [ ] Verify Kiri #359: 有新版本推送（0.2.1），我下载之后我选择了立即重启，但是立即重启之后一致循环一下的页面 [#169](https://github.com/AndreKurait/asagiri/issues/169)
- [ ] Verify Kiri #362: Workload numbers on Apple silicon (Kiri 0.2.1 vs OrbStack), and the three "target" rows [#170](https://github.com/AndreKurait/asagiri/issues/170)
- [ ] Verify Kiri #365: Unable to mount folders outside $HOME [#172](https://github.com/AndreKurait/asagiri/issues/172)
- [ ] Verify Kiri #367: Mac: containers have no working HTTP/HTTPS when a security network extension is active (DNS answers 198.18.x, TCP reset) [#173](https://github.com/AndreKurait/asagiri/issues/173)
- [ ] Verify Kiri #368: macOS: setting to stop the engine when Kiri quits [#174](https://github.com/AndreKurait/asagiri/issues/174)
- [ ] Verify Kiri #369: docker command not bindend to kiri [#175](https://github.com/AndreKurait/asagiri/issues/175)
- [ ] Verify Kiri #370: Non-root container with a missing WorkingDir fails to start (os error 2); breaks DDEV [#176](https://github.com/AndreKurait/asagiri/issues/176)
- [ ] Verify Kiri #375: kiri import: containers using --network container:<id> fail with "conflicting options: hostname and the network mode" and drop container options [#177](https://github.com/AndreKurait/asagiri/issues/177)
- [ ] Verify Kiri #376: kiri import: volume import fails when source lacks 5 hardcoded helper images, and container import leaves empty volumes marked "Already in Kiri" [#178](https://github.com/AndreKurait/asagiri/issues/178)
- [ ] Verify Kiri #377: kiri import: containers using digest-pinned untagged images (repo@sha256:...) fail with "No such image" [#179](https://github.com/AndreKurait/asagiri/issues/179)
- [ ] Verify Kiri #378: Engine: --security-opt systempaths=unconfined is ignored, breaking bubblewrap (bwrap) inside non-privileged containers [#180](https://github.com/AndreKurait/asagiri/issues/180)
- [ ] Verify Kiri #379: Please add docker-credential-osxkeychain [#181](https://github.com/AndreKurait/asagiri/issues/181)
- [ ] Verify Kiri #381: macOS: no IPv4 default route when an internal network occupies eth0 [#182](https://github.com/AndreKurait/asagiri/issues/182)
- [ ] Verify Kiri #382: macOS bind mount: chown succeeds but leaves the file owned by root [#183](https://github.com/AndreKurait/asagiri/issues/183)
- [ ] Verify Kiri #383: macOS: VM's NAT DNS proxy (mDNSResponder on port 53) blocks Cloudflare WARP with CF_DNS_PROXY_FAILURE [#184](https://github.com/AndreKurait/asagiri/issues/184)
- [ ] Verify Kiri #384: macOS: a setting for the VM disk size (data.img is fixed at 64 GiB) [#185](https://github.com/AndreKurait/asagiri/issues/185)
- [ ] Verify Kiri #385: Engine: `name` filter is not a regex (`^[/]?name$` matches nothing), so `k3d cluster create` fails [#186](https://github.com/AndreKurait/asagiri/issues/186)
- [ ] Verify Kiri #387: exec start / attach answer 101 to clients that didn't ask to upgrade; Testcontainers reads no ExitCode [#187](https://github.com/AndreKurait/asagiri/issues/187)
- [ ] Verify Kiri #394: Windows: launching Kiri at login undoes a Stop [#188](https://github.com/AndreKurait/asagiri/issues/188)
- [ ] Verify Kiri #395: Windows: leftovers from the WSL restart fix (#318) [#189](https://github.com/AndreKurait/asagiri/issues/189)
- [ ] Verify Kiri #399: macOS bind mounts: a file deleted or replaced on the Mac gives ENOENT in containers for about 1 s (reading it, or creating it again) [#190](https://github.com/AndreKurait/asagiri/issues/190)
- [ ] Verify Kiri #404: Gvisor support with runsc runtime [#191](https://github.com/AndreKurait/asagiri/issues/191)
- [ ] Verify Kiri #405: Linux: close the window to the tray, as on Windows [#192](https://github.com/AndreKurait/asagiri/issues/192)
- [ ] Verify Kiri #407: Container created with only NetworkingConfig.EndpointsConfig also joins bridge; network aliases don't resolve (breaks Testcontainers WithNetworkAliases) [#193](https://github.com/AndreKurait/asagiri/issues/193)
- [ ] Verify Kiri #408: Kiri `import` command not working [#194](https://github.com/AndreKurait/asagiri/issues/194)
- [ ] Verify Kiri #409: Unpack fails when a hard link targets a symlink (breaks Nix images such as supabase/postgres) [#195](https://github.com/AndreKurait/asagiri/issues/195)
- [ ] Verify Kiri #410: Docker Desktop import drops each container's writable layer (data outside volumes is lost); docker diff fails with chroot error [#196](https://github.com/AndreKurait/asagiri/issues/196)
- [ ] Verify Kiri #411: Docker Desktop import: containers with /host_mnt bind-mount sources are skipped silently [#197](https://github.com/AndreKurait/asagiri/issues/197)
- [ ] Verify Kiri #412: Copy-up of a file from an image layer changes its creation time: Elasticsearch fails with 'Underlying file changed by an external force' on node.lock [#198](https://github.com/AndreKurait/asagiri/issues/198)
- [ ] Verify Kiri #413: Privileged systemd container with --cgroupns host and /sys/fs/cgroup bind exits 255 with no logs [#199](https://github.com/AndreKurait/asagiri/issues/199)
- [ ] Verify Kiri #414: A container whose first start fails leaves its anonymous volume partly filled, and later starts reuse it [#200](https://github.com/AndreKurait/asagiri/issues/200)
- [ ] Verify Kiri #415: macOS bind mounts: reading many small files is 3–6× slower than OrbStack, and turning the balloon off barely helps [#201](https://github.com/AndreKurait/asagiri/issues/201)
- [ ] Verify Kiri #416: Named volume is only partly populated when the container is reported started [#202](https://github.com/AndreKurait/asagiri/issues/202)
- [ ] Verify Kiri #417: Volumes not mounted or appear empty [#203](https://github.com/AndreKurait/asagiri/issues/203)
- [ ] Verify Kiri #418: Kiri refuses to disconnect a container from its first network, and .NET Aspire (DCP) fails [#204](https://github.com/AndreKurait/asagiri/issues/204)
- [ ] Verify Kiri #420: Linux host: engine creates /run/host-services as root 0700, so Docker Desktop for Linux can't start [#205](https://github.com/AndreKurait/asagiri/issues/205)
- [ ] Verify Kiri #424: Windows: The interface gets stuck after installation [#206](https://github.com/AndreKurait/asagiri/issues/206)
- [ ] Verify Kiri #425: `--add-host host.docker.internal:<ip>` is ignored: Kiri's own entry comes first in /etc/hosts [#207](https://github.com/AndreKurait/asagiri/issues/207)
- [ ] Verify Kiri #426: Windows: Docker Desktop fails to start ("Access is denied" on \.\pipe\docker_engine) while Kiri serves the pipe [#208](https://github.com/AndreKurait/asagiri/issues/208)
- [ ] Verify Kiri #427: Windows: Kiri opens on "Install WSL" when WSL is installed but slow to answer [#209](https://github.com/AndreKurait/asagiri/issues/209)
- [ ] Verify Kiri #432: Linux host on kernel 5.15: containers get no network ("kiri-init: no uplink: Invalid argument") [#210](https://github.com/AndreKurait/asagiri/issues/210)
- [ ] Verify Kiri #434: buildx build --platform linux/amd64 on Apple Silicon produces an arm64 image labelled amd64 (no amd64 emulation registered) [#212](https://github.com/AndreKurait/asagiri/issues/212)
- [ ] Verify Kiri #435: Containers have no internet: VM never gets IPv4/uplink lease when another app binds UDP 67 [#213](https://github.com/AndreKurait/asagiri/issues/213)
- [ ] Verify Kiri #436: let me choose a share name in the desktop app [#214](https://github.com/AndreKurait/asagiri/issues/214)
- [ ] Verify Kiri #440: Images: a damaged images.json is repaired without a warning, and the image made last drops out of the list. On macOS, `raw images` from `6f93083d` retained the image, reported the backup path, and restored the clean index byte for byte. Native Windows image-store recovery passed in job 135971797426. CLI warning visibility remains open. [#215](https://github.com/AndreKurait/asagiri/issues/215)
- [ ] Verify Kiri #441: macOS: ~/.kiri/daemon.log has no size limit and keeps growing [#216](https://github.com/AndreKurait/asagiri/issues/216)
- [ ] Verify Kiri #443: Windows (WSL 2): a second Kiri engine in another distro takes over the first one's bridge and firewall table [#217](https://github.com/AndreKurait/asagiri/issues/217)
- [ ] Verify Kiri #449: A container on the default bridge that joins a user network can't resolve that network's names (k3d's load balancer fails) [#219](https://github.com/AndreKurait/asagiri/issues/219)
- [ ] Verify Kiri #450: Engine: `--volumes-from` is ignored without a word, so backups made with it are empty [#220](https://github.com/AndreKurait/asagiri/issues/220)
- [ ] Verify Kiri #455: User-defined network: embedded resolver does not fall back when the first --dns server refuses (default bridge does) [#221](https://github.com/AndreKurait/asagiri/issues/221)
- [ ] Verify Kiri #456: Builds from `docker compose build` never appear in the Builds view (they run on buildx_buildkit_default, not buildx_buildkit_kiri) [#222](https://github.com/AndreKurait/asagiri/issues/222)
- [ ] Verify Kiri #465: docker load cannot read OCI layout archives (index.json / blobs). Reproduced through `docker load` with an OCI-only archive on the installed engine. It returned `no manifest.json`. PR #294 passed 15 archive tests (including rejection of incorrect descriptor sizes without publishing an image), 33 HTTP tests, and live ARM64 Linux loading with AMD64 and ARM64 selected through the API. The local integration with #293 passed compressed OCI loading and all ten gzip regression cases. Candidate `b3594a88` passed live macOS `docker load --platform` for AMD64 and ARM64 with API 1.48 negotiation. On `5749af88`, the locally signed macOS engine loaded a real tagged Buildx OCI export for ARM64 and AMD64. Docker inspection confirmed the full tag and requested architecture for both. Hosted Linux CI and merge remain open. [#224](https://github.com/AndreKurait/asagiri/issues/224)
- [ ] Verify Kiri #467: docker rename leaves stale DNS names on user-defined networks. The local baseline failed to resolve the new name. PR #300 passed live checks for new and old names plus aliases scoped to two attached networks. Hosted Linux and release verification remain open. [#225](https://github.com/AndreKurait/asagiri/issues/225)
- [ ] Verify Kiri #471: Coolify support: engine gaps left after #253 (`--pid host`, idle `builder prune`) [#226](https://github.com/AndreKurait/asagiri/issues/226)
- [ ] Verify Kiri #473: macOS: Kubernetes LoadBalancer ports below 1024 are never published unless `--lan on` (binding 127.0.0.1 needs root) [#227](https://github.com/AndreKurait/asagiri/issues/227)
- [ ] Verify Kiri #474: The Kubernetes node is killed "for memory" with no reason logged, so a real out-of-memory kill can't be told from Kiri's own restart [#228](https://github.com/AndreKurait/asagiri/issues/228)
- [ ] Verify Kiri #477: WSL 2 Linux host: user-defined bridge networks fail at gateway DNS bind (EINVAL) [#229](https://github.com/AndreKurait/asagiri/issues/229)
- [ ] Verify Kiri #478: Feature request: run WASI/Wasm workloads (`--platform wasi/wasm`) [#230](https://github.com/AndreKurait/asagiri/issues/230)
- [ ] Verify Kiri #485: kiri import: named volumes lose extended attributes (Supabase Storage returns 500 ENODATA) [#231](https://github.com/AndreKurait/asagiri/issues/231)
- [ ] Verify Kiri #487: Windows: file changes on C:\ bind mounts fire no events in containers [#232](https://github.com/AndreKurait/asagiri/issues/232)
- [ ] Verify Kiri #488: Share containers on your own domain with optional Cloudflare Tunnel [#233](https://github.com/AndreKurait/asagiri/issues/233)
- [ ] Verify Kiri #489: Linux: default bridge takes 172.17.0.0/16 while docker0 already owns it — containers cannot reach their own gateway. In a disposable ARM64 Linux namespace, Docker 28.0.4 owned `172.17.0.0/16` before Asagiri started. Verified artifact `96be364d` selected `172.18.0.0/16` on two daemon starts, and Docker’s route survived both shutdowns. The nested fixture could not verify container-to-gateway traffic. PR [#303](https://github.com/AndreKurait/asagiri/pull/303) adds a hosted Docker-first fixture that checks both gateways and route preservation across restarts. The local fixture at `a6595251`, using binaries built from identical engine sources at `087fd677`, stopped during setup because Alpine package index retrieval failed over TLS. Hosted gateway qualification remains pending. [#234](https://github.com/AndreKurait/asagiri/issues/234)
- [ ] Verify Kiri #494: nfs support [#235](https://github.com/AndreKurait/asagiri/issues/235)
- [ ] Verify Kiri #495: Windows: Blank screen on launch - assembled page exceeds WebView2 NavigateToString 2MB limit. PR #301 routes both documents through the in-memory response handler. Native WebView2 job `135999185324` is queued. [#236](https://github.com/AndreKurait/asagiri/issues/236)
- [ ] Verify Kiri #499: Windows Server support for production use? [#237](https://github.com/AndreKurait/asagiri/issues/237)
- [ ] Verify Kiri #509: Containers on a user network get TCP resets under sustained load (Supabase local stack: Kong → PostgREST) [#238](https://github.com/AndreKurait/asagiri/issues/238)
- [ ] Verify Kiri #510: Windows Defender quarantines kiri-win.exe from 0.2.1 x64 as Bearfoos.A!ml [#239](https://github.com/AndreKurait/asagiri/issues/239)
- [ ] Verify Kiri #516: Apache Iggy can't start: the host-boundary filter blocks io_uring, and `seccomp=unconfined` / `--privileged` can't lift it [#240](https://github.com/AndreKurait/asagiri/issues/240)
- [ ] Verify Kiri #517: [Feature Request] Allow using an existing WSL distribution instead of installing a new one [#241](https://github.com/AndreKurait/asagiri/issues/241)
- [ ] Verify Kiri #518: Floci doesn't run the floci-ui sidecar container [#242](https://github.com/AndreKurait/asagiri/issues/242)
- [ ] Verify Kiri #587: Kiri doesn't render UI [#285](https://github.com/AndreKurait/asagiri/issues/285)
- [ ] Verify Kiri #586: Windows terminal loses input focus during refresh. The [native regression runbook](../tests/native-window-regressions.md#windows-terminal-focus-during-status-refresh-286) defines the WebView2 check. Native execution remains pending. [#286](https://github.com/AndreKurait/asagiri/issues/286)

### Asagiri implementation and qualification (12)

- [ ] Keep idle TCP connections alive while bounding stalled relay cleanup (Kiri #388) [#86](https://github.com/AndreKurait/asagiri/issues/86)
- [ ] Prevent container file browsing from blocking the shared agent (Kiri #521) [#87](https://github.com/AndreKurait/asagiri/issues/87)
- [ ] Verify and repair Windows host-relay teardown and connection ownership (Kiri #515) [#88](https://github.com/AndreKurait/asagiri/issues/88)
- [ ] Bound guest replies without silently losing records or restarting the VM (Kiri #463) [#89](https://github.com/AndreKurait/asagiri/issues/89)
- [ ] Define and verify mixed none/custom network endpoint behavior. Hosted Linux job `135935807043` recorded Docker create/start outcomes and interface addresses across repeated fixture runs. The fixture covers four request variants per special network. PR [#298](https://github.com/AndreKurait/asagiri/pull/298) adds 52 Docker reference requests covering network mode and endpoint name/ID combinations across two repetitions. Hosted reference job `135992128917` recorded 56 creates, 13 successful starts, 42 HTTP 400 start responses and one HTTP 500. Successful starts showed two different interface counts and three sets of inspected endpoints. All 11 checks in the [live API compatibility fixture](verification/api-asagiri-arm64-087fd677.json) passed on disposable ARM64 Linux at `087fd677`. Asagiri rejected mixed host and none endpoints before creating containers or volumes. Keep the explicit pre-creation rejection for the current engine. The Docker successes expose differing interfaces and inspected endpoints, so successful start alone does not establish an attachment contract to implement. Before changing that behavior, identify a reproducible request variant whose complete attachment semantics can be preserved, then verify it through the live engine. [#265](https://github.com/AndreKurait/asagiri/issues/265)
- [ ] Verify and fix gzip-compressed docker load (Kiri #528). [PR #293](https://github.com/AndreKurait/asagiri/pull/293) adds streaming gzip support and five live API cases. All 33 HTTP module tests passed locally. Baseline via `docker --context asagiri load` on `27.5.1-asagiri`: gzip exited 1 with `failed to read entire block`; the same tar loaded successfully. The test tag was removed. All ten live cases passed on a disposable ARM64 Linux engine, including preservation of an existing image tag after a corrupt upload. Docker 28.0.4 published images in all six reference cases, including damaged checksum, truncated trailer, and bzip2. Asagiri rejects these three cases with HTTP 400. Hosted Linux job `135971488693` passed and PR #293 merged as `7100bdff`. On isolated macOS at `202b4626`, `docker image load` accepted a real Alpine gzip archive with a `.payload` filename and `docker run --pull never` ran its command. Native Windows CLI qualification remains open. [#266](https://github.com/AndreKurait/asagiri/issues/266)
- [ ] Define supported behavior when the host blocks clone3 (Kiri #529) [#267](https://github.com/AndreKurait/asagiri/issues/267)
- [ ] Review and qualify an Arch desktop package (Kiri PR #526) [#268](https://github.com/AndreKurait/asagiri/issues/268)
- [ ] Define a self-contained headless Linux package (Kiri #532) [#269](https://github.com/AndreKurait/asagiri/issues/269)
- [ ] Verify download consistency during same-source Pages refreshes. Local tests cover both stale index and stale feed diagnostics. PR #295 adds publication identity verification and passes 11 local tests. Merge and release-triggered verification remain open. [#274](https://github.com/AndreKurait/asagiri/issues/274)
- [ ] Measure native Mac networking and file sharing before kernel-bypass work [#275](https://github.com/AndreKurait/asagiri/issues/275)
- [ ] Verify and port the nine reports covered by Kiri Docker compatibility PR 588. PR #281 merged the process-reporting change for Kiri #348. Paired Docker and Asagiri live fixtures passed in Linux platform run 44622598968. PR #282 merged unpublished TCP and UDP port inspection after live ARM64 and hosted Linux checks passed. Custom bridge-name handling also passed its Linux fixture. Four report-level acceptance checks remain open. The macOS CLI built from main retained the image and preserved the original bytes during index recovery. A signed macOS fixture verified Docker exec inspection after two VM stops [#280](https://github.com/AndreKurait/asagiri/issues/280)

## 3. Expand Verus and retire covered tests one change at a time

The [pinned runtime verification report](verification/runtime-proof-d74d0120.json) records nine passing obligations on clean `main` at `d74d0120`, covering health transitions, HTTP numeric parsing, and layer admission. A [fresh run on clean main at `ac4a738f`](verification/runtime-proof-ac4a738f.json) passed after the quota removal merged.

The pinned Verus runtime harness passed on clean updated PR heads. Health transition PR #291 at `a3d95d07` verified two obligations and rejected eight broken variants. It removes two sampled unit tests and keeps the status/log wiring and startup interval checks. Nine remaining health unit tests and seven live macOS cases passed. HTTP number padding PR #292 at `0042060e` verified seven obligations and rejected six broken variants. It removes one sampled unit test with 4,096 iterations. All 32 remaining HTTP tests passed. Hosted checks and verification after merge remain pending.

- [ ] Confirm the health-transition and HTTP zero-padding proofs from PRs #291 and #292 pass on merged `main`.
- [x] Establish a reproducible baseline by crate. Revision `242a64ac` has 1,069 literal `#[test]` declarations under `crates/`.

  | Crate | Declarations |
  | --- | ---: |
  | asagiri | 655 |
  | asagiri-image | 94 |
  | asagiri-init | 212 |
  | asagiri-proto | 5 |
  | asagiri-share-proto | 14 |
  | asagiri-vmm | 12 |
  | asagiri-win | 77 |

  Merged `main` at `7100bdff` has 1,074 declarations. The net increase of five is in `asagiri`. The three proposed unit-test removals in #291 and #292 remain pending.

  Recount with `git grep -n -E '^[[:space:]]*#\[test\]' 242a64ac -- 'crates/**/*.rs' | cut -d: -f2 | cut -d/ -f2 | sort | uniq -c`. This counts source declarations across platforms. Track integration suites and platform runs separately.
- [ ] Start with issue #80 for HTTP framing and #81 for lifecycle cancellation and layer ownership. Inventory the production callers, requirements, and test assertions for one behavior at a time.
- [ ] State the contract, prove the production implementation, and confirm a deliberately incorrect implementation fails verification.
- [ ] Remove unit tests whose assertions the contract covers. Exercise the normal entry point and retain checks for caller wiring, I/O, platform behavior, or independent compatibility comparisons.
- [ ] Record the proof scope, tests removed, remaining test count, and CI result in each PR. Merge before selecting the next behavior.
- [ ] Track progress toward the requested small remaining fraction and document the behavior that each retained test uniquely exercises.

## Completed before this snapshot

- PR #279 merged. Live engine results resolved issues #278 (exec exit codes), #257 (image labels), and #258 (supplementary groups).
- The PR quota workflow was disabled. Draft #6 was closed after its disposable quota test finished.
