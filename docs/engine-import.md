# Import from another engine

On Linux, macOS and Windows, `asagiri import` copies images, named volumes and
container definitions from Docker Desktop. On macOS it also accepts the
`orbstack` source. Container definitions arrive stopped.

Inspect a source before choosing items to copy:

```sh
asagiri import desktop-linux --list
asagiri import desktop-linux image my-app:latest
asagiri import desktop-linux volume my-project-db
asagiri import desktop-linux container my-app
```

Without an item, `asagiri import desktop-linux` attempts the images, named
volumes and containers Asagiri does not already have. It reports failed items.
The [root filesystem import guide](docker-import.md) covers `docker import`
and `docker load`. On Linux, the corresponding Asagiri rootfs command is
`asagiri image import`. Use `asagiri import desktop-linux` for engine migration.

## Check data stored outside volumes

Keep the source container until you verify the files your application wrote in
its writable layer. Check any extended attributes the application uses, even
when the import reports success. In a Docker Engine migration, a file survived
but its binary `user.*` attribute disappeared during `docker commit` on the
source. `docker export` and `docker cp -a` also omitted that attribute.

For workloads that depend on this metadata, make an application-level backup
that preserves it and verify its value after restoring. Volume data is copied
separately.

## Keep volume writers stopped

Stop an application's source containers cleanly before importing its volume.
For a database, let its shutdown finish writing the database files. Keep all
writers stopped until the import finishes.

The importer first verifies that the named source volume exists and checks
for running, paused or restarting containers using it. An active user or an
unreadable source inventory stops the operation before any copy helper or
destination volume is created. The error identifies the active containers.

Both engines need the same digest-pinned Debian helper image. Each engine
pulls it from Docker Hub if its local cache lacks that digest. After resolving
the helper, the importer checks the source again before creating the destination
volume. It also checks for attached destination containers, including stopped
containers. Detach them before importing into an existing volume.

The helper mounts the source read-only and streams the files with GNU tar.
It preserves numeric ownership, user extended attributes such as Supabase
Storage metadata, and POSIX access and default ACLs. Both helpers run with
networking disabled during the copy.

The final check verifies that the source is still idle. A writer can start
and stop between checks, so keep writers stopped throughout the copy.

If copying or the final check fails, inspect the destination before retrying.
A partial volume can appear as already present in `--list`. An explicit
volume import overwrites files with matching paths and leaves other destination
files in place. The source volume remains available for recovery.
