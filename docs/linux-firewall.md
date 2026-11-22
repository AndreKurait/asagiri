# Linux bridge networking and host firewalls

Asagiri installs IPv4 NAT and container isolation in its own `ip asagiri`
nftables table. A host firewall also evaluates forwarded packets. If that
firewall drops bridge traffic, an accept in Asagiri's table does not override
the drop. A container can then reach its gateway while outbound TCP or DNS
times out; the same request with `--network host` may work.

Configure the firewall that owns the drop policy to allow the intended
Asagiri bridge traffic. Keep unrelated forwarding and unsolicited inbound
traffic restricted. Asagiri does not change another firewall's policy.

## Fedora firewalld and bridge DNS

On Fedora 44 with the `FedoraWorkstation` default zone, a container on the
host network could resolve names while a bridge container reported `Host is
unreachable` for its gateway resolver. Check the bridge's zone with
`sudo firewall-cmd --get-zone-of-interface=asagiri0`. An unassigned bridge
uses the default zone. DNS to the bridge gateway traverses the host input
chain, so inspect that path as well as forwarding.

For the default bridge, the following temporary rule allows DNS from its IPv4
subnet to its gateway. Run it with the Docker CLI connected to Asagiri. Use
the zone that applies to the bridge and confirm the subnet and gateway before
applying the rule.

```sh
subnet=$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Subnet}}')
gateway=$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}')
rule="rule family=\"ipv4\" source address=\"$subnet\" destination address=\"$gateway/32\" service name=\"dns\" accept"
sudo firewall-cmd --zone=FedoraWorkstation --add-rich-rule="$rule" --timeout=90
docker run --rm alpine:3.21 nslookup -type=A example.com
```

The rule expires after 90 seconds. A successful lookup verifies gateway DNS.
Test the intended VPN destination separately to check forwarding and NAT.

## Forward to one Tailscale service with firewalld

On Fedora 44 kernel 7.2.15, a container reached a Tailscale peer through a
firewalld policy with a dedicated bridge ingress zone and VPN egress zone.
The policy accepted IPv4 TCP traffic to `100.64.0.2/32` on port `9010`. An
HTTP listener on port `9011` remained reachable from the host and was blocked
from the bridge container.

For an existing deployment, inspect `firewall-cmd --get-active-zones` and
use the zones assigned to the intended interfaces. A zone shared with other
interfaces expands the policy's scope. The following example assumes the
administrator has dedicated zones named `asagiri-bridge` for `asagiri0` and
`tailnet` for `tailscale0`. Configure the peer address and port for the service
you intend to expose to containers.

```sh
sudo firewall-cmd --permanent --new-policy=asagiri-to-peer
sudo firewall-cmd --permanent --policy=asagiri-to-peer --add-ingress-zone=asagiri-bridge
sudo firewall-cmd --permanent --policy=asagiri-to-peer --add-egress-zone=tailnet
sudo firewall-cmd --permanent --policy=asagiri-to-peer --add-rich-rule='rule family="ipv4" destination address="100.64.0.2/32" port port="9010" protocol="tcp" accept'
sudo firewall-cmd --check-config
sudo firewall-cmd --reload
```

To remove this policy, run `sudo firewall-cmd --permanent --delete-policy=asagiri-to-peer`
and reload firewalld. A reload also discards runtime-only changes, including
interface assignments made only for a temporary test.

## Find the failing boundary

With a bridged container running, inspect the engine's NAT table and the
host's forwarding policy:

```sh
sudo asagiri diag firewall
```

This read-only command reports active Asagiri-named bridges with their IPv4
subnets and gateways, the main routing table's default uplink, Asagiri's
NAT table, nftables forward chains with a drop policy, and the iptables
frontend and forwarding policy. For an iptables `FORWARD DROP` policy and
one default uplink, it prints interface-scoped examples for each observed
bridge, using UFW or `DOCKER-USER` when their unconditional forwarding jump
is present. Review existing rules and their order before applying an example.
An earlier accept may already allow the traffic; a drop policy alone does
not prove that the packet is blocked.

The command does not start an engine, change rules, send packets or contact
an external service. It uses the installed `ip`, `nft` and `iptables` tools,
limits each command to two seconds and each output stream to 1 MiB, and
reports unavailable, denied or malformed checks as unknown with a nonzero
exit status. It does not evaluate IPv6, policy routing, VPNs, explicit
drop/reject rule traversal, or a separate iptables-legacy ruleset when the
selected frontend is nft. It runs only when requested, adding no work to
container startup. Reinspect after recreating a network or reloading the
host firewall.

