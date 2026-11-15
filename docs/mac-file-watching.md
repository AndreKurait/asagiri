# File watching in Mac bind mounts

Asagiri relays changes made on the Mac to file watchers inside running
containers. This covers ordinary writes, editor saves that replace a file
by renaming it, metadata changes, and deletion. Both bind-mounted
directories and individual files are watched. Install the matching app
and bundled guest kernel, then restart the engine after updating.

Bind sources are refreshed once per second, so a new bind can take up to
one second to begin reporting host edits. Event delivery is best effort:
FSEvents can coalesce changes, and a container's own write can produce a
duplicate notification after reaching the Mac. File watchers should read
the current file contents when notified.

The host callback and guest queues have fixed limits. The callback never
waits for the guest; overflowing host events produce a diagnostic warning.
The guest processes a limited batch between other agent messages. Paths
remain within the Mac share, and the guest kernel interface accepts writes
only from its initial mount namespace. No file content is changed to
generate a notification.

This preserves the authorship of
[Kiri PR #304](https://github.com/libracoder/kiri/pull/304), addressing
[issue #48](https://github.com/libracoder/kiri/issues/48) and
[issue #310](https://github.com/libracoder/kiri/issues/310). Asagiri uses
protocol message 80 because 78 and 79 already carry cleanup results.
Native Mac tests exercise FSEvents and individual-file filtering; protocol
tests cover message encoding, and guest queue tests cover scope and bounds.
Those tests are separate from verification inside the booted guest.
