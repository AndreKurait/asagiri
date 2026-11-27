# Move a checkpoint group between hosts

This experimental workflow moves a group of containers with non-TTY streams
and private engine-managed bridge networks. Install matching builds on both
hosts. The destination needs compatible guest CPU features, kernel, CRIU and
image layers. `checkpoint preflight` reports the source mounts and other
dependencies. Arrange any host bind mounts separately. The checkpoint can
carry the container writable layer and data in engine-managed volumes.

Checkpoint archives contain process memory, files and potentially credentials.
Create them with restrictive permissions and transfer them over an authenticated
encrypted channel. Keep the archives until you finish verifying the cutover.

## Capture and stage the destination

Run this on the destination and copy `destination.json` to the source.

```sh
asagiri checkpoint platform > destination.json
```

On the source, set `source_a` and `source_b` to the two container names or IDs.
The capture command checks the destination before pausing either source. It
leaves both sources paused and prevents Docker from restarting or unpausing them.

```sh
umask 077
asagiri checkpoint capture-group --destination destination.json --external-network handoff "$source_a" "$source_b"
asagiri checkpoint export "$source_a" handoff a.tar
asagiri checkpoint export "$source_b" handoff b.tar
shasum -a 256 a.tar b.tar > SHA256SUMS
```

Transfer the two archives and `SHA256SUMS` to the destination. Prepare
`a-create.json` and `b-create.json` as Docker container-create payloads with
the original image and required runtime settings. Use fresh destination volume
names in `HostConfig.Binds` or `HostConfig.Mounts`. The checkpoint supplies
the network definitions, endpoint settings and published port bindings.

The following example maps one volume per container. Set `source_volume_a`
and `source_volume_b` to the original volume names and match `received-a`
and `received-b` in the create payloads. Repeat `restore-volume` for each
additional volume.

```sh
shasum -a 256 -c SHA256SUMS
asagiri checkpoint prepare-networks a.tar b.tar
docker volume create received-a
docker volume create received-b
receiver_a=$(asagiri checkpoint create-receiver --wait-for-restore a.tar a-create.json)
receiver_b=$(asagiri checkpoint create-receiver --wait-for-restore b.tar b-create.json)
asagiri checkpoint import "$receiver_a" handoff a.tar
asagiri checkpoint import "$receiver_b" handoff b.tar
asagiri checkpoint restore-volume "$receiver_a" handoff "$source_volume_a" received-a
asagiri checkpoint restore-volume "$receiver_b" handoff "$source_volume_b" received-b

for receiver in "$receiver_a" "$receiver_b"; do
    asagiri checkpoint restore-rootfs "$receiver" handoff
    asagiri checkpoint restore-network-files "$receiver" handoff
    docker start "$receiver"
    asagiri checkpoint fence-receiver "$receiver"
    asagiri checkpoint verify-receiver-mounts "$receiver" handoff
    # Run restore-stdin when the checkpoint has an open input pipe.
    if [ "$(docker inspect -f '{{.Config.OpenStdin}}' "$receiver")" = true ]; then
        asagiri checkpoint restore-stdin "$receiver" handoff
    fi
    asagiri checkpoint install-process "$receiver" handoff
done
```

## Cut over or roll back

Before retirement, you can discard the destination receivers and resume the
original processes. Remove the destination receivers first. Then run these
commands on the source.

```sh
asagiri checkpoint release handoff "$source_a" "$source_b"
docker unpause "$source_a" "$source_b"
```

To cut over, retire both sources before activating either receiver. On the
source, wait for this command to succeed. It terminates the paused processes,
retains their checkpoints and prevents subsequent release or restart.

```sh
asagiri checkpoint retire handoff "$source_a" "$source_b"
```

On the destination, activate the complete group. If activation rejects an
incomplete group, check which receiver still needs installation before retrying.

```sh
asagiri checkpoint activate-group "$receiver_a" "$receiver_b"
```

Reconnect external clients to the destination host. The preserved port
numbers do not move the source host address or the connections through its
port forwards. Internal TCP continuity was observed in both directions
between Debian ARM64 and an Apple Virtualization guest on one physical Mac.
Separate-host and x86_64 handoffs still need qualification.

Measure downtime from the start of the source pause to the first successful
request through the destination's published port. Record archive size,
transfer time and the separate results for an internal open stream and an
external client reconnect. The transfer and verification steps in this
procedure can dominate downtime.
