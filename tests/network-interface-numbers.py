#!/usr/bin/env python3
"""Check that a container's networks keep working as other networks it joined leave and new ones join."""
import http.client
import io
import json
import socket
import sys
import tarfile
import threading
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
    tag = "kif-" + uuid.uuid4().hex[:8] + "-"

    def request(method, target, body=None, statuses=(200,), raw=False):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            # Track a create even if the assertion below rejects its status.
            if response.status == 201 and target.startswith("/containers/create"):
                containers.append(json.loads(data)["Id"])
            if response.status == 201 and target == "/networks/create":
                networks.append(json.loads(data)["Id"])
            assert response.status in statuses, (method, target, response.status, data)
            if raw:
                return data
            return json.loads(data) if data else None
        finally:
            connection.close()

    def network(name):
        request("POST", "/networks/create", {"Name": tag + name}, (201,))
        return tag + name

    def run(net):
        cid = request("POST", "/containers/create", {"Image": image, "Cmd": ["sleep", "120"],
                                                     "HostConfig": {"NetworkMode": net}}, (201,))["Id"]
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        return cid

    def restart(cid):
        request("POST", "/containers/" + cid + "/stop?t=0", statuses=(204,))
        # The exit may be recorded a moment after the stop returns.
        request("POST", "/containers/" + cid + "/wait?condition=not-running")
        request("POST", "/containers/" + cid + "/start", statuses=(204,))

    # Docker answers 200 to both; Kiri answers 204 today.
    def join(net, cid):
        request("POST", "/networks/" + net + "/connect", {"Container": cid}, (200, 204))

    def leave(net, cid):
        request("POST", "/networks/" + net + "/disconnect", {"Container": cid}, (200, 204))

    def output(cid, command):
        target = "/tmp/links-" + uuid.uuid4().hex
        eid = request("POST", "/containers/" + cid + "/exec",
                      {"Cmd": ["sh", "-c", "(" + command + ") > " + target + " 2>&1"]}, (201,))["Id"]
        # Docker answers 200; Kiri answers 204 today.
        request("POST", "/exec/" + eid + "/start", {"Detach": True}, (200, 204))
        deadline = time.monotonic() + 20
        while True:
            state = request("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "probe exec did not exit"
            time.sleep(.01)
        data = request("GET", "/containers/" + cid + "/archive?path=" + target, raw=True)
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            text = archive.extractfile(archive.getmembers()[0]).read().decode()
        assert state["ExitCode"] == 0, (command, state["ExitCode"], text)
        return text

    # The networks inspect shows for the container, with its address there.
    def endpoints(cid):
        found = request("GET", "/containers/" + cid + "/json")["NetworkSettings"]["Networks"]
        return {name: endpoint["IPAddress"] for name, endpoint in found.items()}

    # The container's IPv4 addresses, loopback aside, as its interfaces
    # have them. Interface names are the engine's own business.
    def addresses(cid):
        lines = output(cid, "ip -4 -o addr show").splitlines()
        found = {line.split()[3].split("/")[0] for line in lines if " inet " in line}
        return found - {"127.0.0.1"}

    # Inspect and the container's interfaces agree on exactly these networks.
    def on(cid, *nets):
        shown = endpoints(cid)
        assert set(shown) == set(nets), (sorted(shown), sorted(nets))
        assert all(shown.values()), shown
        assert addresses(cid) == set(shown.values()), (addresses(cid), shown)

    # Sends the requests at the same moment and waits for all of them.
    def at_once(*calls):
        failures = []

        def guarded(call):
            try:
                call()
            except BaseException as error:
                failures.append(error)

        threads = [threading.Thread(target=guarded, args=(call,)) for call in calls]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        if failures:
            raise failures[0]

    def reaches(cid, address):
        return output(cid, "ping -c 1 -W 2 " + address + " >/dev/null 2>&1 && echo up || echo down").strip() == "up"

    passed = 0
    try:
        base = network("base")
        one = network("one")
        two = network("two")
        three = network("three")

        # A neighbour on the second network to talk to.
        peer = run(two)
        peer_address = endpoints(peer)[two]

        # A network that joined after another one stays as it is when the
        # earlier one leaves, and can leave too.
        cid = run(base)
        join(one, cid)
        join(two, cid)
        on(cid, base, one, two)
        assert reaches(cid, peer_address)
        leave(one, cid)
        on(cid, base, two)
        assert reaches(cid, peer_address)
        passed += 1
        print("PASS a later network stays when an earlier one leaves", flush=True)

        leave(two, cid)
        on(cid, base)
        assert not reaches(cid, peer_address)
        passed += 1
        print("PASS the later network can leave after the earlier one", flush=True)

        # A network that joins once an earlier one has left takes nothing
        # from the ones that stayed.
        join(one, cid)
        join(two, cid)
        leave(one, cid)
        join(three, cid)
        on(cid, base, two, three)
        assert reaches(cid, peer_address)
        passed += 1
        print("PASS a new network leaves the ones that stayed alone", flush=True)

        join(one, cid)
        on(cid, base, one, two, three)
        assert reaches(cid, peer_address)
        passed += 1
        print("PASS the network that left can join again", flush=True)

        # In any order, each leaves with its own interface.
        leave(two, cid)
        on(cid, base, one, three)
        assert not reaches(cid, peer_address)
        leave(one, cid)
        on(cid, base, three)
        leave(three, cid)
        on(cid, base)
        passed += 1
        print("PASS each network leaves with its own interface", flush=True)

        # After a restart the same holds.
        join(one, cid)
        join(two, cid)
        leave(one, cid)
        restart(cid)
        on(cid, base, two)
        join(three, cid)
        on(cid, base, two, three)
        assert reaches(cid, peer_address)
        leave(two, cid)
        on(cid, base, three)
        leave(three, cid)
        on(cid, base)
        passed += 1
        print("PASS after a restart", flush=True)

        # Networks that join or leave while another change to the same
        # container is under way: each still has an interface of its own.
        for _ in range(3):
            at_once(lambda: join(one, cid), lambda: join(two, cid), lambda: join(three, cid))
            on(cid, base, one, two, three)
            assert reaches(cid, peer_address)
            at_once(lambda: leave(one, cid), lambda: leave(three, cid))
            on(cid, base, two)
            assert reaches(cid, peer_address)
            at_once(lambda: leave(two, cid), lambda: join(one, cid), lambda: join(three, cid))
            on(cid, base, one, three)
            assert not reaches(cid, peer_address)
            at_once(lambda: leave(one, cid), lambda: leave(three, cid))
            on(cid, base)
        passed += 1
        print("PASS networks that join and leave at once", flush=True)
        print(passed, "network interface scenarios passed")
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for cid in reversed(containers):
            try:
                request("DELETE", "/containers/" + cid + "?force=true", statuses=(204, 404))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        for net in reversed(networks):
            try:
                request("DELETE", "/networks/" + net, statuses=(204, 404))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not primary_failure:
            raise RuntimeError("Could not remove everything the network interface test made") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
