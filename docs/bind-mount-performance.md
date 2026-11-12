# Mac bind mounts

Mac workspaces under `/opt` and `/usr/local` are host paths, like `/Users`
and `/Volumes`. Prefixes match whole path components: `/optical` and
`/usr/locality` are still VM paths. Linux paths such as `/lib/modules`
remain available for kind and other nested runtimes.

Several mounts can be used together. Keep source in a bind mount and put
Linux dependencies and generated caches in named volumes:

```yaml
services:
  app:
    image: node:22-alpine
    working_dir: /app
    volumes:
      - ./src:/app
      - ./config:/config:ro
      - node_modules:/app/node_modules
      - build_cache:/app/.cache
volumes:
  node_modules:
  build_cache:
```

Named volumes avoid a host/guest filesystem request for each dependency
file. This changes where those files live: back up important volume data
and keep source files in the bind mount.

The guest also sets read-ahead to 1024 KiB for the Mac virtiofs share only.
This batches sequential reads without lengthening metadata cache timeouts
or disabling host/guest change visibility. The tuning is best effort: if
the kernel has no backing-device setting, the share keeps its default.
Rosetta, the data disk and Linux host filesystems are not changed.

Measure the workload before choosing a filesystem layout. A sequential
read benchmark does not predict dependency installation or small-file
metadata performance, and warm host caches are not cold disk results.

`bench/bind-read.c` is a small Linux reader for measuring the transfer
without timing the Docker CLI. It reads in 1 MiB blocks and evicts only its
input file from the guest page cache before timing. It does not purge the
Mac's caches. Compile it for the guest architecture, for example:

```sh
zig cc -target aarch64-linux-musl -O2 bench/bind-read.c -o /tmp/bind-read
dd if=/dev/zero of=/tmp/bind-fixture.bin bs=1048576 count=512
docker run --rm --network none \
  -v /tmp/bind-read:/reader:ro \
  -v /tmp/bind-fixture.bin:/fixture:ro \
  alpine:3.22 /reader /fixture
```

Use identical file sizes, images, CPU/memory limits and read block sizes.
Warm the host cache in both arms and alternate their order. Preserve every
sample and label the measured runtime, kernel and hardware. The reader's
JSON reports bytes, elapsed seconds and MiB/s; it makes no speed assertion.
