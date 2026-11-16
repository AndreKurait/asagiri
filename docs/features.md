# Features

What Asagiri supports today, through the `asagiri` Docker context.

## Platforms

Apple silicon and Intel Macs (macOS 14+), Windows 10 and 11 (x86-64 and Arm, through WSL 2), and Linux (.deb and .rpm, tested on Ubuntu and Fedora). 
## What works

- **Containers**: `run` (`--rm`, `-d`, `-it`, `-e`, `-u`, `-w`, `--name`,
  `--restart`, `--entrypoint`), `exec`, `logs`, `attach`, `stop`, `kill`,
  `restart`, `rm`, `wait`, `rename`, `pause`/`unpause`, `update`, `stats`,
  `top` (default `-ef` columns), `diff`, `export`, `commit`, `cp` (both ways, running or not),
  `container prune`; records survive a daemon restart; filters on every list;
  healthchecks (`--health-cmd`, `--health-*`, including
  `--health-start-interval`, and `ps --filter health=`)
- **Limits and security**: `--memory`, `--cpus`, `--cpu-shares`,
  `--pids-limit`, `--shm-size`; Docker's default capabilities and a default
  seccomp filter; `--cap-add`/`--cap-drop`, `--privileged`, `--read-only`,
  `--security-opt seccomp=unconfined` or `seccomp=<profile.json>`, and
  `no-new-privileges`. Even privileged and unconfined containers, and those
  with their own profile, keep the host boundary: no vsock sockets (the way
  to the Mac) and no io_uring
- **Networking**: a default bridge, user networks with DNS by name and
  alias, `network connect`/`disconnect`, `--network host|none`, `-p`/`-P`
  (TCP and UDP) on Linux and Mac interfaces (`-p 127.0.0.1:8080:80`
  keeps a port local; Windows bindings are inside WSL, as described below), `--add-host`, `host.docker.internal`
  ([Mac loopback services](mac-host-loopback.md)),
  `--network container:<name>` (Compose `network_mode: service:x`; as with
  Docker, if that container restarts, the ones sharing its network keep the
  old, now empty namespace until they are restarted too), IPAM (`--subnet`,
  `--gateway`, `--ip-range`, `--aux-address`), `--ip`, `--mac-address`,
  `--dns`, `--dns-search`, `--dns-option`, `--domainname`, `--link`,
  `--ipv6` (Compose `enable_ipv6`: an IPv6 subnet beside the IPv4 one, from
  `--subnet` or a /64 of Asagiri's own under fd00::/8; the default bridge stays
  IPv4-only, as Docker's does), and
  `http://<name>.asagiri.local` once you turn it on (the app asks on first open;
  Settings → Domains changes it), or `http://<name>.asagiri.localhost` with no
  setup. Compose services answer at `<service>.<project>.asagiri.local`, and a
  `dev.asagiri.domain=api.myapp` label picks a container's own names; HTTPS too
  (`https://<name>.asagiri.local`), with Asagiri's own certificate authority in
  `~/.asagiri/tls`, whose name constraints allow only `*.asagiri.local` and
  `*.asagiri.localhost` (Settings → Domains → HTTPS asks macOS to trust it)
- **Storage**: named and anonymous volumes, bind mounts of Mac paths
  (virtiofs), `--tmpfs`, `--mount`, `volume` commands; local volumes that bind
  a folder (`driver_opts: {type: none, o: bind, device: /path}`). Mac bind sources in
  `/Users`, `/Volumes`, `/private`, `/tmp`, `/var/folders`, `/opt` and
  `/usr/local` use the Mac's files. Other paths stay in the Linux VM.
  The Mac share uses 1 MiB read-ahead for sequential reads; cache coherence
  and read-only mounts are unchanged. Other filesystems keep their settings.
  New named and anonymous `--mount` volumes keep requested labels and local
  driver options. Existing volumes keep their own metadata.
- **Images**: `pull` (with `docker login` credentials), `push`, `tag`,
  `save`, `load`, [`import`](docker-import.md) (tar/gzip root filesystems, from a file, stdin or an
  HTTP(S) URL), `history`, `search`, `image prune`, `system df`;
  **amd64 images run through Rosetta** on Apple silicon. Intel release builds
  package a [source-built QEMU emulator](qemu-emulation.md) for arm64 images,
  with its matching source archive and dependency licenses. Native Intel Mac
  VM execution remains unverified. `--platform` is honoured. Docker Hub
  pulls (and Dockerfile `FROM`s) can go through pull-through mirrors, in
  order, from Settings → Registry mirrors or `asagiri registry-mirror`; other
  registries are pulled directly
- **Registry configuration** — `asagiri config` saves validated private-registry
  exceptions, mirrors and per-pull/push transfer limits. See
  [Registry configuration](registry-configuration.md).
- **Builds**: `docker build` / `buildx build` through the
  [built-in Docker driver](buildkit.md), with direct image loading, secrets,
  caching, file exports and cancellation. The existing docker-container
  builder shares the managed BuildKit and cache. BuildKit stops when idle;
  image loading supports one platform per build. On Apple silicon, amd64
  build steps run through Rosetta too
- **Compose**: `up` (including `--build` and `--wait`), `down`, `ps`, `logs`,
  `exec`, `build:`, healthchecks, `depends_on` conditions
  (`service_healthy`, `service_completed_successfully`), `platform:`,
  multiple networks, events
- **USB devices (beta)**: the USB tab hands a device (a serial adapter, a
  Zigbee stick, a development board) to your containers on Mac, Windows and
  Linux, and shows where it is inside Linux with the `--device` line to
  copy. On a Mac, a device that drops off the bus while attached (the board
  resets, or the Mac sleeps) is attached again when it comes back
- **Mac integration**: container files at `~/Asagiri` in Finder (read-only
  NFS, always current), SSH agent forwarding at
  `/run/host-services/ssh-auth.sock`
- **Memory on macOS**: a balloon gives unused memory back while running,
  reserves active and mapped file cache, and waits 30 seconds after memory
  pressure or substantial cache refaults before shrinking again. Cold,
  unmapped cache remains reclaimable. The
  VM sleeps after 60 s idle (the daemon stays at ~15 MB; waking takes
  ~0.15 s). On Windows, Stop stops Asagiri's engine and containers; WSL can
  remain running and retain memory. The stopped engine's memory reading
  does not measure the resident WSL VM.
- **The window**: containers, images, volumes and networks, with Compose
  projects grouped (start, stop, restart or remove a whole project); each
  container has its own page with details, live logs (find, level filter),
  a command line (and "Open in Terminal" for a full shell) and its files
  (preview, Save to Downloads, Show in Finder); Settings for fonts, text
  size, the VM's CPUs and memory (`~/.asagiri/vm.json`, used from the VM's
  next start; `ASAGIRI_CPUS` and `ASAGIRI_MEMORY_MIB` override it) and local names
