# Windows registry download cancellation

This adapts the image transport change from [public Kiri PR #259](https://github.com/libracoder/kiri/pull/259), commit [`99a341254a85cb6b9f6c0f1e3c6f755a471632a2`](https://github.com/libracoder/kiri/commit/99a341254a85cb6b9f6c0f1e3c6f755a471632a2), by AD PAO. That head contains the cancellation change; its former WSL build-distro work is covered separately by PR #314, and its former HTTP connection-close change was removed upstream.

When a Windows pull has a cancellation switch, its registry reads use a nonblocking socket and check the switch every 100 ms. Each read retains the full remaining request timeout, capped by the existing stall timeout. Interrupted reads spend that same budget. Completion, timeout, cancellation, and I/O failure all attempt to restore blocking mode. Other platforms and requests without a cancellation switch retain their existing blocking reads.

Tests exercise real loopback sockets, including cancellation before the stall timeout, a delayed successful read followed by a blocking read, and a timeout followed by another blocking read. The existing POSIX shutdown-interruption test remains; it is bounded by a socket timeout. The end-to-end registry tests require a cancelled partial blob to disappear, and require 400 ms delays before both headers and body to complete within the request budget in exactly one request.

`cargo test --locked -p asagiri-image` passed all 79 tests locally with Rust 1.99.1 on macOS. The Windows Platform job now runs this crate as well as the Windows app and build-helper tests, so it exercises the actual Windows transport selection. Native Windows results must be checked on the published candidate; the macOS result alone does not establish Winsock behavior.
