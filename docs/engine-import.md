# Import from another engine

On macOS and Windows, `asagiri import` copies images, named volumes and
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
`asagiri image import`; the bare `asagiri import` migration command is unavailable.

## Keep volume writers stopped

Stop an application's source containers cleanly before importing its volume.
For a database, let its shutdown finish writing the database files. Keep all
writers stopped until the import finishes.

The importer first verifies that the named source volume exists and checks
for running, paused or restarting containers using it. An active user or an
unreadable source inventory stops the operation before any copy helper or
destination volume is created. The error identifies the active containers.

The importer checks the source again after copying. These checks observe two
points in time; they cannot guarantee consistency if a writer starts and
stops between them. They do not lock the source engine or create an atomic
filesystem snapshot.

If copying fails or the final source check fails, inspect the destination
before retrying. It may contain files written by that attempt. An explicit
volume import copies into the destination volume and does not remove extra
files already there, so retrying against existing contents requires care.
The source volume remains available for recovery.
