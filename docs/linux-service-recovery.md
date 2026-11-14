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
  Source `ce0eb4f` waits for release, then immediately starts the same container
  with its uploaded file intact.
- `tests/linux-service-recovery.py` restarts the daemon twice within one persistent
  host mount namespace, then kills it, removes its empty owned cgroups, and leaves
  four orphaned mount layers. Source `6f7ac86` retains the old mounts; `ce0eb4f`
  reclaims them. Manual and restart-always containers preserve their writable data.
- The stopped volume/bind archive, ownership and `/proc` symlink checks pass,
  along with the real BuildKit Docker driver, concurrent builds and cancellation.

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

These probes pass on Linux source `ce0eb4f2451246b22700c60161d5d18a0228ce3e`.
The original automatic-allocation and gateway-DNS failures were not reproduced on
this host. The reporter's CachyOS host with kernel 7.3.4 and systemd 262, native Mac
VMs and Windows WSL remain separate validation targets. The Linux platform workflow runs
the recovery and kind probes on subsequent commits.
