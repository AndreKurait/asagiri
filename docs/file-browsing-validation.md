# Mounted files in Files and Finder

The file browser reads a running container through its init process's mounted
root. Named volumes, bind mounts, and files created at runtime use the same
view. An exited container becomes unavailable while its cleanup finishes.
Requested paths still use the existing `fsview` in-root resolution.

This adapts the runtime fix from public Kiri PR334 (issue317). Asagiri retains
its ID-keyed process table and does not fall back to the overlay for a reaped
process.

The Linux production-agent test
`running_view_sees_private_mounts_and_rejects_reaped_entries` creates a child
with a private mount namespace and root. It checks listings, metadata, complete
and partial reads of volume, readonly bind, and runtime files; in-root symlinks;
outside-path refusal; and exited/exec entries. The original root is a negative
control: its mount targets remain empty. Linux CI runs this in its privileged
disposable compiler container.

For the installed Mac, after updating both the app and guest:

```sh
python3 tests/finder-mounted.py "$HOME/.asagiri/run/docker.sock" "$HOME/Asagiri" alpine:3.21
```

This requires the existing Finder mount and a locally available image with
`sh` and `sleep`. It creates uniquely named container and volume resources,
a temporary readonly host bind, and checks the real NFS client's listing,
contents, file sizes, fresh writes and stopped-container removal. It removes
only its own container, volume and temporary directory. Passing Linux tests
alone does not establish native Finder behavior.
