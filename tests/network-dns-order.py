#!/usr/bin/env python3
"""Compare duplicate DNS aliases across joined networks and endpoint lifecycles."""
import argparse
import hashlib
import http.client
import ipaddress
import json
import os
from pathlib import Path
import re
import socket
import sys
import time
import urllib.parse
import uuid

LABEL = "dev.asagiri.dns-order-test"
LIMIT = 4 * 1024**2


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=15)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def exec_output(data):
    """Decode the complete non-TTY stdout/stderr stream without losing frames."""
    output = bytearray()
    while data:
        assert len(data) >= 8 and data[0] in (1, 2) and data[1:4] == bytes(3), "Invalid exec frame"
        size = int.from_bytes(data[4:8], "big")
        assert len(data) >= 8 + size, "Truncated exec frame"
        assert len(output) + size <= 65536, "DNS output exceeded the fixture limit"
        output.extend(data[8:8 + size])
        data = data[8 + size:]
    return output.decode()


def owned(kind, item, names, nonce, original, image):
    identity = item["Id"]
    assert re.fullmatch(r"[0-9a-f]{64}", identity) and identity not in original
    assert item["Name"].removeprefix("/") in names
    labels = item["Config"].get("Labels", {}) if kind == "container" else item.get("Labels", {})
    assert labels.get(LABEL) == nonce
    if kind == "container":
        assert item["Image"] == image
    else:
        assert kind == "network" and item["Driver"] == "bridge"


