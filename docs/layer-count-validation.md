# Images with many layers

Asagiri configures overlay layers individually when the kernel supports
`lowerdir+`, avoiding the single-page option string that prevented images with
about 60 layers from starting. This addresses
[Kiri issue #253](https://github.com/libracoder/kiri/issues/253).
The implementation follows the
[Linux overlay filesystem API](https://docs.kernel.org/filesystems/overlayfs.html#multiple-lower-layers),
available since Linux 6.8.

Older kernels retain the legacy mount path. An option list that cannot fit
returns an explicit size-limit error instead of passing a truncated path to
the kernel. Permission and missing-path failures are reported directly.
For data directories exceeding `fsconfig`'s string limit, kernels with
directory-FD support can configure the upper and work directories that way;
otherwise, a legacy mount is used if its options fit.

The kernel's overlay index policy is retained. Both paths disable metacopy
and directory redirects so the writable layer still contains the complete
file data used by diff and commit.

The privileged Linux unit tests use disposable mount namespaces. They mount
120 distinct lower layers through the production container function, verify
duplicate-layer handling and precedence, copy up file data, unmount and mount
again. They also execute a legacy mount, verify oversized options are rejected
before mounting, and exercise a data directory longer than 256 bytes. CI
requires the real mount checks; an unprivileged developer run cannot execute
them.

`tests/many-layers.py SOCKET BASE_IMAGE --record RESULT.json` derives a unique
120-layer image from an already local image containing `sh`, `cat`, `find` and
`wc`. It checks start, every added layer, overlay precedence, stopped archive
reads and restart, then deletes its own image and container. It uses no
network or host mounts. The platform workflow runs this same fixture against
Docker and Asagiri.

An installed Asagiri 0.1.5 negative control reproduced the reported
`mount …/rootfs: No such file or directory` failure on the 120-layer fixture.
Its container and image were removed. The new kernel and API positive checks
remain pending until the branch's Linux workflow completes.
