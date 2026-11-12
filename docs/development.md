# Development

Building Asagiri from source, running it, and how it fits together.

## Build

Needs Rust and, for the kernel and disk template, a Docker engine. Asagiri can
build its own kernel: `DOCKER_CONTEXT=asagiri cargo xtask kernel`.

```sh
cargo xtask kernel    # guest/out/Image
cargo xtask disk      # guest/out/data.img (sparse 64 GiB ext4 template)
cargo xtask guest     # guest/out/initramfs.cpio
cargo xtask qemu      # guest/out/qemu/qemu-aarch64 (Intel: arm64 images)
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
static qemu `cargo xtask qemu` fetches, which the app bundles as
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
containers over from OrbStack or Docker Desktop.) The daemon-free tools
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
