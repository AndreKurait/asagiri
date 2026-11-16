# Tmpfs volume ownership

A local tmpfs volume keeps its requested UID, GID and mode when the image
does not contain its mount target. Previously, creating the target also made
it visible through the cloned image view. Copy-up then treated that generated
directory as image content and replaced the volume root's metadata with
`0:0 0755`, despite correct tmpfs mount options.

The engine marks a newly generated copy-up mountpoint only when its device
and inode match the directory in the image view. The marker survives container
restarts. Targets created under a host bind have a different identity and are
never marked. Looking for image contents no longer creates missing image
directories. Real image directories still populate empty volumes with their
contents and metadata; `volume-nocopy` still leaves them empty.

This independently implements the public report in
[Kiri issue #296](https://github.com/libracoder/kiri/issues/296), while preserving
copy-up beneath a parent bind mount as required by
[Kiri issue #30](https://github.com/libracoder/kiri/issues/30).
The marker identifies directories created by this implementation; it does not
retroactively classify an older container's unmarked directories or repair
volume metadata that was already overwritten.

Installed Asagiri 0.1.5 reproduced the reported failure on two starts of the
same container. The API record shows `uid=10001,gid=10050,mode=2750` in the
mount table while `stat` reports `0:0 0755`. The test used an already cached,
digest-pinned Alpine image, with networking disabled, and removed its own
container and volume.

`tests/volume-owner.py` checks two starts for a missing target, empty and
populated image directories, `volume-nocopy`, and two nested host-bind cases.
It verifies the new start's output, compares root metadata and contents, checks
that host files and metadata remain intact, and records cleanup. Linux CI
runs it against Docker before the private Asagiri engine.

The Linux unit fixture calls the production mount path using real overlays,
tmpfs volumes and bind mounts in a disposable child mount namespace. It
unmounts and remounts the overlay between rounds, checks all six cases, and
requires the real fixture to run in CI. A separate filesystem test verifies
that image lookup creates no missing parents. Linux ARM64 musl cross-checks
of all init targets, Python syntax and shell checks pass on the Mac. Actual
Linux execution and the full platform workflow remain required before merge.