For the full rules and routing details:

```sh
sudo nft list table ip asagiri
sudo iptables -S FORWARD
sudo nft list ruleset
ip -4 route show default
docker --context asagiri network inspect bridge
```

The engine's NAT rule uses the uplink interface. It is visible in
`nft list table ip asagiri`; `iptables -t nat -S` does not list rules in this
separate nftables table. If the table is absent, inspect the daemon's startup
log and `journalctl -u asagiri`. Adding a duplicate masquerade rule does not
repair a rejected or missing engine table.

The examples below use the default bridge `asagiri0` and uplink `eth0`.
Use the actual interfaces on your machine. Custom bridge networks need their
own explicit allowance. VPNs, multiple uplinks, IPv6 and native nftables
firewalls need rules appropriate to their routing and policy.

Compose normally creates a project network with its own bridge and subnet.
Allowing only `asagiri0` therefore does not enable egress from that project.
Find its gateway and the interface holding that address:

```sh
docker --context asagiri network inspect myproject_default \
  --format '{{range .IPAM.Config}}{{println .Subnet .Gateway}}{{end}}'
ip -4 -o address show
```

Use that bridge, for example `asagiri2`, in the rules below. Repeat for each
network whose egress you intend to allow. Inspect again after recreating
networks because their subnets and bridge names can change. Asagiri's own
NAT table covers the bridges it creates; the host firewall still needs the
corresponding forwarding allowances.

## Docker with its iptables backend

Docker can leave the host's `FORWARD` policy at `DROP`. Its `DOCKER-USER`
chain is the place for operator rules processed before Docker's bridge
rules. This also applies when `iptables --version` reports `nf_tables`;
that is the iptables compatibility frontend, rather than Docker's separate
native nftables backend.

After reviewing the intended egress, allow traffic from this bridge to this
uplink and established replies in the other direction:

```sh
sudo iptables -I DOCKER-USER 1 -i asagiri0 -o eth0 -j ACCEPT
sudo iptables -I DOCKER-USER 1 -i eth0 -o asagiri0 \
  -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
```

These rules do not allow a new inbound connection to a container. Published
ports may require a separate allowance in your host policy. Keep the global
`FORWARD` policy and Docker's own rules in place.

For a host whose forwarding policy is managed directly with iptables and
has no `DOCKER-USER` chain, the same two rules belong in `FORWARD`.
For UFW, express the egress allowance through UFW, for example:

```sh
sudo ufw route allow in on asagiri0 out on eth0
```

UFW normally handles established replies in its existing before-forward
rules. Review customized rules and any other forward-hook chains as well.
With native nftables, put the corresponding allowances in the firewall
that drops the packets; adding an accepting table beside it is insufficient.
The firewall manager should persist the chosen rules and restore them in
the right order after startup or reload.

To remove the two temporary Docker iptables allowances:

```sh
sudo iptables -D DOCKER-USER -i asagiri0 -o eth0 -j ACCEPT
sudo iptables -D DOCKER-USER -i eth0 -o asagiri0 \
  -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
```

## Reproducible verification

`tests/linux-firewall.py` owns two disposable Linux hosts and a private Docker
network. It checks real TCP, UDP and DNS traffic, observes the masqueraded source,
reproduces blocking under `FORWARD DROP`, and verifies the scoped allowances.
It also starts a pinned Docker daemon beside Asagiri and exercises the real
`DOCKER-USER` chain. The checks retain the drop policy, require an unpublished
listener to remain unreachable from outside, and remove the allowances to
confirm that blocking returns. The diagnostic is checked against the actual
bridge/subnet, `FORWARD DROP`, and Docker's chain, with nftables and iptables
snapshots required to remain unchanged after inspection.
The `--user-network` variant reserves another
network first, then repeats all checks on a custom bridge. It derives the
interface from that network's actual gateway instead of assuming `asagiri0`
or a fixed subnet.

The script preserves the source and binary identities, rules, logs and
cleanup results. Platform CI and both Linux release architectures run the
default and custom-network variants.
It changes only the disposable hosts' firewall rules, publishes no outer
ports and mounts no user Docker socket. This tests the IPv4 iptables path;
it does not certify every UFW/firewalld policy, VPN, IPv6 route or WSL host.

See Docker's guidance on
[packet filtering](https://docs.docker.com/engine/network/packet-filtering-firewalls/),
[operator iptables rules](https://docs.docker.com/engine/network/firewall-iptables/),
and [nftables forwarding policies](https://docs.docker.com/engine/network/firewall-nftables/#forward-policy-in-iptables).
