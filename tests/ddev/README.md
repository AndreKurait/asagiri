# DDEV project regression

The Linux platform and release jobs run a complete DDEV v1.25.5 project on the
engine binaries just built. The project uses PHP 8.4, MariaDB 11.8 and Traefik.
It checks an SQL-backed response over HTTP and certificate-verified HTTPS,
restarts DDEV, checks both paths again, and stops the project and router.
This exercises Compose builds, named volumes, file uploads, container health,
DNS, database connections and router configuration replacement together.

```sh
python3 scripts/ci/ddev.py /path/to/binaries --output /tmp/asagiri-ddev-result
```

The binary directory must contain `asagiri`, `asagiri-init`, `SHA256SUMS` and
`SOURCE_COMMIT` from `scripts/ci/build-linux.sh`. Use a new output directory.
The test downloads a digest-pinned DDEV release archive for the Docker host's
architecture. It preloads digest-pinned DDEV images, checks their identities
again after startup and restart, and verifies that the derived web/database
images preserve their pinned base layers.

All runtime work uses one owned disposable privileged Linux container, a
private engine socket and fresh data volumes. No outer port is published and
no user Docker socket is mounted. DDEV runs as an ordinary user inside that
host. Its local CA, HTTPS keys, test-only sudo configuration and containers
are discarded with that host. HTTP/HTTPS use explicit ports 8080/8443 and
local name resolution inside the private host.

Logs, source and binary hashes, image identities and cleanup status are saved
on success and failure. This checks the Linux integration. It does not
exercise DDEV through the native macOS VM, USB, Colima or WSL networking.
