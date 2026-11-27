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
    parser.add_argument("--read-mountinfo", action="store_true", help="independently verify source mounts in containers that provide cat")
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
        if not matches:
            assert "incompatible guest architectures" in result.stderr
        report = json.loads(result.stdout)
        expected_destination = {"cpuFeatures": None, "criuPackageVersion": None, "systemMountTargets": None, **destination}
        assert report["source"] == source and report["destination"] == expected_destination
        assert report["guestArchitectureMatches"] == matches
        source_features, destination_features = source["cpuFeatures"], expected_destination["cpuFeatures"]
        missing = None
        if matches and source_features is not None and destination_features is not None:
            missing = sorted(set(source_features) - set(destination_features))
        assert report["sourceCpuFeaturesMissingOnDestination"] == missing
        versions = (source["criuPackageVersion"], expected_destination["criuPackageVersion"])
        assert report["criuPackageVersionsMatch"] == (versions[0] == versions[1] if None not in versions else None)
        assert [row["id"] for row in report["containers"]] == list(original)
        current = inspect()
        missing_targets = set()
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
            mounted = row["protectedSystemMounts"]
            targets = expected_destination["systemMountTargets"]
            if source["systemMountTargets"] is not None:
                assert isinstance(mounted, list) and mounted == sorted(set(mounted))
                if args.read_mountinfo:
                    info = subprocess.check_output(["docker", "exec", row["id"], "cat", "/proc/1/mountinfo"])
                    actual = {line.split()[4] for line in info.splitlines()}
                    assert mounted == sorted(path for path in source["systemMountTargets"] if path.encode() in actual)
            else:
                assert mounted is None
            expected = None if mounted is None or targets is None else sorted(set(mounted) - set(targets))
            assert row["protectedSystemMountsMissingOnDestination"] == expected
            missing_targets.update(expected or [])
        assert (result.returncode == 0) == (matches and not missing_targets), result.stderr
        if matches and missing_targets:
            assert "destination lacks required system mount targets" in result.stderr
            assert all(path in result.stderr for path in missing_targets)
    print(f"Checkpoint preflight verified both destination architectures and {len(original)} running containers")


if __name__ == "__main__":
    main()
