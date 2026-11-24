# Asagiri #89 guest reply verification

2026-11-24 on arm64 macOS 26.7.3 (25G342). Baseline and local `origin/main` were both `ee0ffa16937638167e9f127a45e86bceb2d6381b`. The candidate is the working-tree patch on that revision. The issue remains open for live VM verification.

The adjacent `upstream.json` records [Kiri PR #766](https://github.com/libracoder/kiri/pull/766) as merged at `76a61830530865a33636cf7f151417d9fd3db0ee`, closing Kiri #463. [Kiri PR #464](https://github.com/libracoder/kiri/pull/464) was closed without a merge. I checked both supplied diffs (heads `63309bf9d82a1b3e7d848f84241a6b9edfeb47f5` and `69ef97568017c657305b817f486820d73bdc0072`). The fork's audit already called for an explicit failure or pagination, and the checkout still wrote complete lists without a size check. The merged upstream patch budgets list bytes, drops trailing records, and shortens an oversized string. Its platform test expects a successful truncated top reply. Asagiri uses the frame budget and boundary tests with explicit whole-reply rejection. FS events in this checkout travel from host to guest, where the host writer already rejects oversized frames.

The baseline reproduces the earlier issue comment at the current revision using the real encoder and `frame_len`. The scratch harness `/tmp/asagiri89-baseline.rs` calls each encoder with 3,700 distinct 255-byte UTF-8 file names, one top row one byte over budget, and 9,000 usage rows of 120 bytes each. I ran the same commands on the baseline and working tree using Homebrew Rust 1.98.0.

```sh
rustc --edition=2024 --crate-type=rlib --crate-name asagiri_proto crates/asagiri-proto/src/lib.rs -o /tmp/libasagiri_proto.rlib
rustc --edition=2024 /tmp/asagiri89-baseline.rs --extern asagiri_proto=/tmp/libasagiri_proto.rlib -o /tmp/asagiri89
/tmp/asagiri89
```

| Reply | Baseline frame | Baseline receiver | Candidate receiver |
| --- | ---: | --- | --- |
| Directory | 1,076,710 bytes | `TooLarge(1076706)` | 47-byte `Error` |
| Top | 1,048,577 bytes | `TooLarge(1048573)` | 47-byte `Error` |
| Usage | 1,116,010 bytes | `TooLarge(1116006)` | 6-byte `UsageTooLarge` |

The encoder reserves the six-byte header and four-byte count. Before each append, checked subtraction accounts for the fixed record fields and the entire UTF-8 string. Every record requires a four-byte length prefix at minimum, which bounds the count before its `u32` conversion. A rejected frame rolls back to its original offset before writing the bounded error. Directory enumeration also reports an error above its existing 5,000-record limit. Usage rejection takes the asynchronous path and reaches `/system/df` as an error. The new message advances the protocol to 16, so host and guest must be upgraded together.

```sh
rustc --edition=2024 --test crates/asagiri-proto/src/lib.rs -o /tmp/asagiri89-proto-tests
/tmp/asagiri89-proto-tests guest_list_replies
RUSTC="$(rustup which --toolchain 1.99.1 rustc)" rustup run 1.99.1 cargo check --offline -p asagiri-init --target aarch64-unknown-linux-musl --target-dir /tmp/asagiri89-linux
```

The protocol test passed. It decoded the three error frames followed by complete ordinary replies, including an exact 1 MiB frame containing a multibyte top row. The cross-check passed and compiled Linux guest code. The first cross-check attempt inherited the Homebrew compiler and failed with `E0463` for the Linux target. Selecting the rustup compiler explicitly resolved it.

The socket-pair test uses the production host reader and guest encoders with in-memory replies. A booted VM, real sibling containers, and native Linux and Windows runtime coverage remain unverified. The local checks are defensive verification with synthetic encoder inputs.

```sh
cargo test --offline -p asagiri -p asagiri-proto -p asagiri-init > /tmp/asagiri89-tests.log 2>&1
target/debug/deps/asagiri-39a0860d9c4a48e7 daemon::vmlink::tests
/tmp/asagiri89-proto-tests
target/debug/deps/asagiri_init-32c1454eef3ba653
git diff --check
```

The focused suites passed 10 VM-link tests, 6 protocol tests, and 8 macOS-compatible init tests. The production host reader delivered the top error to its FIFO waiter, recorded the usage error in its separate slot, and delivered the next pong with the link still connected. The broad run reported 32 failures before I stopped it (exit 130) with a bridge test still running after 60 seconds. The failures included tests requiring listening sockets and macOS services. See `/tmp/asagiri89-tests.log` for the partial run.
