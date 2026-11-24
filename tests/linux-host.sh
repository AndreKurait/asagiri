#!/bin/bash
# Asagiri on a Linux host: runs the Linux build (cargo xtask linux) as the host
# engine inside a privileged container on the current Docker engine (Asagiri
# works), then drives it with the docker CLI from inside. No VM is involved:
# containers run on that Linux kernel directly.
set -u
D=${D:-docker}
BIN=${BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/linux/release}
# The engine's container (and its volumes): another name runs a second copy beside this one.
N=${N:-asagiri-linux-host}
[ -x "$BIN/asagiri" ] || { echo "build first: cargo xtask linux"; exit 1; }
$D rm -f "$N" >/dev/null 2>&1
$D run -d --name "$N" --privileged --tmpfs /run -v "$N":/var/lib/asagiri -v "$N"-store:/root/.local/share/asagiri \
  -v "$BIN:/opt/asagiri:ro" alpine sh -c 'apk add -q docker-cli curl >/dev/null 2>&1; exec /opt/asagiri/asagiri daemon' >/dev/null
up() { for _ in $(seq 60); do $D exec "$N" sh -c 'DOCKER_HOST=unix:///run/asagiri/docker.sock docker version' >/dev/null 2>&1 && break; sleep 1; done; }
up
PASS=0; FAIL=0
t() {
  local name=$1 want=$2; shift 2
  local got; got=$($D exec "$N" sh -c "export DOCKER_HOST=unix:///run/asagiri/docker.sock; $*" 2>&1 | tr -d '\r')
  if [[ "$got" == *"$want"* ]]; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); printf '  FAIL %s\n    want: %s\n    got:  %s\n' "$name" "$want" "$(echo "$got" | head -3)"; fi
}
# On a Linux host, published ports on a --network host container are discarded
# at start (HostConfig.PortBindings kept, NetworkSettings.Ports empty, no host
# forwarder bound) so the container's own listener binds the port without
# EADDRINUSE (#764).
t hostnet-ports-discarded 'running host {"18089/tcp":[{"HostIp":"","HostPort":"18089"}]} {} hostnet-764-ok' "docker rm -f lhhp764 >/dev/null 2>&1; docker run -d --name lhhp764 --network host -p 18089:18089 alpine sh -c 'while true; do printf \"HTTP/1.1 200 OK\\r\\nContent-Length: 14\\r\\n\\r\\nhostnet-764-ok\" | nc -l -p 18089 >/dev/null 2>&1 || exit 1; done' >/dev/null; ans=; for i in \$(seq 30); do ans=\$(curl -s -m 2 http://127.0.0.1:18089/) && [ \"\$ans\" = hostnet-764-ok ] && break; sleep 0.1; done; ins=\$(docker inspect -f '{{.State.Status}} {{.HostConfig.NetworkMode}} {{json .HostConfig.PortBindings}} {{json .NetworkSettings.Ports}}' lhhp764); docker rm -f lhhp764 >/dev/null 2>&1; echo \"\$ins \$ans\""
t server-os "linux" "docker version --format '{{.Server.Os}}'"
t run "3." "docker run --rm alpine cat /etc/alpine-release"
t home "/root" "docker run --rm alpine sh -c 'echo \$HOME'"
t internet "Example Domain" "docker run --rm alpine wget -qO- -T 10 http://example.com"
t port "Welcome to nginx" "docker run -d --name web -p 18081:80 nginx:alpine >/dev/null; for i in \$(seq 30); do curl -s localhost:18081 && break; sleep 0.3; done"
t exec "nginx version" "docker exec web nginx -v"
# What the registry serves now, for update checkers (#257).
t distribution '"mediaType":"application/vnd.oci.image.index.v1+json"' "curl -s --unix-socket /root/.asagiri/run/docker.sock http://d/v1.47/distribution/library/alpine:3.21/json"
t distribution-missing "404" "curl -s -o /dev/null -w '%{http_code}' --unix-socket /root/.asagiri/run/docker.sock http://d/v1.47/distribution/library/alpine:asagiri-missing-manifest-test/json"
# A container and an exec each lead their own session, so killpg from inside
# never reaches the daemon (#191).
t session-own "SID=1" "docker run --rm alpine sh -c 'echo SID=\$(cut -d\" \" -f6 /proc/1/stat)'"
t exec-session-own "own" "docker exec web sh -c 's=\$(cut -d\" \" -f6 /proc/\$\$/stat); [ \"\$s\" = \"\$\$\" ] && echo own || echo \"session \$s\"'"
t cp "copied" "echo copied > /tmp/x && docker cp /tmp/x web:/tmp/x && docker exec web cat /tmp/x"
t stats "web" "docker stats --no-stream --format '{{.Name}}'"
t network-dns "PONG" "docker network create lhn >/dev/null 2>&1; docker run -d --name db --network lhn redis:7-alpine >/dev/null; sleep 1; docker run --rm --network lhn redis:7-alpine redis-cli -h db ping"
t bind-host-path "3." "docker run --rm -v /etc:/h:ro alpine cat /h/alpine-release"
t volume "kept" "docker run --rm -v lhv:/d alpine sh -c 'echo kept > /d/f' && docker run --rm -v lhv:/d alpine cat /d/f"
t memory-limit "67108864" "docker run --rm --memory 64m alpine cat /sys/fs/cgroup/memory.max"
t seccomp "Seccomp:	2" "docker run --rm alpine grep Seccomp: /proc/self/status"
t udp "10.9.8.7" "docker run -d --name dns -p 15354:53/udp alpine sh -c 'apk add -q dnsmasq >/dev/null 2>&1; dnsmasq -k --address=/asagiri.test/10.9.8.7' >/dev/null; for i in \$(seq 40); do out=\$(nslookup -port=15354 asagiri.test 127.0.0.1 2>/dev/null) && break; sleep 0.5; done; echo \"\$out\" | grep -o 10.9.8.7"
t ipv6-network "true" "docker network create --ipv6 --subnet fd00:4c:1::/64 lh6 >/dev/null && docker network inspect lh6 -f '{{.EnableIPv6}}'"
t ipv6-ping "1 packets received" "docker run -d --name v6a --network lh6 nginx:alpine >/dev/null; docker run --rm --network lh6 alpine ping -6 -c 1 -W 2 v6a"
t ipv6-wget "Welcome to nginx" "docker run --rm --network lh6 alpine wget -qO- -T 5 'http://[fd00:4c:1::2]'"
t ipv6-host-nat "masquerade" "apk add -q nftables >/dev/null 2>&1; nft list table ip6 asagiri"
t ipv6-port "Welcome to nginx" "docker run -d --name web6 -p 18082:80 nginx:alpine >/dev/null; for i in \$(seq 30); do curl -sg 'http://[::1]:18082' && break; sleep 0.3; done"
t ipv6-cleanup "gone" "docker rm -f v6a web6 >/dev/null; docker network rm lh6 >/dev/null; for i in \$(seq 20); do nft list table ip6 asagiri >/dev/null 2>&1 || { echo gone; break; }; sleep 0.1; done"
t ipv6-forwarding-restored "0" "cat /proc/sys/net/ipv6/conf/all/forwarding"
# A build's --add-host host-gateway is the bridge gateway, as for docker run (not the Mac VM's 198.19.248.254).
t build-host-gateway "GW-OK" "gw=\$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}'); mkdir -p /bhg && printf 'FROM alpine:3.21\\nRUN grep -w hgw /etc/hosts\\n' > /bhg/Dockerfile && DOCKER_BUILDKIT=0 docker build --no-cache --add-host hgw:host-gateway /bhg 2>&1 | grep -q \"\$gw\" && echo GW-OK"
# A privileged container must not touch the host's /dev. The kernel has one
# devtmpfs, so mount it, put ptmx back as the device it should be, run a
# privileged container and look again: once, ptmx became a link into the
# host's /dev/pts and no user on the machine could open a terminal.
t privileged-host-ptmx "character special file" "mkdir -p /hostdev && mount -t devtmpfs none /hostdev && { [ -c /hostdev/ptmx ] || { rm -f /hostdev/ptmx && mknod -m 666 /hostdev/ptmx c 5 2 && chgrp 5 /hostdev/ptmx; }; } && docker run --rm --privileged alpine true && stat -c %F /hostdev/ptmx; umount /hostdev"
t privileged-devices "DEVS-OK" "docker run --rm --privileged alpine sh -c 'test -c /dev/kmsg && test -c /dev/null && test -L /dev/ptmx && test -c /dev/pts/ptmx && echo DEVS-OK'"
t privileged-tty "/dev/pts/0" "docker run --rm -t --privileged alpine tty"
t cleanup "web" "docker rm -f web db dns"
# Asagiri's Stop is the engine's stop, not the user's, as stopping Docker's
# daemon is: Start, also after the engine restarted, brings back what
# `always` and `unless-stopped` kept running, but not one the user stopped
# (docker stop) while Asagiri was stopped, which answers 204 (#181, #216).
POST="curl -s -o /dev/null -w %{http_code} --unix-socket /run/asagiri/docker.sock -X POST"
RUNNING="docker inspect -f '{{.Name}}={{.State.Running}}' ss-us ss-al ss-user ss-no | tr -d / | tr '\\n' ' '"
BACK="for i in \$(seq 100); do [ \"\$(docker inspect -f '{{.State.Running}}' ss-us ss-al | tr '\\n' ' ')\" = 'true true ' ] && break; sleep 0.2; done; $RUNNING"
ALL='ss-us ss-al ss-user ss-no ss-al1 ss-al2 ss-al3 ss-al4 ss-al5'
t ss-setup "ok" "docker run -d --name ss-us --restart unless-stopped alpine sleep 1d && docker run -d --name ss-al --restart always alpine sleep 1d && docker run -d --name ss-user --restart unless-stopped alpine sleep 1d && docker run -d --name ss-no alpine sleep 1d && for i in 1 2 3 4 5; do docker run -d --name ss-al\$i --restart always alpine sleep 1d || exit; done >/dev/null && echo ok"
# All at once, as a daemon shutdown stops them: nine `sleep 1d`s (PID 1
# ignores SIGTERM) take one 10 s timeout, not nine.
t ss-stop "204 in one timeout" "s=\$(date +%s); c=\$($POST http://d/asagiri/stop); e=\$(( \$(date +%s) - s )); [ \$e -lt 25 ] && echo \"\$c in one timeout\" || echo \"\$c in \${e}s\""
t ss-stop-all-down "ss-us=false ss-al=false ss-user=false ss-no=false" "$RUNNING"
t ss-docker-stop-while-stopped "204" "$POST http://d/containers/ss-user/stop"
t ss-start "204" "$POST http://d/asagiri/wake"
t ss-start-brings-back "ss-us=true ss-al=true ss-user=false ss-no=false" "$BACK"
# Start, then Stop at once, after the engine restarted: the restarts under
# way stop too, and nothing runs while Asagiri is stopped (#181).
t ss-stop-again "204" "$POST http://d/asagiri/stop"
$D restart -t 20 "$N" >/dev/null; up
t ss-start-then-stop "204 204" "echo \$($POST http://d/asagiri/wake) \$($POST http://d/asagiri/stop)"
t ss-start-then-stop-none-run "running: 0" "sleep 8; echo running: \$(docker ps -q | wc -l)"
t ss-start-then-stop-refused "Asagiri is stopped" "docker run --rm alpine true"
t ss-restarted-start "204" "$POST http://d/asagiri/wake"
t ss-restarted-brings-back "ss-us=true ss-al=true ss-user=false ss-no=false" "$BACK"
t ss-restarted-all-always "running: 7" "for i in \$(seq 100); do [ \$(docker ps -q | wc -l) -ge 7 ] && break; sleep 0.2; done; echo running: \$(docker ps -q | wc -l)"
t ss-cleanup "ss-us" "docker rm -f $ALL"
# A command that may not be run is 126, a missing one 127, as with Docker (#183).
t exit-126-not-executable "126" "docker run --rm alpine /etc/passwd; echo \$?"
t not-executable-msg 'exec: "/etc/passwd": permission denied' "docker run --rm alpine /etc/passwd"
t exit-127-missing "127" "docker run --rm alpine /no/such/file; echo \$?"
# Linux refuses a directory with EACCES; runc names it.
t dir-msg 'exec: "/etc": is a directory: permission denied' "docker run --rm alpine /etc"
t exec-dir-msg 'exec: "/etc": is a directory: permission denied' "docker run -d --name xd alpine sleep 60 >/dev/null; docker exec xd /etc; docker rm -f xd >/dev/null"
# runc's exec fails after the start: 255, in its own words on stderr. tini
# (--init) hands a script without a shebang to /bin/sh, as execvp does.
XS="mkdir -p /xs && printf 'echo hi\\n' > /xs/nosb && printf '#!/no/such/sh\\necho hi\\n' > /xs/badsb && chmod +x /xs/*"
t no-shebang "exec /xs/nosb: exec format error 255" "$XS; o=\$(docker run --rm -v /xs:/xs alpine /xs/nosb 2>&1); echo \"\$o \$?\""
t missing-interpreter "exec /xs/badsb: no such file or directory 255" "$XS; o=\$(docker run --rm -v /xs:/xs alpine /xs/badsb 2>&1); echo \"\$o \$?\""
t exec-no-shebang "exec /xs/nosb: exec format error 255" "$XS; docker run -d --name xsb -v /xs:/xs alpine sleep 60 >/dev/null; o=\$(docker exec xsb /xs/nosb 2>&1); echo \"\$o \$?\"; docker rm -f xsb >/dev/null"
t init-no-shebang "hi 0" "$XS; o=\$(docker run --rm --init -v /xs:/xs alpine /xs/nosb 2>&1); echo \"\$o \$?\""
t init-missing-interpreter "127" "$XS; docker run --rm --init -v /xs:/xs alpine /xs/badsb >/dev/null 2>&1; echo \$?"
t exit-126-exec "126" "docker run -d --name x126 alpine sleep 60 >/dev/null; docker exec x126 /etc/passwd; echo \$?; docker rm -f x126 >/dev/null"
# The engine stopped with containers running (systemctl stop asagiri): a stop
# that was asked for, "the VM stopped" at info, not an ERROR.
t clean-stop-setup "ok" "docker run -d --name cs alpine sleep 1d >/dev/null && echo ok"
$D restart -t 20 "$N" >/dev/null; up
if $D logs "$N" 2>&1 | grep -q 'stopped unexpectedly'; then FAIL=$((FAIL + 1)); echo "  FAIL clean-stop-not-error: $($D logs "$N" 2>&1 | grep 'stopped unexpectedly' | head -1)"; else PASS=$((PASS + 1)); fi
t clean-stop-cleanup "cs" "docker rm -f cs"
$D rm -f "$N" >/dev/null
echo "$PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
