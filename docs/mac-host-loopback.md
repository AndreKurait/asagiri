# Reach a development server on the Mac

From a container, use `host.docker.internal` and the port of the server on
your Mac. For example, a server listening on `localhost:5173` is available at
`http://host.docker.internal:5173` inside the container.

Asagiri first connects to `127.0.0.1`. If that address refuses the connection,
it tries `::1`. This supports servers that resolve `localhost` to IPv6, including
common Node development setups. If separate IPv4 and IPv6 servers listen on the
same port, the IPv4 server is used. Both connection attempts share a two-second
deadline; IPv4 timeouts and policy denials are returned without trying another address.

The connection uses the VM's socket transport. A host-network container also has
a route for the reserved relay address, so it can reach the Mac before DHCP
assigns the VM an address. External container networking still needs a working
uplink. The relay accepts only its existing loopback protocol address; containers
cannot use it to request an arbitrary LAN address.

`--add-host name:host-gateway` uses the same relay on macOS. On a native Linux
host, `host.docker.internal` continues to name the bridge gateway.

## Regression checks

The fix follows [Kiri issue #258](https://github.com/libracoder/kiri/issues/258).
The Mac regression runs requests through both bridge and host networks, using
`host.docker.internal` and a `host-gateway` alias. It checks IPv4-only and
IPv6-only servers, IPv4 preference when both listen, and refusal when both stop.
It creates its own loopback servers and containers and removes them afterward.

With Asagiri running, from the source checkout:

```sh
docker --context asagiri pull alpine:3.21@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab
python3 tests/host-loopback.py "$HOME/.asagiri/run/docker.sock"
```

Mac unit tests use real IPv4/IPv6 listeners and reject other relay addresses.
The Linux guest regression creates a separate network namespace with no DHCP
server or default route, initializes the production VM network path, and checks
data transfer and the original destination through its actual NAT rule.