def check(args):
    if not args.reference_docker:
        assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1", "Use an isolated test engine"
        assert os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
    if args.output:
        assert not args.output.exists(), "Inspect the retained result before repeating this fixture"
    nonce = uuid.uuid4().hex
    prefix = "dns-order-" + nonce
    names = {"container": set(), "network": set()}
    pending = {"container": set(), "network": set()}
    record = {
        "schema": "asagiri.dns-network-order.v1", "status": "running",
        "reference_docker": args.reference_docker, "owner_nonce": nonce,
        "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "cases": [], "created": [], "cleanup": [],
    }

    def persist():
        if args.output:
            temporary = args.output.with_suffix(args.output.suffix + ".tmp")
            temporary.write_text(json.dumps(record, indent=2) + "\n")
            temporary.replace(args.output)

    def request(method, path, body=None, statuses=(200,), raw=False):
        connection = Connection(args.socket)
        try:
            connection.request(method, ("/v1.48" if args.reference_docker else "/v1.47") + path,
                               None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read(LIMIT + 1)
            assert len(data) <= LIMIT, "API response exceeded the fixture limit"
            assert response.status in statuses, (method, path, response.status, data[:1000])
            return response.status, data if raw else json.loads(data) if data else None
        finally:
            connection.close()

    def get(path):
        return request("GET", path)[1]

    def inventory():
        return {
            "container": sorted(row["Id"] for row in get("/containers/json?all=1")),
            "network": sorted(row["Id"] for row in get("/networks")),
            "images": sorted((row["Id"], sorted(row.get("RepoTags") or []),
                              sorted(row.get("RepoDigests") or []))
                             for row in get("/images/json?all=1")),
            "volumes": sorted(get("/volumes").get("Volumes") or [], key=lambda row: row["Name"]),
        }

    before = inventory()
    image = get("/images/" + urllib.parse.quote(args.image, safe="") + "/json")
    record.update(image_id=image["Id"], engine_version=get("/version"), original_inventory=before)
    assert image["Os"] == "linux"
    persist()

    def create(kind, name, body):
        names[kind].add(name)
        body.setdefault("Labels", {})[LABEL] = nonce
        record["pending_create"] = {"kind": kind, "name": name}
        persist()
        path = "/containers/create?" + urllib.parse.urlencode({"name": name}) if kind == "container" else "/networks/create"
        _, result = request("POST", path, body, (201,))
        identity = result["Id"]
        assert re.fullmatch(r"[0-9a-f]{64}", identity) and identity not in before[kind]
        pending[kind].add(identity)
        record["created"].append({"kind": kind, "name": name, "id": identity})
        record.pop("pending_create")
        persist()
        return identity

    def network(name, *, internal=False, ipv6=False):
        body = {"Name": name, "Driver": "bridge", "Internal": internal, "EnableIPv6": ipv6}
        if ipv6:
            # Only this generated bridge gets the private ULA. The API
            # rejects a collision; the fixture never changes host routes.
            value = uuid.uuid4().hex
            subnet = f"fd{value[:2]}:{value[2:6]}:{value[6:10]}::/64"
            body["IPAM"] = {"Driver": "default", "Config": [{"Subnet": subnet}]}
        return create("network", name, body)

    def container(name, net, alias=None):
        body = {
            "Image": image["Id"], "Entrypoint": [], "Cmd": ["sleep", "300"],
            "User": "65534:65534",
            "HostConfig": {"NetworkMode": net, "ReadonlyRootfs": True,
                           "Tmpfs": {"/tmp": "rw,nosuid,nodev,size=1m"},
                           "CapDrop": ["ALL"], "SecurityOpt": ["no-new-privileges"],
                           "Memory": 64 * 1024**2, "PidsLimit": 32},
        }
        if alias:
            body["NetworkingConfig"] = {"EndpointsConfig": {net: {"Aliases": [alias]}}}
        identity = create("container", name, body)
        request("POST", "/containers/" + identity + "/start", statuses=(204,))
        return identity

    def attach(cid, net, priority=None):
        body = {"Container": cid}
        if priority is not None:
            body["EndpointConfig"] = {"GwPriority": priority}
        request("POST", "/networks/" + net + "/connect", body, (200, 204))

    def endpoint(cid, net):
        return get("/containers/" + cid + "/json")["NetworkSettings"]["Networks"][net]

    def query(cid, kind):
        body = {"Cmd": ["timeout", "5", "nslookup", "-type=" + kind, "db.", "127.0.0.11"],
                "AttachStdout": True, "AttachStderr": True, "Tty": False}
        eid = request("POST", "/containers/" + cid + "/exec", body, (201,))[1]["Id"]
        assert re.fullmatch(r"[0-9a-f]{64}", eid)
        # Docker's archive API cannot copy from tmpfs. Read the attached
        # output to EOF before inspecting completion; a detached exec may
        # initially report Running=false before its process has started.
        _, output = request("POST", "/exec/" + eid + "/start",
                            {"Detach": False, "Tty": False}, raw=True)
        text = exec_output(output)
        deadline = time.monotonic() + 10
        while True:
            state = get("/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "DNS lookup did not finish after its output closed"
            time.sleep(.01)
        addresses = []
        in_answer = False
        for line in text.splitlines():
            if line.startswith("Name:"):
                assert line.split(":", 1)[1].strip().rstrip(".") == "db", text
                in_answer = True
            elif in_answer and re.match(r"^Address(?: [0-9]+)?:", line):
                addresses.append(str(ipaddress.ip_address(line.split(":", 1)[1].strip().split()[0])))
        assert state["ExitCode"] == 0 and addresses, (kind, state["ExitCode"], text)
        assert len(addresses) == len(set(addresses)), text
        return sorted(addresses), text

    def answers(cid, label, peers, net, ipv6):
        expected4 = sorted(endpoint(peer, net)["IPAddress"] for peer in peers)
        assert expected4 and all(expected4)
        row = {"name": label, "status": "running", "network": net, "container": cid,
               "expected_ipv4": expected4, "observations": []}
        # Multiple queries exercise every member of a duplicate alias, not
        # just whichever address happens to be printed first.
        for _ in range(3):
            actual, text = query(cid, "A")
            assert actual == expected4, (label, actual, expected4, text)
            row["observations"].append({"type": "A", "addresses": actual})
        if ipv6:
            expected6 = sorted(str(ipaddress.ip_address(endpoint(peer, net)["GlobalIPv6Address"]))
                               for peer in peers)
            actual, text = query(cid, "AAAA")
            assert actual == expected6, (label, actual, expected6, text)
            row["expected_ipv6"] = expected6
            row["observations"].append({"type": "AAAA", "addresses": actual})
        row["status"] = "passed"
        record["cases"].append(row)
        persist()

    def cleanup():
        errors = []
        for kind in ("container", "network"):
            try:
                query = urllib.parse.urlencode({"filters": json.dumps({"label": [LABEL + "=" + nonce]})})
                path = "/containers/json?all=1&" + query if kind == "container" else "/networks?" + query
                ids = pending[kind] | {row["Id"] for row in get(path)}
                for identity in sorted(ids):
                    try:
                        assert identity not in before[kind]
                        inspect_path = f"/{kind}s/{identity}" + ("/json" if kind == "container" else "")
                        status, item = request("GET", inspect_path, statuses=(200, 404))
                        if status != 404:
                            assert item["Id"] == identity
                            owned(kind, item, names[kind], nonce, before[kind], image["Id"])
                            request("DELETE", f"/{kind}s/{identity}"
                                    + ("?force=true" if kind == "container" else ""), statuses=(204, 404))
                        deadline = time.monotonic() + 10
                        while request("GET", inspect_path, statuses=(200, 404))[0] != 404:
                            assert time.monotonic() < deadline, "Owned object was not removed"
                            time.sleep(.05)
                        pending[kind].discard(identity)
                        record["cleanup"].append({"kind": kind, "id": identity, "absent": True})
                        persist()
                    except Exception as error:
                        errors.append({"kind": kind, "id": identity, "error": repr(error)})
            except Exception as error:
                errors.append({"kind": kind, "error": repr(error)})
        return errors

    try:
        scenarios = [
            ("shared-first", "a-shared", "z-project", False, False, False, False, 0),
            ("project-first", "z-shared", "a-project", False, False, False, False, 1),
            ("routed-first", "a-internal", "z-routed", True, False, False, False, 1),
            ("dual-stack-first", "a-ipv4", "z-dual", False, False, False, True, 1),
            ("internal-name-order", "a-internal4", "z-internal6", True, True, False, True, 0),
        ]
        for index, (case, first, second, internal0, internal1, ipv60, ipv61, preferred) in enumerate(scenarios):
            stem = prefix + "-" + str(index)
            nets = [stem + "-" + first, stem + "-" + second]
            versions = [ipv60, ipv61]
            ids = [network(nets[0], internal=internal0, ipv6=ipv60),
                   network(nets[1], internal=internal1, ipv6=ipv61)]
            # An earlier-sorting unjoined network also owns 'db'; it must
            # never contribute an address to either client's response.
            outsider = stem + "-000-unjoined"
            network(outsider, internal=True)
            container(stem + "-outsider", outsider, "db")
            peers = [[container(stem + "-shared0", nets[0], "db"),
                      container(stem + "-shared1", nets[0], "db")],
                     [container(stem + "-project", nets[1], "db")]]
            clients = [container(stem + "-client0", nets[0]), container(stem + "-client1", nets[1])]
            attach(clients[0], ids[1])
            attach(clients[1], ids[0])
            for order, cid in enumerate(clients):
                answers(cid, case + "-join-order-" + str(order), peers[preferred],
                        nets[preferred], versions[preferred])
            # Exercise endpoint removal on the client that joined the
            # preferred network second. Asagiri keeps its first attachment;
            # both initial join orders were checked above.
            lifecycle = clients[1 - preferred]
            request("POST", "/networks/" + ids[preferred] + "/disconnect",
                    {"Container": lifecycle}, (200, 204))
            other = 1 - preferred
            answers(lifecycle, case + "-disconnect-preferred", peers[other], nets[other], versions[other])
            attach(lifecycle, ids[preferred])
            answers(lifecycle, case + "-reconnect-preferred", peers[preferred], nets[preferred], versions[preferred])
            request("POST", "/containers/" + lifecycle + "/stop?t=0", statuses=(204,))
            request("POST", "/containers/" + lifecycle + "/wait?condition=not-running")
            request("POST", "/containers/" + lifecycle + "/start", statuses=(204,))
            answers(lifecycle, case + "-restart", peers[preferred], nets[preferred], versions[preferred])
            if case == "shared-first":
                request("POST", "/networks/" + ids[1] + "/disconnect",
                        {"Container": clients[0]}, (200, 204))
                attach(clients[0], ids[1], priority=10)
                answers(clients[0], "higher-gateway-priority", peers[1], nets[1], versions[1])
            errors = cleanup()
            assert not errors, errors
            assert inventory() == before, "Original inventory changed between DNS scenarios"
        assert len(record["cases"]) == 26
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error))
    finally:
        errors = cleanup()
        if errors:
            record.update(status="failed", cleanup_errors=errors)
        try:
            assert inventory() == before, "Original inventory changed"
            assert len(record["cleanup"]) == len(record["created"])
            assert len({(row["kind"], row["id"]) for row in record["cleanup"]}) == len(record["cleanup"])
            record["original_inventory_unchanged"] = True
        except BaseException as error:
            record.update(status="failed", final_check_error=repr(error))
        persist()
        print(json.dumps(record, indent=2), flush=True)
    assert record["status"] == "passed", "Inspect the DNS network-order evidence"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image")
    parser.add_argument("--reference-docker", action="store_true")
    parser.add_argument("--output", type=Path)
    check(parser.parse_args())
