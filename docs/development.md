# Development

Building Asagiri from source, running it, and how it fits together.

The [performance guide](performance.md) covers paired GHA benchmarks,
regression limits, raw samples, and the public history by commit.

## Build

Needs Rust and, for the kernel and disk template, a Docker engine. Asagiri can
build its own kernel: `DOCKER_CONTEXT=asagiri cargo xtask kernel`.

```sh
cargo xtask kernel    # guest/out/Image
cargo xtask disk      # guest/out/data.img (sparse 64 GiB ext4 template)
cargo xtask guest     # guest/out/initramfs.cpio
cargo xtask qemu      # import verified source-built Intel emulator products
cargo xtask host      # target/release/asagiri, signed with the virtualization entitlement
```

For Intel Macs add `--arch x86_64` to `kernel`, `guest`, `host`, `app`,
`dmg` and `release`: an x86-64 guest in `guest/out/x86_64` (a `bzImage`,
cross-compiled on Apple Silicon), the host for `x86_64-apple-darwin`
(`rustup target add x86_64-apple-darwin x86_64-unknown-linux-musl`), and
`target/intel/Asagiri.app` with Intel builds of the docker CLI, Compose and
buildx, released as `Asagiri-<version>-x86_64.dmg`/`.tar.gz` with its own feed,
`latest-mac-x86_64.json`. Each Mac gets its own app rather than one universal
app, which would carry both builds of the docker tools. On Intel there is
no Rosetta for Linux: amd64 images run natively, and arm64 ones through the
static QEMU that `cargo xtask qemu` imports from verified build products.
See [Intel emulation](qemu-emulation.md) for the source build and product paths.
The app bundles it as
`Resources/qemu` and the daemon shares into the guest (`ASAGIRI_QEMU_DIR` points
a checkout elsewhere). Without it, images with no amd64 build are refused
with that reason rather than failing later with `exec format error`.

On Windows, `cargo xtask windows-dev` builds the Linux engine, Asagiri Linux and
Docker tools into `target/asagiri-windows-x86_64` with a debug `asagiri-win.exe`.
`cargo run -p asagiri-win` uses that bundle automatically. Use `cargo xtask
windows` for the release bundle and `cargo xtask msi` for the installer.
Add `--arch arm64` to any of the three for Windows on Arm
(`target/asagiri-windows-aarch64`, `Asagiri-<version>-arm64.msi`); testing it in
a VM on a Mac is in [docs/testing-windows-arm.md](docs/testing-windows-arm.md).

## Use

```sh
./target/release/asagiri daemon &      # serves ~/.asagiri/run/docker.sock; the VM boots on first use
docker context create asagiri --docker host=unix://$HOME/.asagiri/run/docker.sock
docker --context asagiri run --rm alpine echo hello
docker buildx create --name asagiri-build --driver docker-container \
  --driver-opt default-load=true --use asagiri    # for docker build
./target/release/asagiri bench         # Asagiri vs OrbStack vs Docker Desktop
SOAK=600 tests/e2e.sh               # every check, then 600 runs watching for leaks
```

Docker's commands also work as `asagiri` commands, always on Asagiri whatever
docker context is selected: `asagiri run`, `asagiri ps`, `asagiri exec`, `asagiri logs`,
`asagiri build`, `asagiri compose up` and the rest run the docker CLI against
Asagiri's socket. (`asagiri import` is Asagiri's own: it brings images, volumes and
containers over from OrbStack or Docker Desktop; see the [engine import guide](engine-import.md).) The daemon-free tools
that boot their own guest are `asagiri raw run|pull|images|rmi`.

`asagiri` keeps its data in `~/Library/Application Support/Asagiri` (override with
`ASAGIRI_DATA_DIR`). It does not touch `/usr/local/bin/docker`.
`/var/run/docker.sock` changes only when you ask: `asagiri setup --docker-socket`
(or the password prompt for `*.asagiri.local` names) links it to Asagiri's socket
when no other engine has it, after macOS asks for an administrator password,
and installs `/Library/LaunchDaemons/dev.asagiri.docker-socket.plist` to put the
link back after a restart. A socket another engine owns is never replaced.

## Architecture

```
docker CLI ─┐
compose ────┼─ ~/.asagiri/run/docker.sock ─► asagiri daemon ── vsock ──► guest agent (PID 1)
buildx ─────┤     Docker Engine API          owns the VM            clone3 → container
asagiri UI ────┘
```

- **VM**: Apple's Virtualization.framework, driven from Rust through objc2.
  A custom 10 MB kernel (`guest/kernel/`) built from allnoconfig, with one
  patch that cuts virtio-pci reset waits from a scheduler tick to
  microseconds, one that hands big TCP to the host as GSO, one that keeps
  newly created FUSE files owner-readable, and an upstream GRO fix that keeps
  forwarded packets segmentable. Rosetta is shared in over virtiofs and registered with
  binfmt_misc (on Intel, static qemu-aarch64 is, for arm64 images).
- **Guest agent**: a ~900 KB static Rust binary that is PID 1. One `poll()`
  loop serves the host, reaps children, forwards container output, relays
  ports, answers DNS and runs DHCP. `docker cp` and friends run in forked
  children so a large copy never stalls it.
- **Containers**: one `clone3` straight into new namespaces and the
  container's cgroup (`CLONE_INTO_CGROUP`), overlayfs over layers unpacked
  on an ext4 data disk, `pivot_root`, capabilities and a seccomp filter.
  Networking is set up with hand-written rtnetlink and nf_tables messages.
  No runc, no shim, no dockerd.
- **Images**: pulled and pushed on the Mac (`crates/asagiri-image`), streamed
  into the guest once and unpacked there.

