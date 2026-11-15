# Import a root filesystem

`docker import` creates an image from a root filesystem tar archive. Asagiri
accepts uncompressed tar and gzip, streamed from a file, standard input, or an
HTTP(S) URL. Large uploads and chunked HTTP bodies are streamed to the image
store.

Image layers retain FIFOs, character and block device nodes, and their hardlinks,
along with ownership, permission bits, modification times and device numbers.
Local PAX modification times retain nanosecond precision, including dates
before the epoch. Malformed PAX size, ownership or time records fail extraction
before a special node replaces an existing entry. Extraction creates these
nodes without opening them. A special node can replace a symlink
at its own path without changing the symlink's former target.

An ordinary container still needs its cgroup device-access filter. If Linux
refuses to load or attach that filter, startup fails before the application
runs; removing the ability to create new nodes would not protect existing
image nodes. The bundled guest kernels enable `CONFIG_BPF_SYSCALL` and
`CONFIG_CGROUP_BPF`. On a Linux host, these facilities and permission to attach
the filter are required. After correcting the host restriction, the same
container can be started again.

```sh
docker --context asagiri export my-container > rootfs.tar
docker --context asagiri import \
  --change 'CMD ["/bin/sh"]' \
  --message 'Local root filesystem snapshot' \
  rootfs.tar local/snapshot:latest
docker --context asagiri run --rm -it local/snapshot:latest
```

An export contains the root filesystem. Supply runtime settings such as `CMD`,
`ENTRYPOINT`, `ENV`, `WORKDIR`, and `USER` with `--change`. `LABEL`, `EXPOSE`,
`VOLUME`, and `ONBUILD` are also supported. `docker save` and `docker load`
preserve the image's existing configuration when moving a complete image.

```sh
gzip -c rootfs.tar | docker --context asagiri import \
  --change 'CMD ["/bin/sh"]' - local/snapshot:compressed

docker --context asagiri import \
  --change 'CMD ["/bin/sh"]' \
  https://example.com/rootfs.tar.gz local/snapshot:downloaded
```

Omitting the repository creates an untagged image and prints its image ID.
The default platform is the engine's Linux architecture; `--platform` records
another Linux platform without converting its binaries.

The Engine API endpoint is `POST /images/create?fromSrc=-&repo=name:tag`.
It also accepts a separate `tag` query parameter, repeated `changes` parameters,
`message`, and `platform`. For URL imports, put the URL in `fromSrc`.
Successful imports return the image ID in the streamed JSON response.

Unsupported compression, invalid instructions, damaged archives, and incomplete
uploads fail without publishing an image. The input archive is checked before
its image record is written.

`tests/image-import.py` exercises the Docker CLI and Engine API beside the
disposable Linux daemon used by CI. Its `--reference` mode records Docker's
behavior separately; Asagiri must also pass the stricter check that a rejected
import leaves no image record. The live suite checks special-node metadata and
links after file, gzip, chunked and URL imports, and after `docker save/load`.
