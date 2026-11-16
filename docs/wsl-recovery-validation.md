# WSL daemon and bridge recovery

This adapts [public Kiri #318](https://github.com/libracoder/kiri/pull/318) at `c82077684b5a82553b71201b03c520fa92ea58db`, preserving Precious Okwu's authorship. The Linux bridge shares one startup attempt across pending connections, backs off unsuccessful starts, and carries a failure reason to the Windows pipe proxy. Connecting to a socket alone does not reset the backoff; the daemon must answer.

The startup script serializes concurrent starts, waits for a stopping daemon, and distinguishes an exited daemon from one that never creates its socket. Windows refresh requests use short total deadlines, Start's health probe uses a total deadline, and stale startup files are synchronized once after an engine version is known. Existing startup serialization, Stop intent, whole-operation version checks and anchored process matching are retained.

Validation completed locally with Rust 1.99.1:

- 24 portable Windows recovery/synchronization/refusal tests and four actual activation-shell tests passed. Windows GNU all-target compilation passed. Strict Clippy reports the same existing diagnostics as the unchanged base, with no new diagnostics in the comparison.
- 14 production bridge/time tests passed in Linux ARM64 inside the installed Asagiri VM, including bounded flow control for a slow reader and a large stdin stream.
- The actual startup script passed four simultaneous starts, lock-descriptor closure, restart during shutdown, a 20-second shutdown, missing-socket failure and immediate daemon exit. It used a compiled stand-in in a uniquely owned offline Alpine container, subsequently verified absent.
- The production Linux bridge component passed all four CLI protocol scenarios: three connections and one start, no start for a running daemon, one failed start with two useful failure replies, and backoff after a daemon binds then dies without answering. This native component check used the verified dev.98 Linux daemon; it did not substitute for a complete rebuilt CLI or native Windows WSL test.

The Linux Platform job builds the standalone startup fixture with the same pinned Rust image as the engine. Fixture hashes and source identity remain in a separate directory; the engine's two-binary checksum contract is preserved. The job requires both the startup fixture and `tests/bridge-start.py` against the complete rebuilt CLI. The controller runs Python on the host; test containers use the cached digest-pinned Alpine image, networking disabled and no package installation. It retains source/binary hashes, daemon logs and verified cleanup. Failed Linux runs also retain their built engine artifact for diagnosis.

The native Windows job exercises the Windows unit and pipe tests. A real Windows/WSL suspend/resume and setup/update session remains a separate platform validation requirement. The process-exit timeout helper is not a proof of a strict bound on draining every descendant-held output pipe.
