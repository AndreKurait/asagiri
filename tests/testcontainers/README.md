# Java Testcontainers regression

This runs the actual Testcontainers 1.20.3 version reported in upstream
issues #121 and #222, with the MySQL module and a real JDBC driver.
`images.json` pins Alpine, MySQL 8.0 and the SDK's Ryuk 0.9.0 reaper by digest.

From the repository root, after building the native Linux engine:

```sh
sh scripts/ci/testcontainers.sh /path/to/linux-binaries /path/to/maven-cache
```

The second directory is optional and caches only Maven dependencies. The
runner creates its own privileged Linux host, mounts the source and binaries
read-only, publishes no outer ports, and removes its host and engine volumes
when finished. The engine socket stays inside that host. No native macOS
daemon or VM is started.

The Java client uses default startup checks and Ryuk. It verifies image
defaults, copying before start, command execution, two delayed log messages,
both MySQL readiness messages, and a real SQL query through a published
container port. The parent also verifies that a deliberately unclosed
container is reaped after the client JVM exits. Disabling either startup
checks or Ryuk is an assertion failure.

The GHA platform workflow runs this on Linux PRs/main, and release packaging
runs it on native x86_64 and aarch64. These checks exercise the Linux engine;
they do not establish native Mac VM or Windows/WSL parity.