Host–agent control requests have a 1 MiB frame budget, including the header.
Custom seccomp JSON shares that budget with arguments, environment variables,
mounts and the other request fields. The daemon checks the complete encoded
request before sending it or queuing its reply. An oversized request fails
locally and leaves the shared connection available to other containers.
Streamed layer bodies travel outside the control frame.

## Layout

| Path | What |
|------|------|
| `crates/asagiri` | the `asagiri` binary: daemon, Docker API, CLI, UI, bench |
| `crates/asagiri-vmm` | Virtualization.framework VM |
| `crates/asagiri-init` | guest PID 1, agent and container runtime |
| `crates/asagiri-proto` | host ↔ guest wire protocol (no_std, no allocation per message) |
| `crates/asagiri-image` | image references, registry client, blob store, archives |
| `guest/kernel` | kernel config and patches |
| `tests/e2e.sh` | ~350 checks through the real `docker` CLI, plus a leak soak (`K8S=1` adds Kubernetes) |
| `tests/linux-host.sh` | the Linux host build, driven in a privileged container |
| `tests/finder-mount.sh` | the Mac's Finder mount at `~/Asagiri`: covers only an empty folder or its own old mount, never your files or another mount |
| `contrib/` | systemd unit, WSL2 and Windows install scripts |
| `installer/windows` | the Windows MSI (WiX 5): `asagiri.wxs`, licence, dialog bitmaps |
| `packaging/` | Linux `.deb` and `.rpm` (`asagiri` with the docker CLI, `asagiri-lite` engine only), the Homebrew plan |
| `xtask` | build tasks |
| `bench/results.json` | measured results; the website keeps a copy |
| `brand/` | brand (the website and raw benchmark output live outside this repository) |

## Unexpected VM stops

The daemon keeps the guest's serial console, kernel oopses included, in
`~/.asagiri/logs/guest-console.log` (`.asagiri-dev` for Asagiri Dev). The file and two
rotations (`.1`, `.2`) hold at most 1 MiB each. Files are private, credential
patterns are redacted, and lines over 16 KiB are omitted. The unexpected-stop
log line names this file. The console stays local; crash reports do not send it.

After a guest crash, Asagiri automatically boots a new VM when containers were
running with `always` or `unless-stopped`, then restarts them and increments
their `RestartCount`. Repeated losses back off from 100 ms to one minute. A
manual stop, kill or removal wins, including during that wait; `no`,
`on-failure` and containers that never ran stay down. If the automatic boot
fails, the next request that needs the VM retries recovery.

Before a replacement opens the data disk, Asagiri confirms that the previous
VM has stopped and releases its resources, even if relay threads still
hold old links. It allows ten seconds for shutdown, then up to ten seconds
for a confirmed forced stop. Errors and timeouts keep the previous VM
owned and block replacement; recovery retries with backoff. This wait does
not hold container lifecycle locks, so stop, kill and removal can cancel
pending starts. Linux gives each guest process its own socket directory,
so late destruction of an old link cannot remove its successor's sockets.

The real-VM regression is an opt-in ignored test:
`vm_loss_live_recovers_without_a_request`. Run its signed test binary with
an isolated `HOME` and `ASAGIRI_DATA_DIR` containing `alpine:latest` and no
containers, `ASAGIRI_GUEST_DIR` pointing to matching guest assets, and
`ASAGIRI_LIVE_VM_RECOVERY=1`. It force-stops only the VM it creates.

### Concurrent Docker API clients

The daemon admits 2048 concurrent API connections by default. Long-lived
container waits, log followers and attached sessions each occupy a connection,
including when no data is flowing. For larger fleets, set the daemon's startup
`ASAGIRI_MAX_CONNECTIONS` environment variable to a decimal integer from 1 to
4096, for example `ASAGIRI_MAX_CONNECTIONS=4096`. Invalid values fail startup.
Changing it requires restarting the daemon; it does not change socket access.

The Linux and WSL systemd services set `TasksMax=infinity`: their cgroups
include container threads as well as the daemon. `docker run --pids-limit`
still limits an individual container. Parent cgroup limits, including an
operator's fleet unit or slice, still apply; inspect `TasksCurrent`,
`TasksMax` and `EffectiveTasksMax` with `systemctl show` when thread or
process creation fails with `EAGAIN`. Connection capacity does not raise a
cgroup task limit.

On an otherwise idle test daemon, `tests/api-connections.py SOCKET IMAGE
--capacity 512 --connection-limit 2048 --only-capacity` holds mixed idle
keepalive and wait connections while checking ping and container inspection.
It also fills the configured limit, checks that excess connections close,
and checks that a released slot can be reused. Set `--connection-limit` to the
daemon's actual limit; this test temporarily occupies all its API slots.
The client also holds one socket per connection. Its shell's file descriptor
limit must exceed `--connection-limit`; for example, run `ulimit -n 4096`
before the 2048-slot command if the shell's hard limit permits.

Budget for the peak number of streams plus ordinary API requests. Each admitted
connection starts an OS thread and holds at least two socket descriptors;
attached sessions may allocate more. Thread stacks reserve virtual address
space (normally 2 MiB per Rust thread, platform dependent), and active requests
also consume resident memory. Raising the bound does not guarantee sufficient
file descriptors or memory. Keep a bounded value appropriate to the host and
leave room for control requests. At the limit new connections are closed.
Disconnected waits and quiet log followers release their connection promptly;
a client that only closes its write half can still receive the response.

### macOS VM disk capacity

Set `ASAGIRI_DISK_GB` in the daemon environment to a positive whole number of GiB, for example `128`. On the next VM start, Asagiri extends the sparse data image and grows its mounted ext4 filesystem before starting containers. A new disk starts at the larger of the template size and the requested size. Lowering or removing the setting preserves the existing capacity.
