#!/bin/sh
# Run against an isolated checkpoint-capable engine with jq and Alpine 3.21.
set -eu
work=$(mktemp -d)
network=checkpoint-net-${work##*/}
secondary=$network-other
cid=''
busy=''
cleanup() {
    if [ -n "$cid" ]; then docker rm -f "$cid" >/dev/null || true; fi
    if [ -n "$busy" ]; then docker rm -f "$busy" >/dev/null || true; fi
    for n in "$secondary" "$network"; do docker network rm "$n" >/dev/null 2>&1 || true; done
    rm -rf "$work"
}
trap cleanup EXIT
asagiri=${ASAGIRI_BIN:-asagiri}
docker network create --subnet 10.218.47.0/24 --ip-range 10.218.47.0/26 --aux-address reserved=10.218.47.12 \
    --ipv6 --subnet fd83:218:47::/64 -o com.docker.network.driver.mtu=1450 --label purpose=checkpoint "$network" >/dev/null
docker network create --internal --attachable "$secondary" >/dev/null
cid=$(docker run -d --pull=never --network "$network" --ip 10.218.47.9 --network-alias before \
    --hostname checkpoint-host --dns 192.0.2.53 --dns-search example.test --dns-option ndots:2 \
    --add-host local.example:192.0.2.1 -p 127.0.0.1::8080/tcp alpine:3.21 sh -ec '
        while :; do
            printf "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\ncheckpoint-host" | nc -l -p 8080 >/dev/null
        done
    ')
docker network connect --alias second "$secondary" "$cid"
docker inspect "$cid" >"$work/container.json"
docker network inspect "$network" "$secondary" >"$work/networks.json"
host_port=$(jq -er '.[0].NetworkSettings.Ports["8080/tcp"][] | select(.HostIp == "127.0.0.1") | .HostPort' "$work/container.json")
check_service() {
    test "$(curl --noproxy '*' --fail --silent --show-error --retry 3 --retry-connrefused --max-time 5 "http://127.0.0.1:$host_port/")" = checkpoint-host
    test "$(docker run --rm --pull=never --network "$network" alpine:3.21 wget -q -T 5 -O - http://before:8080/)" = checkpoint-host
}
check_service
# The bind-mounted files are writable independently of the Docker network spec.
docker exec "$cid" sh -ec 'printf "192.0.2.42\tfrozen.example\n" >>/etc/hosts; printf "# before capture\n" >>/etc/resolv.conf; chmod 0640 /etc/resolv.conf; chown 12345:12345 /etc/resolv.conf'
# Give the captured hostname a hole. Its bind mount is independent of the UTS hostname.
docker exec "$cid" sh -ec 'printf x | dd of=/etc/hostname bs=1 seek=131072 conv=notrunc 2>/dev/null'
for file in hostname hosts resolv.conf; do docker exec "$cid" cat "/etc/$file" >"$work/$file"; done
docker pause "$cid" >/dev/null
docker checkpoint create --leave-running "$cid" before-change
docker unpause "$cid" >/dev/null
docker exec "$cid" sh -ec 'printf "192.0.2.43\tlater.example\n" >>/etc/hosts; printf "# after capture\n" >>/etc/resolv.conf'
check_service
docker network disconnect "$secondary" "$cid"
docker inspect "$cid" | jq -e --arg name "$secondary" '.[0].NetworkSettings.Networks | has($name) | not' >/dev/null
"$asagiri" checkpoint export "$cid" before-change "$work/checkpoint.tar"
tar -xOf "$work/checkpoint.tar" network-files.tar >"$work/network-files.tar"
test "$(tar -tf "$work/network-files.tar" | LC_ALL=C sort)" = "$(printf '%s\n' hostname hosts resolv.conf)"
for file in hostname hosts resolv.conf; do
    tar -xOf "$work/network-files.tar" "$file" >"$work/exported-$file"
    cmp "$work/$file" "$work/exported-$file"
done
tar -xOf "$work/checkpoint.tar" network.json | jq -e --slurpfile inspect "$work/container.json" --slurpfile nets "$work/networks.json" \
    --arg first "$network" --arg second "$secondary" '
    $inspect[0][0] as $c | $c.HostConfig as $h |
    (has("externalNetwork") | not) and .version == 1 and .containerId == $c.Id and .containerName == ($c.Name | ltrimstr("/")) and
    .mode == $h.NetworkMode and .hostname == $c.Config.Hostname and .domainname == $c.Config.Domainname and
    .dns == $h.Dns and .dnsSearch == $h.DnsSearch and .dnsOptions == $h.DnsOptions and .extraHosts == $h.ExtraHosts and
    .endpoints == $c.NetworkSettings.Networks and .endpoints[$first].IPAddress == "10.218.47.9" and
    (.endpoints[$second].Aliases | index("second") != null) and
    (.networks | sort_by(.Id)) == ($nets[0] | map(del(.Containers)) | sort_by(.Id)) and
    .ports == $c.NetworkSettings.Ports and .requestedPorts == $h.PortBindings and
    .exposedPorts == $c.Config.ExposedPorts and
    (.ports["8080/tcp"] | any(.HostIp == "127.0.0.1" and (.HostPort | tonumber) > 0)) and
    .requestedPorts["8080/tcp"][0].HostPort == "0"
' >/dev/null
echo 'PASS checkpoint retained the frozen network files, network definitions, endpoints, DNS and allocated ports after source changes'

# Restore the two bridge definitions into an engine that no longer has either.
docker rm -f "$cid" >/dev/null
cid=''
docker network rm "$secondary" "$network" >/dev/null
docker network create --label conflicting=true "$secondary" >/dev/null
if "$asagiri" checkpoint prepare-networks "$work/checkpoint.tar" >"$work/error" 2>&1; then exit 1; fi
grep -Fq "$secondary differs" "$work/error"
if docker network inspect "$network" >/dev/null 2>&1; then exit 1; fi
test "$(docker network inspect -f '{{index .Labels "conflicting"}}' "$secondary")" = true
docker network rm "$secondary" >/dev/null
if [ -n "${NETWORK_TEST_RECORDS:-}" ]; then
    mkdir "$NETWORK_TEST_RECORDS.tmp"
    if "$asagiri" checkpoint prepare-networks "$work/checkpoint.tar" >"$work/error" 2>&1; then exit 1; fi
    rmdir "$NETWORK_TEST_RECORDS.tmp"
    for net in "$network" "$secondary"; do if docker network inspect "$net" >/dev/null 2>&1; then exit 1; fi; done
fi
"$asagiri" checkpoint prepare-networks "$work/checkpoint.tar" "$work/checkpoint.tar" >"$work/prepared.json" & first=$!
"$asagiri" checkpoint prepare-networks "$work/checkpoint.tar" >"$work/concurrent.json" & second=$!
wait "$first"; wait "$second"
jq -es 'all(.[]; length == 2) and ((.[0] | map_values(.id)) == (.[1] | map_values(.id))) and
    any(.[]; all(.[]; .created == true)) and any(.[]; all(.[]; .created == false))' \
    "$work/prepared.json" "$work/concurrent.json" >/dev/null
docker network inspect "$network" "$secondary" | jq -e --slurpfile source "$work/networks.json" '
    def config: map(del(.Id, .Created, .Containers)) | sort_by(.Name);
    config == ($source[0] | config)
' >/dev/null
"$asagiri" checkpoint prepare-networks "$work/checkpoint.tar" | jq -e --slurpfile before "$work/prepared.json" '
    all(.[]; .created == false) and map_values(.id) == ($before[0] | map_values(.id))
' >/dev/null
echo 'PASS both checkpoint networks matched the source, reused destination IDs, and rejected a conflicting group'

# Let the destination operator supply the image and workload. All network fields come from the archive.
jq '.[0].Config | {Image, Entrypoint, Cmd}' "$work/container.json" >"$work/create.json"
jq '.HostConfig.NetworkMode="none"' "$work/create.json" >"$work/mismatch.json"
if "$asagiri" checkpoint create-receiver "$work/checkpoint.tar" "$work/mismatch.json" >"$work/error" 2>&1; then exit 1; fi
grep -Fq 'NetworkMode in the create request conflicts' "$work/error"
cid=$("$asagiri" checkpoint create-receiver "$work/checkpoint.tar" "$work/create.json")
test "$(docker inspect -f '{{.State.Status}}' "$cid")" = created
if "$asagiri" checkpoint create-receiver "$work/checkpoint.tar" "$work/create.json" >"$work/error" 2>&1; then exit 1; fi
test "$(docker inspect -f '{{.State.Status}}' "$cid")" = created
"$asagiri" checkpoint import "$cid" received "$work/checkpoint.tar"
if "$asagiri" checkpoint restore-network-files "$cid" missing >"$work/error" 2>&1; then exit 1; fi
grep -Fq 'has no network file archive' "$work/error"
# Import an older checkpoint without the optional archive, then a partial set.
mkdir "$work/partial"
tar -xf "$work/checkpoint.tar" -C "$work/partial" inventory.img pstree.img
COPYFILE_DISABLE=1 tar --format=ustar -cf "$work/legacy.tar" -C "$work/partial" inventory.img pstree.img
"$asagiri" checkpoint import "$cid" legacy "$work/legacy.tar"
if "$asagiri" checkpoint restore-network-files "$cid" legacy >"$work/error" 2>&1; then exit 1; fi
grep -Fq 'has no network file archive' "$work/error"
COPYFILE_DISABLE=1 tar --format=ustar -cf "$work/partial/network-files.tar" -C "$work" hosts resolv.conf
COPYFILE_DISABLE=1 tar --format=ustar -cf "$work/partial.tar" -C "$work/partial" inventory.img pstree.img network-files.tar
"$asagiri" checkpoint import "$cid" incomplete "$work/partial.tar"
if "$asagiri" checkpoint restore-network-files "$cid" incomplete >"$work/error" 2>&1; then exit 1; fi
grep -Fq 'missing network file' "$work/error"
"$asagiri" checkpoint restore-network-files "$cid" received & first=$!
"$asagiri" checkpoint restore-network-files "$cid" received & second=$!
wait "$first"; wait "$second"
"$asagiri" checkpoint import "$cid" another "$work/checkpoint.tar"
if "$asagiri" checkpoint restore-network-files "$cid" another >"$work/error" 2>&1; then exit 1; fi
grep -Fq 'different checkpoint' "$work/error"
busy=$(docker run --pull=never --network bridge -p "127.0.0.1:$host_port:7777" -d alpine:3.21 sleep 180)
if docker start "$cid" >"$work/error" 2>&1; then exit 1; fi
test "$(docker inspect -f '{{.State.Running}}' "$cid")" = false
test "$(docker port "$busy" 7777/tcp)" = "127.0.0.1:$host_port"
docker rm -f "$busy" >/dev/null
busy=''
docker start "$cid" >/dev/null
check_service
docker inspect "$cid" | jq -e --slurpfile source "$work/container.json" '
    def endpoint: {IPAddress,GlobalIPv6Address,MacAddress,GwPriority,Gateway,IPv6Gateway};
    def network: .NetworkSettings | {Ports, Networks:(.Networks | map_values(endpoint))};
    def host: .HostConfig | {NetworkMode,Dns,DnsSearch,DnsOptions,ExtraHosts};
    .[0] as $dest | $source[0][0] as $src |
    ($dest|network) == ($src|network) and ($dest|host) == ($src|host) and
    $dest.Name == $src.Name and $dest.Config.Hostname == $src.Config.Hostname and
    all($src.NetworkSettings.Networks | to_entries[]; . as $srcNet |
        ($srcNet.value.DNSNames - $dest.NetworkSettings.Networks[$srcNet.key].DNSNames) == [])
' >/dev/null
echo 'PASS stopped receiver retained both endpoint configurations, DNS names, and the allocated host port after start'
for file in hostname hosts resolv.conf; do
    docker exec "$cid" cat "/etc/$file" >"$work/restored-$file"
    cmp "$work/$file" "$work/restored-$file"
done
test "$(docker exec "$cid" stat -c '%a %u %g' /etc/resolv.conf)" = '640 12345 12345'
test "$(docker exec "$cid" stat -c '%b' /etc/hostname)" -lt 256
docker exec "$cid" awk '$5 == "/etc/hosts" && $6 ~ /nosuid/ && $6 ~ /nodev/ && $6 ~ /noexec/ {ok=1} END {exit !ok}' /proc/self/mountinfo
if "$asagiri" checkpoint restore-network-files "$cid" received >"$work/error" 2>&1; then exit 1; fi
grep -Fq 'freshly created receiver' "$work/error"
docker exec "$cid" sh -ec 'printf "192.0.2.44\treceiver.example\n" >>/etc/hosts'
docker run --rm --pull=never --network "container:$cid" alpine:3.21 cat /etc/hosts >"$work/shared-hosts"
docker exec "$cid" cat /etc/hosts >"$work/edited-hosts"
grep -Fq 'receiver.example' "$work/edited-hosts"
cmp "$work/edited-hosts" "$work/shared-hosts"
docker restart -t 0 "$cid" >/dev/null
docker exec "$cid" cat /etc/hosts >"$work/restarted-hosts"
cmp "$work/edited-hosts" "$work/restarted-hosts"
check_service
echo 'PASS restored all three frozen network files, sparse data and ownership; shared receiver edits survived restart'

# An existing built-in bridge and an existing automatically allocated bridge also need the old addresses.
docker rm -f "$cid" >/dev/null
cid=''
docker network rm "$secondary" "$network" >/dev/null
secondary=$network-auto
docker network create "$secondary" >/dev/null
cid=$(docker run -d --pull=never --network bridge alpine:3.21 sleep 180)
docker network connect --alias attached "$secondary" "$cid"
docker inspect "$cid" >"$work/auto.json"
docker pause "$cid" >/dev/null
"$asagiri" checkpoint create --external-network "$cid" auto
"$asagiri" checkpoint export "$cid" auto "$work/auto.tar"
docker rm -f "$cid" >/dev/null
cid=''
ip=$(jq -er '.[0].NetworkSettings.Networks.bridge.IPAddress' "$work/auto.json")
if docker create --pull=never --network bridge --ip "$ip" alpine:3.21 true >"$work/error" 2>&1; then exit 1; fi
grep -q 'user defined networks only' "$work/error"
printf '%s\n' '{"Image":"alpine:3.21","Cmd":["sleep","180"]}' >"$work/auto-request.json"
# Use the captured addresses, then exercise a destination collision and retry.
busy=$(docker run --pull=never --network bridge -d alpine:3.21 sleep 180)
test "$(docker inspect -f '{{.NetworkSettings.Networks.bridge.IPAddress}}' "$busy")" = "$ip"
cid=$("$asagiri" checkpoint create-receiver "$work/auto.tar" "$work/auto-request.json")
if docker start "$cid" >"$work/error" 2>&1; then exit 1; fi
grep -qi 'address already in use' "$work/error"
docker rm -f "$busy" >/dev/null
busy=''
docker start "$cid" >/dev/null
docker inspect "$cid" | jq -e --slurpfile source "$work/auto.json" '
    def endpoint: .NetworkSettings.Networks | map_values({IPAddress,GlobalIPv6Address,MacAddress});
    (.[0]|endpoint) == ($source[0][0]|endpoint)
' >/dev/null
echo 'PASS receiver reclaimed default and automatically assigned bridge addresses after a collision cleared'
