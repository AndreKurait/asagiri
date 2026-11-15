# Linux service recovery and kind

Stopped-container uploads and metadata queries now wait until their temporary
mount namespace has been released. This keeps an immediate container start from
competing with the copy worker for the same overlay directories. Archive downloads
hold their connection open through that cleanup, and workers terminate when their
owning agent dies.

Linux startup also reclaims a container's old rootfs mounts when a service manager
has already removed its cgroup. Stacked mounts from earlier recoveries are detached
without deleting the writable layer.

The regression checks exercise the actual engine in disposable privileged Linux
hosts, with private sockets and no published outer ports:

- `tests/archive-completion.py` delays a copy worker's exit by five seconds.
  Source `6f7ac86` acknowledges the upload while its private mount remains alive.
  Source `2fa4f56` waits for release, then immediately starts the same container
  with its uploaded file intact.
- `tests/linux-service-recovery.py` restarts the daemon twice within one persistent
  host mount namespace, then kills it, removes its empty owned cgroups, and leaves
  four orphaned mount layers. Source `6f7ac86` retains the old mounts; `2fa4f56`
  reclaims them. Manual and restart-always containers preserve their writable data.
- The same probe also restarts a daemon while its engine is explicitly stopped,
  verifies that it stays stopped, then checks held `always`/`unless-stopped`
  policies, a canceled restart and a rapid Start/Stop pair. Requested stops are
  not logged as crashes. These checks pass on runtime source `44fab19`.
- `tests/engine-power.py` verifies parallel Stop, Sleep/Wake and repeated
  Start/Stop races. Nine containers with two-second stop timeouts take 18.115
  seconds on the prior build and 2.095 seconds on `44fab19`. Both measurements
  use the same probe in a private Linux host; the CI regression allows 12 seconds.
- `tests/exec-errors.py` checks 12 actual Docker CLI run/exec cases: denied and
  missing executables, directories, invalid scripts and `--init` shell fallback.
  All cases pass on `44fab19`; the prior build fails this regression.
- The stopped volume/bind archive, ownership and `/proc` symlink checks pass,
  along with the real BuildKit Docker driver, concurrent builds and cancellation.

## Overlay copy-up and commit contents

Overlay mounts explicitly use `metacopy=off,redirect_dir=off`. A chmod or
directory rename must leave complete file contents in the writable layer that
`docker commit` exports, even when the host enables these OverlayFS features
by default. Older upper entries with metadata-only copy-up or redirect
attributes return an explicit diff/commit error before a partial image is
published.

The index feature's protection against concurrent mounts of one writable
layer remains enabled when the host enables it. Copy completion and daemon
recovery release old mounts instead of allowing overlapping mounts to proceed.
The upstream systemd cleanup workaround is therefore handled by the engine's
existing startup recovery.

`tests/overlay-copyup.py` uses a private mount namespace to reproduce incomplete
upper contents with metadata-only copy-up and redirects enabled, then checks
complete data with both disabled. Both controls reject an overlapping mount.
The actual engine test commits and runs an image after chmod and directory
rename, checks the file hashes, and refuses injected legacy incomplete entries.
These checks, real BuildKit builds, the five-second archive completion barrier
and the separate daemon-restart regression pass on runtime source `749c40c`.
The tests do not change global kernel parameters. The original Arch/CachyOS
installation remains unverified.

## Automatic networks and Kubernetes DNS

`tests/kind-network.py` covers plain automatic IPv4 allocation, kind's dual-stack
network options, and local A/AAAA and external DNS through the bridge gateway.
The private Linux test allocates distinct free subnets and resolves through both
the embedded resolver and the gateway.

`tests/kind-cluster.py` pins kind `v0.27.0` and its official Kubernetes `v1.32.2`
node image, matching upstream reports #228 and #229. A real cluster becomes ready;
the node's containerd pulls a new image; node and pod queries resolve the public
registry; and the node resolves a peer container's bare network alias. A pod also
resolves that alias as an absolute name through CoreDNS, avoiding Kubernetes'
service search suffixes.

These probes pass on Linux source `2fa4f5690edd77e926ba6fc8a32655ecfcaeea9b`.
The original automatic-allocation and gateway-DNS failures were not reproduced on
this host. The reporter's CachyOS host with kernel 7.3.4 and systemd 262, native Mac
VMs and Windows WSL remain separate validation targets. The Linux platform workflow runs
the recovery and kind probes on subsequent commits.
