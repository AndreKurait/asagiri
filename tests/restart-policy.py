#!/usr/bin/env python3
"""Reject invalid policies before changing container settings or creating volumes."""
import http.client
import json
import socket
import sys
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=10)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image):
    containers = []
    volumes = []

    def request(method, target, body=None, statuses=(200,)):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            if target == "/containers/create" and response.status == 201:
                containers.append(json.loads(data)["Id"])
            assert response.status in statuses, (method, target, response.status, data)
            return json.loads(data) if data else None
        finally:
            connection.close()

    invalid = ({"Name": "invalid"}, {"Name": "always", "MaximumRetryCount": 2},
               {"Name": "on-failure", "MaximumRetryCount": -1})
    try:
        for policy in invalid:
            volume = "kiri-policy-" + uuid.uuid4().hex
            volumes.append(volume)
            error = request("POST", "/containers/create",
                            {"Image": image, "Cmd": ["true"], "HostConfig": {
                                "RestartPolicy": policy,
                                "Mounts": [{"Type": "volume", "Source": volume, "Target": "/data"}]}},
                            (400,))
            assert error["message"], error
            request("GET", "/volumes/" + volume, statuses=(404,))
        print("PASS invalid creates do not create volumes", flush=True)

        cid = request("POST", "/containers/create", {"Image": image, "Cmd": ["true"]}, (201,))["Id"]
        before = request("GET", "/containers/" + cid + "/json")["HostConfig"]
        for policy in invalid:
            error = request("POST", "/containers/" + cid + "/update",
                            {"Memory": 67108864, "RestartPolicy": policy}, (400,))
            assert error["message"], error
            assert request("GET", "/containers/" + cid + "/json")["HostConfig"] == before
        print("PASS invalid updates preserve policy and resource fields", flush=True)

        for policy in ({"Name": "on-failure", "MaximumRetryCount": 3}, {"Name": "always", "MaximumRetryCount": 0},
                       {"Name": "unless-stopped", "MaximumRetryCount": 0}, {"Name": "no", "MaximumRetryCount": 0}):
            request("POST", "/containers/" + cid + "/update", {"RestartPolicy": policy})
            assert request("GET", "/containers/" + cid + "/json")["HostConfig"]["RestartPolicy"] == policy
        print("PASS valid policy updates", flush=True)
        error = request("POST", "/containers/create",
                        {"Image": image, "Cmd": ["true"], "HostConfig": {
                            "AutoRemove": True, "RestartPolicy": {"Name": "always"}}}, (400,))
        assert error["message"], error
        print("PASS auto-remove conflicts", flush=True)
        print("11 restart policy cases passed")
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for target, statuses in ([("/containers/" + cid + "?force=true", (204, 404)) for cid in containers]
                                 + [("/volumes/" + name, (204, 404)) for name in volumes]):
            try:
                request("DELETE", target, statuses=statuses)
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not primary_failure:
            raise RuntimeError("Could not remove all policy-test objects") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
