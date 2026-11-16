#!/usr/bin/env python3
"""Compare overlapping archive uploads and lifecycle operations with Docker."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import hashlib
import http.client
import io
import json
from pathlib import Path
import socket
import tarfile
import time
from urllib.parse import urlencode
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path, timeout=30):
        super().__init__("localhost", timeout=timeout)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image", help="already local Alpine image; this fixture never pulls")
    parser.add_argument("--reference-docker", action="store_true")
    parser.add_argument("--case", choices=["stopped", "restart", "parallel"])
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    prefix = "asagiri-root-" + uuid.uuid4().hex
    owned = []
    record = {"schema": "asagiri.shared-root.api.v1", "status": "running",
              "started_at": datetime.now(timezone.utc).isoformat(), "image": args.image,
              "reference_docker": args.reference_docker, "cases": [], "cleanup": [], "failures": []}

    def save():
        if args.output:
            args.output.write_text(json.dumps(record, indent=2) + "\n")

    def request(method, path, value=None, expected=(200,), timeout=30, content_type="application/json"):
        connection = Connection(args.socket, timeout)
        try:
            connection.request(method, "/v1.47" + path,
                               value if isinstance(value, bytes) else None if value is None else json.dumps(value).encode(),
                               {"Content-Type": content_type})
            response = connection.getresponse()
            data = response.read(8 * 1024**2 + 1)
            assert len(data) <= 8 * 1024**2, "oversized fixture response"
            assert response.status in expected, (method, path, response.status, data[:1000])
            return response.status, data
        finally:
            connection.close()

    def create(name, short=False):
        owned.append(name)
        save()
        value = json.loads(request("POST", "/containers/create?" + urlencode({"name": name}), {
            "Image": args.image, "Entrypoint": [], "Cmd": ["true"] if short else ["sleep", "60"],
            "Labels": {"io.asagiri.test": "shared-root"},
            "HostConfig": {"NetworkMode": "none"},
        }, (201,))[1])
        return value["Id"]

    def remove(name):
        status, _ = request("DELETE", "/containers/" + name + "?force=1&v=1", expected=(204, 404))
        absent, _ = request("GET", "/containers/" + name + "/json", expected=(404,))
        record["cleanup"].append({"container": name, "delete_status": status, "inspect_status": absent})
        owned.remove(name)
        save()

    def begin_copy(cid, filename):
        payload = (filename.encode() + b"\n") * 16384
        tarbytes = io.BytesIO()
        with tarfile.open(fileobj=tarbytes, mode="w") as archive:
            header = tarfile.TarInfo(filename)
            header.size, header.mode = len(payload), 0o644
            archive.addfile(header, io.BytesIO(payload))
        body = tarbytes.getvalue()
        connection = Connection(args.socket)
        connection.putrequest("PUT", "/v1.47/containers/" + cid + "/archive?path=/tmp")
        connection.putheader("Content-Type", "application/x-tar")
        connection.putheader("Content-Length", str(len(body)))
        connection.endheaders()
        connection.send(body[:64 * 1024])
        return {"connection": connection, "body": body, "payload": payload, "filename": filename}

    def changes(cid):
        value = json.loads(request("GET", "/containers/" + cid + "/changes", timeout=2)[1])
        # JSON null is Docker's empty diff. An empty HTTP body or malformed
        # JSON remains a failure, including while waiting for the first write.
        if value is None:
            return []
        assert isinstance(value, list), value
        assert all(isinstance(change, dict) and isinstance(change.get("Path"), str)
                   and change.get("Kind") in (0, 1, 2) for change in value), value
        return value

    def writer_ready(cid, filename):
        # Docker may hold its per-container operation lock throughout an
        # upload. Its baseline is allowed to serialize start/stop behind the
        # copy. Asagiri's upper-layer changes give a direct readiness proof.
        if args.reference_docker:
            time.sleep(0.15)
            return {"method": "incomplete HTTP upload", "observed_upper_write": False}
        deadline = time.monotonic() + 5
        while True:
            observed = changes(cid)
            if any(change["Path"] == "/tmp/" + filename for change in observed):
                return {"method": "upper-layer changes", "observed_upper_write": True}
            assert time.monotonic() < deadline, ("copy did not begin writing", filename, observed)
            time.sleep(0.02)

    def finish_copy(upload):
        connection = upload["connection"]
        try:
            connection.send(upload["body"][64 * 1024:])
            response = connection.getresponse()
            data = response.read(64 * 1024 + 1)
            return {"status": response.status, "body": data.decode(errors="replace")[:1000]}
        except (OSError, http.client.HTTPException) as error:
            return {"error": repr(error)}
        finally:
            connection.close()

    def verify_file(cid, upload):
        body = request("GET", "/containers/" + cid + "/archive?" + urlencode({"path": "/tmp/" + upload["filename"]}))[1]
        with tarfile.open(fileobj=io.BytesIO(body), mode="r:*") as archive:
            members = [member for member in archive.getmembers() if member.isfile()]
            assert len(members) == 1, members
            data = archive.extractfile(members[0]).read()
        assert data == upload["payload"], (upload["filename"], len(data), len(upload["payload"]))
        return {"file": upload["filename"], "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}

    save()
    try:
        record["engine"] = json.loads(request("GET", "/version")[1])
        assert (record["engine"].get("Platform", {}).get("Name") == "Asagiri") is not args.reference_docker
        request("GET", "/images/" + args.image + "/json")
        for case in [args.case] if args.case else ["stopped", "restart", "parallel"]:
            result = {"case": case, "passed": False, "writers": [], "copies": []}
            record["cases"].append(result)
            name = prefix + "-" + case
            cid = create(name)
            uploads = []
            try:
                result["created_changes"] = changes(cid)
                assert result["created_changes"] == [], result
                # Initialize an empty archive view without creating runtime
                # files; its diff must still be valid, empty JSON.
                request("PUT", "/containers/" + cid + "/archive?path=/", b"\0" * 10240,
                        content_type="application/x-tar")
                result["empty_archive_changes"] = changes(cid)
                assert result["empty_archive_changes"] == [], result
                if case == "restart":
                    request("POST", "/containers/" + cid + "/start", expected=(204,))
                for index in range(2 if case == "parallel" else 1):
                    upload = begin_copy(cid, f"shared-root-{case}-{index}")
                    uploads.append(upload)
                    result["writers"].append(writer_ready(cid, upload["filename"]))

                def lifecycle():
                    started = time.monotonic()
                    value = {}
                    try:
                        if case == "restart":
                            value["stop_status"] = request("POST", "/containers/" + cid + "/stop?t=0", expected=(204, 304, 409, 500))[0]
                        status, body = request("POST", "/containers/" + cid + "/start", expected=(204, 304, 409, 500))
                        value.update(start_status=status, body=body.decode(errors="replace")[:1000])
                    except BaseException as error:
                        value["error"] = repr(error)
                    value["seconds"] = time.monotonic() - started
                    return value

                with ThreadPoolExecutor(max_workers=1) as pool:
                    operation = pool.submit(lifecycle)
                    try:
                        # A pending same-container operation must not consume
                        # or delay the reply to an unrelated control request.
                        other = prefix + "-" + case + "-other"
                        other_id = create(other, short=True)
                        start = time.monotonic()
                        request("POST", "/containers/" + other_id + "/start", expected=(204,))
                        waited = json.loads(request("POST", "/containers/" + other_id + "/wait?condition=not-running")[1])
                        assert waited["StatusCode"] == 0, waited
                        result["unrelated_seconds"] = time.monotonic() - start
                        remove(other)
                        result["lifecycle_finished_before_upload_completed"] = operation.done()
                    finally:
                        result["copies"] = [finish_copy(upload) for upload in uploads]
                    result["lifecycle"] = operation.result(timeout=35)

                assert all(copy.get("status") == 200 for copy in result["copies"]), result["copies"]
                assert result["lifecycle"].get("start_status") == 204, result["lifecycle"]
                if case == "restart":
                    assert result["lifecycle"].get("stop_status") == 204, result["lifecycle"]
                result["files"] = [verify_file(cid, upload) for upload in uploads]
                # The writable data remains usable on a subsequent ordinary
                # stop/start after every copy connection and worker finished.
                request("POST", "/containers/" + cid + "/stop?t=0", expected=(204,))
                request("POST", "/containers/" + cid + "/start", expected=(204,))
                result["after_restart"] = [verify_file(cid, upload) for upload in uploads]
                result["passed"] = True
            except BaseException as error:
                result["error"] = repr(error)
                record["failures"].append({"case": case, "error": repr(error)})
            finally:
                for upload in uploads:
                    upload["connection"].close()
                remove(name)
                save()
        record["status"] = "passed" if not record["failures"] else "failed"
    except BaseException as error:
        record["status"] = "failed"
        record["failures"].append({"case": "setup", "error": repr(error)})
    finally:
        for name in list(owned):
            try:
                remove(name)
            except BaseException as error:
                record["status"] = "failed"
                record["failures"].append({"case": "cleanup", "container": name, "error": repr(error)})
        record["finished_at"] = datetime.now(timezone.utc).isoformat()
        save()
        print(json.dumps(record, indent=2), flush=True)
    return 0 if record["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
