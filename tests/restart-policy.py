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

    # Docker's messages, word for word.
    unknown = ("invalid restart policy: unknown policy 'invalid'; "
               "use one of 'no', 'always', 'on-failure', or 'unless-stopped'")
    other_policy = "invalid restart policy: maximum retry count can only be used with 'on-failure'"
    invalid = (({"Name": "invalid"}, unknown),
               ({"Name": "always", "MaximumRetryCount": 2}, other_policy),
               ({"Name": "no", "MaximumRetryCount": -1}, other_policy + " and cannot be negative"),
               ({"Name": "on-failure", "MaximumRetryCount": -1},
                "invalid restart policy: maximum retry count cannot be negative"))
    # No name: a create reads it as "no", an update leaves the policy alone.
    unnamed = {"Name": "", "MaximumRetryCount": 2}
    try:
        for policy, message in invalid + ((unnamed, other_policy),):
            volume = "kiri-policy-" + uuid.uuid4().hex
            volumes.append(volume)
            error = request("POST", "/containers/create",
                            {"Image": image, "Cmd": ["true"], "HostConfig": {
                                "RestartPolicy": policy,
                                "Mounts": [{"Type": "volume", "Source": volume, "Target": "/data"}]}},
                            (400,))
            assert error["message"] == message, (policy, error)
            request("GET", "/volumes/" + volume, statuses=(404,))
        print("PASS invalid creates do not create volumes", flush=True)

        cid = request("POST", "/containers/create", {"Image": image, "Cmd": ["true"]}, (201,))["Id"]
        before = request("GET", "/containers/" + cid + "/json")["HostConfig"]
        for policy, message in invalid:
            error = request("POST", "/containers/" + cid + "/update",
                            {"Memory": 67108864, "RestartPolicy": policy}, (400,))
            assert error["message"] == message, (policy, error)
            assert request("GET", "/containers/" + cid + "/json")["HostConfig"] == before
        print("PASS invalid updates preserve policy and resource fields", flush=True)

        for policy in ({"Name": "always", "MaximumRetryCount": 0}, {"Name": "unless-stopped", "MaximumRetryCount": 0},
                       {"Name": "no", "MaximumRetryCount": 0}, {"Name": "on-failure", "MaximumRetryCount": 3}):
            request("POST", "/containers/" + cid + "/update", {"RestartPolicy": policy})
            assert request("GET", "/containers/" + cid + "/json")["HostConfig"]["RestartPolicy"] == policy
        request("POST", "/containers/" + cid + "/update", {"RestartPolicy": unnamed})
        assert request("GET", "/containers/" + cid + "/json")["HostConfig"]["RestartPolicy"] == policy
        print("PASS valid policy updates", flush=True)

        # The conflict with AutoRemove is reported before an invalid policy
        # on create, and after one on update.
        for policy in ({"Name": "always"}, {"Name": "invalid"}):
            error = request("POST", "/containers/create",
                            {"Image": image, "Cmd": ["true"], "HostConfig": {
                                "AutoRemove": True, "RestartPolicy": policy}}, (400,))
            assert error["message"] == "can't create 'AutoRemove' container with restart policy", (policy, error)
        cid = request("POST", "/containers/create",
                      {"Image": image, "Cmd": ["true"], "HostConfig": {"AutoRemove": True}}, (201,))["Id"]
        error = request("POST", "/containers/" + cid + "/update", {"RestartPolicy": {"Name": "always"}}, (409,))
        assert error["message"] == ("Cannot update container " + cid + ": Restart policy cannot be updated "
                                    "because AutoRemove is enabled for the container"), error
        error = request("POST", "/containers/" + cid + "/update", {"RestartPolicy": {"Name": "invalid"}}, (400,))
        assert error["message"] == unknown, error
        request("POST", "/containers/" + cid + "/update", {"RestartPolicy": {"Name": "no"}})
        assert request("GET", "/containers/" + cid + "/json")["HostConfig"]["RestartPolicy"] == {
            "Name": "no", "MaximumRetryCount": 0}
        print("PASS auto-remove conflicts", flush=True)

        missing = "/containers/kiri-policy-" + uuid.uuid4().hex + "/update"
        error = request("POST", missing, {"RestartPolicy": {"Name": "invalid"}}, (400,))
        assert error["message"] == unknown, error
        request("POST", missing, {"RestartPolicy": {"Name": "always"}}, (404,))
        print("PASS policy checked before container lookup", flush=True)
        print("21 restart policy cases passed")
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
