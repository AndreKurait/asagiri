# BuildKit

Asagiri serves Docker's BuildKit API on its existing Docker socket. A client
using Buildx's `docker` driver can build and load images directly, pass build
contexts and secrets, reuse cache, export files, and cancel a running build.
`--add-host name:host-gateway` works with this driver.

After setup, `asagiri build -t example .` uses the engine's Docker driver.
An explicit `BUILDX_BUILDER` or `--builder` still selects the requested
builder. `docker --context asagiri buildx build --builder asagiri -t example .`
also uses it unless a user-defined Buildx instance shadows that context.

The existing `asagiri-build` docker-container builder remains available and
shares the same managed BuildKit and cache. Settings → Builds can still send
every build to this explicit instance across Docker contexts. Buildx supports
that global selection for an instance, but not for a context's implicit
Docker driver. Setup preserves that choice and its original restore snapshot.
It removes an old context shadow only when it exactly matches Asagiri's own
copy; a user-modified instance stays.

BuildKit starts when first used and stops after the configured idle period.
Its managed container stays out of ordinary container lists, events and
container pruning. Its named cache and socket volumes remain visible.
Existing explicit builder references can still inspect and start it.
The Builds view reads the managed backend's history once, alongside existing
user builders. Opening an empty Builds view does not create or pull BuildKit.

The daemon accepts `/grpc` and `/session` HTTP upgrades through the same local
transport and permissions as the Docker API. Stock BuildKit runs inside the
engine, behind a fixed Unix socket relayed by the agent. There is no BuildKit
TCP control listener. Context, registry authentication, secret and SSH session
RPCs are forwarded; the Moby image exporter is adapted to a streamed import
into Asagiri's image store. Explicit client file outputs keep their destination.

The maintained `h2` library handles HTTP/2 framing and flow control. The bridge
limits connections, active RPCs, image imports, protobuf message sizes and
relay buffers. EOF and cancellation release registrations and abort dependent
streams. A truncated archive stream cannot report a successful import.
The bridge preserves worker details but removes the internal snapshotter
label that Buildx interprets as a Docker containerd image store. Imported
images use their config digest as the local image ID; the bridge does not
claim multi-platform image loading.

## Platforms and image loading

Build and load one platform at a time, for example
`docker --context asagiri buildx build --platform linux/arm64 -t example .`.
The built-in Docker driver does not advertise multi-platform or OCI archive
export support.

For multi-platform file output, select a docker-container builder such as
the existing `asagiri-build` instance:

```sh
docker --context asagiri buildx build --builder asagiri-build \
  --platform linux/amd64,linux/arm64 \
  --output type=oci,dest=image.tar .
```

This writes an OCI archive. Asagiri's `docker load` currently requires a
Docker image archive containing `manifest.json`; an OCI-only `index.json`
archive is not accepted. A Docker exporter or `--load` cannot load a
multi-platform manifest list into Asagiri.

The local mirror serves locally created names. For a tag pulled from a
registry, BuildKit resolves that registry's current platform information;
Asagiri's cached single-platform image is not substituted for the registry's
complete manifest. Digest requests match the manifest bytes the mirror
actually serves, which can differ from the original registry manifest.
Keep this distinction in mind when diagnosing an offline `FROM` failure.

## Verification

`tests/buildkit-driver.py` runs against a private Linux engine in the platform
and release workflows. It exercises an 8 MiB context, secret delivery, two
image tags, image IDs, implicit loading, cached and ordinary builds, explicit
tar output, concurrent Bake targets, host-gateway, a real running-build
cancellation, and builder migration with exact preference restoration.
Protocol tests cover unknown protobuf fields, legacy and current exporters,
session ownership, bounded frames, socket pinning and incomplete streams.
Native Mac VM and Windows WSL execution remain separate verification gaps.

`tests/buildx-bootstrap.py` checks a locally tagged base image and a remote
base exported for the other architecture, including the resulting ELF
architecture. It does not execute that other architecture or establish a
multi-platform application build. `tests/compose-health.py` adds a pinned
Compose 5.7.1 build/start/wait scenario with health and completed-job
dependencies, a failed-job control, and a local bind volume. That new
scenario is awaiting its first combined hosted qualification; native
health-probe results are recorded separately in
[`native-healthchecks-2026-11-16.json`](verification/native-healthchecks-2026-11-16.json).

Protocol references are the upstream
[BuildKit control API](https://github.com/moby/buildkit/blob/f55949dea4f03bb9d85b73e9afc9a609291289db/api/services/control/control.proto),
[session file services](https://github.com/moby/buildkit/blob/f55949dea4f03bb9d85b73e9afc9a609291289db/session/filesync/filesync.proto)
and [Buildx Docker driver](https://github.com/docker/buildx/tree/360040a78d6113dc84e5d4dcdf0f438e1be7a41f/driver/docker).
