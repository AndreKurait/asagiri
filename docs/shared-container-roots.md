# Shared container roots

A copy into a stopped container can overlap `docker start`; a copy from a
running container can outlive its process and overlap a restart. Both operations
must use one overlay mount for that container's writable layer. Linux
[does not support sharing upper/work directories between overlay mounts](https://docs.kernel.org/filesystems/overlayfs.html#sharing-and-copying-layers);
an overlap can fail with `EBUSY`, or appear to work with undefined filesystem
behavior.

This adapts [Kiri PR #321](https://github.com/libracoder/kiri/pull/321), commit
[`91151ba308e53cecd137c70283ebafade11db11a`](https://github.com/libracoder/kiri/commit/91151ba308e53cecd137c70283ebafade11db11a),
by Precious Okwu, to Asagiri's existing asynchronous cleanup and per-container
lifecycle barriers.

The agent owns each overlay and counts its process and copy users. A copy worker
requests its view from the agent after reading the request, so it uses current
process state. A running view is pinned through an owned descriptor. A stopped
view uses the same overlay in a private namespace with the container's persistent
mounts; export continues to exclude those mounts.

The agent holds the copy connection and filesystem lease until its direct child
is reaped. Upload acknowledgments, stat replies, and download EOF cannot precede
that point. Large archives stream in the child, and the agent sends bounded
metadata without blocking. Root requests are capped at 64 KiB, with repeated
layers removed; final metadata is capped at 16 KiB. Oversized packets fail
explicitly.

The final user queues an unmount outside the control loop. Removal waits for all
users and uses the same retrying worker queue. The ID stays reserved through
worker reap; busy mounts are retried without lazy-detach reuse. A stopped-copy
worker also releases its inherited tree before streaming the archive. If another
root retires while that namespace is being prepared, its ID stays reserved until
those inherited mount references are released.

Process exits remain separate from filesystem cleanup. If a start races the
final copy's unmount, the agent immediately returns a specific transient error
after `CleanupPending`. The host waits for that ID and retries. Ordinary replies
remain in FIFO order, and other containers' requests continue.

## Validation

`shared-root-process` is a standalone Linux test without libtest threads. It
exercises the production modules with real overlays, private mount namespaces,
Unix sockets, descriptor passing, and `waitpid`. CI requires privileged execution
through `ASAGIRI_REQUIRE_SHARED_ROOT_TEST=1`. It checks:

- A stopped copy and a start share the same overlay device, while an unrelated
  root can retire and mount again during the copy.
- A reaped unmount stays reserved while a copied namespace retains its mount.
- A pinned running view survives process exit and shares its overlay with a
  restart.
- Stat replies and upload acknowledgments remain held until worker reap.
- Downloads stream their complete archive but hold EOF until worker reap.
- Removal waits for an active writer; disconnect and shutdown release leases
  and leave no unreaped children.

All seven scenarios passed on the installed Asagiri 0.1.5 ARM64 Linux guest using a
separately compiled test executable in an owned, network-isolated container.
Its writable fixtures used a private tmpfs. Six host link and cleanup tests also
passed. This validates the component under the guest kernel; the installed app
was not replaced by this test.

`tests/shared-root.py` compares stopped copy/start, running copy/stop/restart, and
two concurrent uploads/start against Docker before the private Asagiri CI build.
It allows Docker to serialize lifecycle operations behind the upload, checks
unrelated start/wait responses, verifies file bytes before and after restart, and
confirms deletion of every owned test container.

The installed 0.1.5 app passed the small stopped-copy/start API probe, including
an observed upper-layer write before start. That is a compatibility control,
not a reproduction of an overlay failure or proof of a single mount.
The full Docker comparison and rebuilt engine API results are pending hosted CI.
