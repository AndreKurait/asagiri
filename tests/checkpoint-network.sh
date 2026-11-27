#!/bin/sh
# Run against an isolated checkpoint-capable engine with jq and Alpine 3.21.
set -eu
work=$(mktemp -d)
network=checkpoint-net-${work##*/}
secondary=$network-other
cid=''
cleanup() {
    if [ -n "$cid" ]; then docker rm -f "$cid" >/dev/null || true; fi
    docker network rm "$secondary" "$network" >/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT
asagiri=${ASAGIRI_BIN:-asagiri}
docker network create --subnet 10.218.47.0/24 --label purpose=checkpoint "$network" >/dev/null
docker network create --internal "$secondary" >/dev/null
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
    .version == 1 and .containerId == $c.Id and .containerName == ($c.Name | ltrimstr("/")) and
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
