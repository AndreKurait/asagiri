#!/usr/bin/env python3
"""Check implicit volume metadata and removal through real engine API calls."""
import argparse
from datetime import datetime, timezone
import hashlib
import http.client
import json
from pathlib import Path
import socket
import sys
from urllib.parse import quote, urlencode
import uuid

OWNER = "dev.asagiri.volume-label-test"


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image", help="an already local image; never pulled by this fixture")
    parser.add_argument("--reference-docker", action="store_true")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    nonce = uuid.uuid4().hex
    prefix = "asagiri-volume-labels-" + nonce
    containers, volumes = {}, {}
    record = {
        "schema": "asagiri.volume-labels.v1", "status": "running",
        "started_at": datetime.now(timezone.utc).isoformat(),
        "owner_nonce": nonce, "prefix": prefix,
        "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "reference_docker": args.reference_docker, "cases": [], "cleanup": [],
        "scope": "Real creation/inspection/removal metadata; no container process is started.",
    }

    def request(method, path, body=None, expected=(200,)):
        connection = Connection(args.socket)
        try:
            connection.request(method, "/v1.47" + path,
                               None if body is None else json.dumps(body).encode(),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            raw = response.read(2 * 1024**2 + 1)
            assert len(raw) <= 2 * 1024**2
            assert response.status in expected, (method, path, response.status, raw[:1000])
            return response.status, json.loads(raw) if raw else None
        finally:
            connection.close()

    def inventory():
        images = request("GET", "/images/json")[1]
        return {
            "containers": sorted(row["Id"] for row in request("GET", "/containers/json?all=1")[1]),
            "images": sorted((row["Id"], sorted(row.get("RepoTags") or []),
                              sorted(row.get("RepoDigests") or [])) for row in images),
            "volumes": sorted((row["Name"], row.get("Driver"), row.get("Labels"), row.get("Options"))
                              for row in request("GET", "/volumes")[1].get("Volumes") or []),
        }

    def volume_path(name):
        return "/volumes/" + quote(name, safe="")

    def save():
        record["tracked_containers"] = list(containers)
        record["tracked_volumes"] = list(volumes)
        if args.output:
            temporary = args.output.with_suffix(args.output.suffix + ".tmp")
            temporary.write_text(json.dumps(record, indent=2) + "\n")
            temporary.replace(args.output)

    def check(row, condition, detail):
        if not condition:
            row["errors"].append(detail)

    def create_case(name, *, anonymous=False, driver=None, existing=False, invalid_target=False):
        row = {"name": name, "errors": []}
        record["cases"].append(row)
        container = prefix + "-" + name
        volume = None if anonymous else container
        requested = {OWNER: nonce, "case": name, "value": "metadata with spaces"}
        options = {"Labels": requested, "NoCopy": True}
        if driver is not None:
            options["DriverConfig"] = driver
        expected_labels = requested
        expected_options = (driver or {}).get("Options", {})
        if volume is not None:
            request("GET", volume_path(volume), expected=(404,))
            volumes[volume] = {"anonymous": False, "case": row, "proof": "unique requested name absent before create"}
        if existing:
            expected_labels = {OWNER: nonce, "keep": "original"}
            expected_options = {}
            request("POST", "/volumes/create", {
                "Name": volume, "Driver": "local", "Labels": expected_labels,
            }, expected=(201,))
            volumes[volume] = {"anonymous": False, "case": row, "proof": "explicit owned create"}
        request("GET", "/containers/" + container + "/json", expected=(404,))
        mount = {"Type": "volume", "Target": "relative" if invalid_target else "/data",
                 "VolumeOptions": options}
        if volume is not None:
            mount["Source"] = volume
        save()
        status, created = request("POST", "/containers/create?" + urlencode({"name": container}), {
            "Image": args.image, "Entrypoint": [], "Cmd": ["true"],
            "User": "65534:65534", "Labels": {OWNER: nonce},
            "HostConfig": {
                "NetworkMode": "none", "ReadonlyRootfs": True, "CapDrop": ["ALL"],
                "SecurityOpt": ["no-new-privileges"], "Memory": 32 * 1024**2,
                "PidsLimit": 16, "Mounts": [mount],
            },
        }, expected=(400,) if invalid_target else (201,))
        row["create_status"] = status
        if invalid_target:
            request("GET", "/containers/" + container + "/json", expected=(404,))
            status, leaked = request("GET", volume_path(volume), expected=(200, 404))
            if status == 200:
                assert leaked["Name"] == volume and (leaked.get("Labels") or {}).get(OWNER) == nonce
                volumes[volume] = {"anonymous": False, "case": row, "proof": "owned label and absent before failed create"}
            row["volume_status_after_rejection"] = status
            check(row, status == 404, "invalid mount target created a volume")
            return
        cid = created["Id"]
        containers[cid] = {"name": container, "case": row, "anonymous": anonymous}
        save()
        info = request("GET", "/containers/" + cid + "/json")[1]
        assert info["Config"]["Labels"].get(OWNER) == nonce and info["Name"].lstrip("/") == container
        assert info["State"]["Status"] == "created" and not info["State"]["Running"]
        mounts = [item for item in info["Mounts"] if item["Destination"] == "/data"]
        assert len(mounts) == 1 and mounts[0]["Type"] == "volume"
        actual_name = mounts[0]["Name"]
        if anonymous:
            assert actual_name not in original_volume_names
        else:
            assert actual_name == volume
        volumes[actual_name] = {"anonymous": anonymous, "case": row, "proof": "owned container mount"}
        actual = request("GET", volume_path(actual_name))[1]
        labels, driver_options = actual.get("Labels") or {}, actual.get("Options") or {}
        row.update(volume=actual_name, anonymous=anonymous, labels=labels, options=driver_options)
        check(row, all(labels.get(key) == value for key, value in expected_labels.items()),
              "requested or preexisting labels were not preserved")
        if existing:
            check(row, labels == expected_labels, "container mount changed an existing volume's labels")
        check(row, driver_options == expected_options, "requested or preexisting driver options were not preserved")
        check(row, actual["Driver"] == "local", "local driver identity changed")

    before = None
    save()
    try:
        record["engine"] = request("GET", "/version")[1]
        assert bool(record["engine"].get("AsagiriVersion")) != args.reference_docker
        image = request("GET", "/images/" + quote(args.image, safe="") + "/json")[1]
        assert not image["Config"].get("Volumes"), "Use an image without additional declared volumes"
        record["image_id"] = image["Id"]
        before = inventory()
        original_volume_names = {row[0] for row in before["volumes"]}
        create_case("named-no-driver")
        create_case("named-local-driver", driver={"Name": "local"})
        create_case("anonymous-no-driver", anonymous=True)
        tmpfs = {"Name": "local", "Options": {"type": "tmpfs", "device": "tmpfs", "o": "size=16m"}}
        create_case("anonymous-driver-options", anonymous=True, driver=tmpfs)
        create_case("existing-keeps-metadata", existing=True, driver=tmpfs)
        create_case("invalid-target-no-side-effect", driver={"Name": "local"}, invalid_target=True)
    except BaseException as error:
        record["error"] = repr(error)
    finally:
        cleanup_errors = []
        for cid, owned in containers.items():
            try:
                info = request("GET", "/containers/" + cid + "/json")[1]
                assert info["Name"].lstrip("/") == owned["name"] and info["Config"]["Labels"].get(OWNER) == nonce
                for mount in info["Mounts"]:
                    if mount["Type"] == "volume" and mount["Destination"] == "/data":
                        assert mount["Name"] not in original_volume_names
                        volumes.setdefault(mount["Name"], {
                            "anonymous": owned["anonymous"], "case": owned["case"], "proof": "owned container mount",
                        })
                request("DELETE", "/containers/" + cid + "?force=1&v=1", expected=(204,))
                request("GET", "/containers/" + cid + "/json", expected=(404,))
                record["cleanup"].append({"kind": "container", "id": cid, "absent": True})
            except BaseException as error:
                cleanup_errors.append(repr(error))
        for name, owned in volumes.items():
            try:
                status, volume = request("GET", volume_path(name), expected=(200, 404))
                row = owned["case"]
                if owned["anonymous"]:
                    row["anonymous_removed_with_container"] = status == 404
                    check(row, status == 404, "anonymous volume survived container removal with v=1")
                elif row["name"] != "invalid-target-no-side-effect":
                    check(row, status == 200, "named volume was removed with its container")
                if status == 200:
                    assert volume["Name"] == name and name not in original_volume_names
                    request("DELETE", volume_path(name), expected=(204,))
                request("GET", volume_path(name), expected=(404,))
                record["cleanup"].append({"kind": "volume", "name": name, "absent": True})
            except BaseException as error:
                cleanup_errors.append(repr(error))
        if before is not None:
            try:
                assert inventory() == before, "unrelated image, container or volume inventory changed"
                record["original_inventory_unchanged"] = True
            except BaseException as error:
                cleanup_errors.append(repr(error))
        if cleanup_errors:
            record["cleanup_errors"] = cleanup_errors
        for row in record["cases"]:
            row["status"] = "failed" if row["errors"] else "passed"
        record["status"] = (
            "passed" if len(record["cases"]) == 6 and not record.get("error")
            and not cleanup_errors and all(row["status"] == "passed" for row in record["cases"]) else "failed"
        )
        record["finished_at"] = datetime.now(timezone.utc).isoformat()
        save()
        text = json.dumps(record, indent=2) + "\n"
        print(text, end="", flush=True)
    return 0 if record["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