- **AI clients**: an [MCP server over local stdio](integrations.md), disabled
  until enabled in Settings or the CLI. Read permissions cover bounded
  container information and recent logs; starting, stopping and restarting
  existing containers requires a separate permission.
- **Local browser**: `asagiri web` serves a password-protected interface on
  loopback after explicit setup. Browse containers, images, volumes and
  networks, inspect bounded metadata, and read recent logs. Start, stop and
  restart require a separate permission. Access is independent of MCP;
  see [setup and permissions](integrations.md#local-browser).
- **Menu bar**: Asagiri stays in the menu bar when its window closes, with the
  VM's state, running containers (click one to open it) and Start/Stop
- **Kubernetes**: `asagiri k8s start` — a single-node k3s cluster in one
  container, ready in ~20 s, as the `asagiri` kubectl context
- **Linux machines**: `asagiri machine create dev --distro ubuntu` and
  `asagiri machine shell dev` — your user, sudo, and `/Users` at the same path
- **The app**: `cargo xtask app` builds `Asagiri.app` with the `docker` CLI,
  Compose and buildx inside; opening it runs `asagiri setup` (docker context,
  LaunchAgent, build builder) and the window; `asagiri update` installs new
  releases after checking their SHA-256 and signature
- **Linux hosts**: the same `asagiri` with no VM — the agent runs on the host's
  kernel (`cargo xtask linux`, `contrib/linux/asagiri.service`); arm64 and
  x86-64; `tests/linux-host.sh` checks it. Packaged as `.deb` and `.rpm`
  (`packaging/linux`): `asagiri` with the docker CLI, Compose and buildx, or
  `asagiri-lite` for the engine alone. Membership of the `asagiri` group is
  root-equivalent, exactly like the `docker` group
- **Windows**: the x86-64 Linux build in a WSL2 distro, and `asagiri-win.exe`,
  which serves the API on `npipe:////./pipe/asagiri` (the `asagiri` docker
  context) and carries it into WSL over one `wsl.exe … asagiri bridge`, so it
  needs no change to WSL's or Windows' network settings (`cargo xtask
  msi` builds `target/Asagiri-<version>-x86_64.msi`, a per-user installer
  around `target/asagiri-windows-x86_64`; `asagiri.ps1` in that folder installs
  the same without an MSI). Not TCP:
  WSL's localhost relay drops half-closes, and `docker run` loses its
  output

Kubernetes LoadBalancer services are published on `localhost`, and
`asagiri machine create NAME --systemd` boots systemd as the machine's init.

`docker top` accepts its default columns; other `ps_args`, such as `aux` or
`-eo pid,rss,args`, return an explicit error. Custom process columns and RSS
reporting through `top` are not implemented.

Resource limits apply to individual containers. Buildx's default
`/docker/buildx` parent groups its containers under `docker/buildx` inside
Asagiri's delegated cgroup tree. Its aggregate counters and limits are
available there; container updates, pause, exec and stats use their child
cgroups. Other aggregate parent cgroups
(`--cgroup-parent` / `HostConfig.CgroupParent`) and user namespace remapping
(`--userns=private` or `auto`) are not implemented. Asagiri rejects those
requests with HTTP 400. An omitted user namespace setting or `--userns=host`
uses the engine's existing user namespace. Container root is not remapped to
an unprivileged host UID. Custom bridge interface names
(`com.docker.network.bridge.name`) also return HTTP 400; the engine owns its
bridge names. Inter-container communication controls (`enable_icc`) are
supported on IPv4 and IPv6 bridge networks.

On Windows, ports bind inside WSL. A wildcard binding (`0.0.0.0`, `::`, or
an omitted address) produces a warning because it does not configure Windows
LAN forwarding. Explicit non-loopback host addresses return a clear
unsupported-address error. Windows reachability depends on WSL's networking
configuration; Asagiri does not install Windows forwarding or firewall rules.

## Releases

Asagiri releases are hosted on GitHub, with verified download links on the
[installation page](https://andrekurait.github.io/asagiri/install/). Current
Mac packages are ad hoc signed and not notarized by Apple. The release
workflow supports [Developer ID signing and notarization](macos-signing.md)
once the project's Apple credentials are configured. Every file and
feed names its architecture: `Asagiri-<version>-arm64.dmg`, `Asagiri-<version>-arm64.tar.gz`
and `latest-mac-arm64.json` for Apple silicon (`-x86_64` for Intel Macs),
`Asagiri-<version>-x86_64.msi` / `-arm64.msi` with `latest-windows-x86_64.json` /
`latest-windows-arm64.json` for Windows. Each app reads only its own feed. The published DMGs bundle the Docker
CLI, Compose, and Buildx. `cargo xtask app-lite` can build a local smaller app
that downloads pinned CLI versions on first launch, but this is not a separate
published Mac release asset.

## Known issues

- **Mac: an attached USB device leaves your containers while the Mac
  sleeps.** macOS takes USB devices off the bus when it sleeps, so a device
  such as `/dev/ttyACM0` is gone from containers until the Mac wakes. Asagiri
  then attaches it again by itself. Asagiri doesn't keep the Mac awake for a
  device, since that would drain a laptop's battery. For a long job that
  runs unattended (flashing boards, logging a serial port), keep the Mac
  awake while it runs: `caffeinate -s` in a terminal (on power), or turn on
  "Prevent automatic sleeping when the display is off" in System Settings
  (Battery › Options on a laptop, Energy on a desktop Mac).

- **`host-gateway` requires the built-in Docker driver.** Use
  `asagiri build --add-host name:host-gateway .` after setup, or explicitly
  select the Asagiri context's Docker builder. Buildx still rejects this
  special value for an explicitly selected docker-container builder.

- **Multi-platform image loading is not supported.** Build and load each
  platform separately with `--platform`. Multi-platform file exports require
  a docker-container builder. An OCI-only archive cannot be read by Asagiri's
  `docker load`; see [platforms and image loading](buildkit.md#platforms-and-image-loading).

- **A build's `not found` error can require a network check.** The managed
  builder tries Asagiri's local image mirror before the registry. A mirror
  miss and an unreachable registry can hide the underlying network failure.
  Pulling a base image does not guarantee an offline build: registry tags
  and original registry digests can still require registry access. Use an
  already cached image to check DNS on the builder's default bridge, for
  example:
  `docker --context asagiri run --rm --pull=never alpine:3.21 nslookup registry-1.docker.io`.
  A successful DNS lookup alone does not establish registry HTTPS access
  or valid credentials.
