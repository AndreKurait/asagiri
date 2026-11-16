# OCI hard-link extraction

Rooted hard-link targets resolve within the layer. GNU long names and PAX
path overrides keep their resolved names, and existing file destinations are
replaced without following their final symlink. The existing tar-rs checks
still reject source or destination paths outside the layer.

Docker applies the link header's ownership and mode to the shared inode.
The Linux extractor now does that for both relative and rooted links, along
with timestamps and extended attributes. It pins the inode with `O_PATH`:
changing a FIFO or device's metadata does not read it, and a hard link to a
symlink does not change the symlink target's attributes.

The first Docker reference run exposed an incorrect test expectation:
the source header had UID1001/GID1002/mode0640, while its hard-link header
had UID0/GID0/mode07777. Docker returned the latter for both paths.
Installed Asagiri0.1.5 was then checked with that corrected expectation
and reproduced the former values. Its uniquely owned fixture was removed.
The current test keeps these deliberately different headers and requires
Docker's result; it does not accept both behaviors.

Validation includes seven real Mac filesystem tests for the portable link
constructor, Linux layer tests for shared ownership/mode, PAX timestamps and
xattrs, replacement, and symlink-inode containment, and eight Docker API cases
in `tests/layer-hardlinks.py`. The API fixture runs against Docker and then
Asagiri in Linux CI, checking both paths' bytes and attributes and retaining
the engine version and observed metadata. The latest Linux execution must
pass before the branch is ready to merge.

Related public upstream: Kiri issue302 and PR322. This is an independent
implementation; the initial rooted-link fix was prepared before reviewing
PR322. The reference attribute behavior was also checked in Moby's
`pkg/archive/archive.go` at v28.0.4.
