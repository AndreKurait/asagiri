#!/usr/bin/env python3
"""Check that a container started on the default bridge resolves the names of a network it joins later."""
import http.client
import io
import json
import socket
import sys
import tarfile
import time
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image):
    containers = []
    networks = []
    tag = "kcd-" + uuid.uuid4().hex[:8]

    def request(method, target, body=None, statuses=(200,)):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            if response.status == 201 and target.startswith("/containers/create"):
                containers.append(json.loads(data)["Id"])
            if response.status == 201 and target == "/networks/create":
                networks.append(json.loads(data)["Id"])
            assert response.status in statuses, (method, target, response.status, data)
            if "/archive?" in target:
                return data
            return json.loads(data) if data else None
        finally:
            connection.close()

    def run(name, host=None):
        cid = request("POST", "/containers/create?name=" + name,
                      {"Image": image, "Cmd": ["sleep", "120"], "HostConfig": host or {}}, (201,))["Id"]
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        return cid

    # A command's exit status and output. The output goes to a file in the
    # container and is read through the archive API: that asks nothing of
    # how an engine attaches to an exec.
    def sh(cid, command):
        target = "/tmp/out-" + uuid.uuid4().hex
        eid = request("POST", "/containers/" + cid + "/exec",
                      {"Cmd": ["sh", "-c", "(" + command + ") > " + target + " 2>&1"]}, (201,))["Id"]
        request("POST", "/exec/" + eid + "/start", {"Detach": True}, (200, 204))
        deadline = time.monotonic() + 30
        while True:
            state = request("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "the exec did not exit"
            time.sleep(.01)
        data = request("GET", "/containers/" + cid + "/archive?path=" + target)
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            return state["ExitCode"], archive.extractfile(archive.getmembers()[0]).read().decode()

    def resolv(cid):
        code, text = sh(cid, "cat /etc/resolv.conf")
        assert code == 0, text
        return [line for line in text.splitlines() if line and not line.startswith("#")]

    def nameservers(cid):
        return [line.split()[1] for line in resolv(cid) if line.startswith("nameserver")]

    # Only the A record is asked for: a name the engine knows has no AAAA
    # record, and that question would go on to servers that may not answer.
    def resolves(cid, name, address):
        code, text = sh(cid, "nslookup -type=a " + name)
        return code == 0 and ("Address: " + address + "\n") in text + "\n"

    def join(network, cid, statuses=(200, 204)):
        request("POST", "/networks/" + network + "/connect", {"Container": cid}, statuses)

    def address(cid, network):
        return request("GET", "/containers/" + cid + "/json")["NetworkSettings"]["Networks"][network]["IPAddress"]

    cases = 0
    try:
        name = tag + "-net"
        network = request("POST", "/networks/create", {"Name": name}, (201,))["Id"]
        peer = run(tag + "-peer", {"NetworkMode": name})
        peer_ip = address(peer, name)

        # The default bridge has no names, so a container there asks other
        # resolvers. With its first user network it asks 127.0.0.11, which
        # knows that network's names.
        late = run(tag + "-late")
        assert not resolves(late, tag + "-peer", peer_ip), "the default bridge resolved a name of another network"
        join(network, late)
        assert nameservers(late) == ["127.0.0.11"], resolv(late)
        assert resolves(late, tag + "-peer", peer_ip), sh(late, "nslookup -type=a " + tag + "-peer")
        assert resolves(peer, tag + "-late", address(late, name)), sh(peer, "nslookup -type=a " + tag + "-late")
        print("PASS a container on the default bridge resolves the network it joins", flush=True)
        cases += 1

        # Leaving the network takes its names away again; the resolver stays.
        request("POST", "/networks/" + network + "/disconnect", {"Container": late}, (200, 204))
        assert nameservers(late) == ["127.0.0.11"], resolv(late)
        assert not resolves(late, tag + "-peer", peer_ip), "a name of a network the container left still resolves"
        print("PASS the names go with the network", flush=True)
        cases += 1

        # A container that is connected when it starts resolves them too.
        join(network, late)
        request("POST", "/containers/" + late + "/stop?t=0", statuses=(204,))
        request("POST", "/containers/" + late + "/wait?condition=not-running")
        request("POST", "/containers/" + late + "/start", statuses=(204,))
        assert nameservers(late) == ["127.0.0.11"], resolv(late)
        assert resolves(late, tag + "-peer", peer_ip), sh(late, "nslookup -type=a " + tag + "-peer")
        print("PASS after a restart with both networks", flush=True)
        cases += 1

        # `--dns`, `--dns-search` and `--dns-option`: the servers leave the
        # file (127.0.0.11 forwards to them), the rest of it stays.
        own = run(tag + "-own", {"Dns": ["192.0.2.53"], "DnsSearch": ["corp.example"], "DnsOptions": ["ndots:2"]})
        assert nameservers(own) == ["192.0.2.53"], resolv(own)
        join(network, own)
        lines = resolv(own)
        assert nameservers(own) == ["127.0.0.11"], lines
        assert "search corp.example" in lines, lines
        assert any(line.startswith("options") and "ndots:2" in line.split() for line in lines), lines
        # The name is asked for as it is: with the search domain it would
        # first go to the servers given, and nothing answers there.
        assert resolves(own, tag + "-peer.", peer_ip), sh(own, "nslookup -type=a " + tag + "-peer.")
        print("PASS a container with its own DNS servers", flush=True)
        cases += 1

        # A file the container has changed is the container's.
        edited = run(tag + "-edited")
        code, text = sh(edited, "echo 'nameserver 192.0.2.54' > /etc/resolv.conf")
        assert code == 0, text
        join(network, edited)
        assert resolv(edited) == ["nameserver 192.0.2.54"], resolv(edited)
        print("PASS a changed resolv.conf is left alone", flush=True)
        cases += 1

        # A container that starts on a user network already asks 127.0.0.11,
        # and resolves a second network's names once it joins.
        other = tag + "-other"
        second = request("POST", "/networks/create", {"Name": other}, (201,))["Id"]
        far = run(tag + "-far", {"NetworkMode": other})
        assert not resolves(peer, tag + "-far", address(far, other))
        join(second, peer)
        assert nameservers(peer) == ["127.0.0.11"], resolv(peer)
        assert resolves(peer, tag + "-far", address(far, other)), sh(peer, "nslookup -type=a " + tag + "-far")
        print("PASS a container on a user network joins a second one", flush=True)
        cases += 1
        print(cases, "network connect DNS scenarios passed")
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for cid in reversed(containers):
            try:
                request("DELETE", "/containers/" + cid + "?force=true", statuses=(204,))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        for network in reversed(networks):
            try:
                request("DELETE", "/networks/" + network, statuses=(204,))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not primary_failure:
            raise RuntimeError("Could not remove everything the network connect DNS test made") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
