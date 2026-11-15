# Security

Asagiri runs containers, a Linux VM or WSL engine, and local networking
services. Treat membership in the Linux `asagiri` group like Docker group
membership: it grants powerful access to the host.

## Report a vulnerability privately

Use **Security → Report a vulnerability** in AndreKurait/asagiri, or
https://github.com/AndreKurait/asagiri/security/advisories/new.
Do not disclose an unpatched vulnerability in a public issue or PR.

Include the commit or version, operating system, reproduction steps, impact,
and relevant redacted logs. The maintainer is Andre Kurait
(AndreKurait@gmail.com). Attribution is offered unless you prefer anonymity.

## Supported code

Asagiri's actively maintained code is the latest `main` branch. Stable and
preview builds are available on the [release page](https://github.com/AndreKurait/asagiri/releases).
Include the exact version or commit when reporting, and retest with a current
build when practical.
There is no default update feed or hosted sharing server. Configuring either
opts into infrastructure you choose and trust.

## Docker socket access

Clients that can connect to Asagiri's Docker Engine API socket can control the
engine. This includes containers into which the socket is mounted. Grant this
access only to users and workloads trusted to manage the engine.

On native Linux, membership in the `asagiri` group permits root-level host
access through containers and host bind mounts, like membership in `docker`.
On macOS and Windows, engine access controls the Linux guest and host paths
available to it. Socket access is separate from the narrower, opt-in browser
and MCP permissions described in [local integrations](docs/integrations.md).
See [host access and packaging](packaging/README.md#linux-package-assembly).

## In scope

Reports about container or VM isolation, the Engine API socket, networking,
local certificate handling, installers, image archives, and configured updates
are in scope. Coordinate issues inherited from Kiri privately with the
relevant maintainers before public disclosure.
