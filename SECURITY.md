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

Asagiri currently supports the latest `main` branch. There are no published
Asagiri releases yet. Build from current source before retesting a report.
There is no default update feed or hosted sharing server. Configuring either
opts into infrastructure you choose and trust.

Reports about container or VM isolation, the Engine API socket, networking,
local certificate handling, installers, image archives, and configured updates
are in scope. Coordinate issues inherited from Kiri privately with the
relevant maintainers before public disclosure.
