"""Check a source engine against platform records from two real destinations."""
import argparse
import json
import os
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("matching", type=Path, help="checkpoint platform output from the same guest architecture")
    parser.add_argument("mismatching", type=Path, help="checkpoint platform output from a different architecture")
    parser.add_argument("containers", nargs="+", help="names of running source containers")
    args = parser.parse_args()
    cli = os.environ.get("ASAGIRI_BIN", "asagiri")
    source = json.loads(subprocess.check_output([cli, "checkpoint", "platform"], text=True))
    if len(args.containers) < 2:
        parser.error("supply at least two running containers")

    def inspect():
        items = json.loads(subprocess.check_output(["docker", "inspect", *args.containers], text=True))
        return {item["Id"]: item for item in items}

    original = inspect()
    assert len(original) == len(args.containers)
    assert all(item["State"]["Running"] and not item["State"]["Paused"] for item in original.values())
    for path, matches in [(args.mismatching, False), (args.matching, True)]:
        destination = json.loads(path.read_text())
        assert (source["guestArchitecture"] == destination["guestArchitecture"]) == matches
        result = subprocess.run(
            [cli, "checkpoint", "preflight", "--destination", path, *args.containers],
            text=True, capture_output=True, timeout=60,
        )
        assert (result.returncode == 0) == matches, result.stderr
        if not matches:
            assert "incompatible guest architectures" in result.stderr
        report = json.loads(result.stdout)
        assert report["source"] == source and report["destination"] == destination
        assert report["guestArchitectureMatches"] == matches
        assert [row["id"] for row in report["containers"]] == list(original)
        current = inspect()
        for row in report["containers"]:
            before, after = original[row["id"]], current[row["id"]]
            assert after["State"]["Pid"] == before["State"]["Pid"]
            assert after["State"]["Running"] and not after["State"]["Paused"]
            assert row["mounts"] == before["Mounts"] and row["image"] == before["Image"]
            assert row["networks"] == before["NetworkSettings"]["Networks"]
            assert row["ports"] == before["NetworkSettings"]["Ports"]
            assert row["dns"] == before["HostConfig"]["Dns"]
            assert row["extraHosts"] == before["HostConfig"]["ExtraHosts"]
            assert "Env" not in row and "Config" not in row
    print(f"Checkpoint preflight verified both destination architectures and {len(original)} running containers")


if __name__ == "__main__":
    main()
