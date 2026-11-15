#!/usr/bin/env python3
"""Keep a container's image usable after its tag moves to another config.

Run against a private Asagiri or Docker daemon. Fixtures have unique names,
share one real rootfs layer, and use the supplied base image's architecture.
The receipt modes retain one stopped container for an actual daemon restart.
"""
import argparse
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import uuid


class Probe:
    def __init__(self, socket, prefix=None):
        self.socket = socket
        self.prefix = prefix or "asagiri-image-retention-" + uuid.uuid4().hex
        assert re.fullmatch(r"asagiri-image-retention-[0-9a-f]{32}", self.prefix)
        self.images = []
        self.containers = []
        self.checks = []

    def cli(self, *args, data=None, ok=True):
        result = subprocess.run(
            ["docker", "--host", "unix://" + self.socket, *args],
            input=data, capture_output=True, timeout=120,
        )
        if ok:
            assert result.returncode == 0, (args, result.stderr.decode(errors="replace"))
        return result

    def inspect(self, image):
        result = self.cli("image", "inspect", image, ok=False)
        assert result.returncode == 0, (
            "retained image identity is unavailable", image,
            result.stderr.decode(errors="replace"),
        )
        return json.loads(result.stdout)[0]

    def passed(self, message):
        self.checks.append(message)
        print("PASS " + message, flush=True)

    def create(self, name, image, *command):
        result = self.cli("create", "--network", "none", "--name", name, image, *command)
        cid = result.stdout.decode().strip()
        assert re.fullmatch(r"[0-9a-f]{64}", cid), result.stdout
        self.containers.append(cid)
        return cid

    def cleanup(self):
        errors = []
        for ref in [*self.containers, self.prefix + "-fresh", self.prefix + "-committed"]:
            result = self.cli("rm", "-f", ref, ok=False)
            if result.returncode and b"no such container" not in result.stderr.lower():
                errors.append(("container", ref, result.stderr.decode(errors="replace")))
        for ref in reversed(list(dict.fromkeys(self.images))):
            result = self.cli("image", "rm", "-f", ref, ok=False)
            if result.returncode and b"no such image" not in result.stderr.lower():
                errors.append(("image", ref, result.stderr.decode(errors="replace")))
        assert not errors, ("fixture cleanup failed", errors)


def tar_bytes(files):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w") as archive:
        for name, data in files:
            entry = tarfile.TarInfo(name)
            entry.size, entry.mode = len(data), 0o644
            archive.addfile(entry, io.BytesIO(data))
    return output.getvalue()


def image_archive(base, rootfs, tag, prefix, revision):
    marker = (prefix + " " + revision + "\n").encode()
    delta = tar_bytes([("retained-image-version", marker)])
    layers = [rootfs, delta]
    digests = [hashlib.sha256(layer).hexdigest() for layer in layers]
    config = copy.deepcopy(base["Config"] or {})
    config.update(
        Cmd=["/bin/cat", "/retained-image-version"], Entrypoint=None,
        WorkingDir="/", User="",
        Labels={"fixture.retention": prefix, "revision": revision},
    )
    document = {
        "architecture": base["Architecture"], "os": "linux", "config": config,
        "rootfs": {"type": "layers", "diff_ids": ["sha256:" + value for value in digests]},
    }
    if base.get("Variant"):
        document["variant"] = base["Variant"]
    encoded = json.dumps(document, sort_keys=True, separators=(",", ":")).encode()
    config_hex = hashlib.sha256(encoded).hexdigest()
    names = [digest + "/layer.tar" for digest in digests]
    manifest = [{"Config": config_hex + ".json", "RepoTags": [tag], "Layers": names}]
    archive = tar_bytes([
        (config_hex + ".json", encoded), *zip(names, layers),
        ("manifest.json", json.dumps(manifest).encode()),
    ])
    return "sha256:" + config_hex, archive, marker, document["rootfs"]["diff_ids"]


def verify_identity(probe, state):
    image = probe.inspect(state["old_id"])
    assert image["Id"] == state["old_id"], image
    assert image["RootFS"]["Layers"] == state["layers"], image["RootFS"]
    assert image["Config"]["Labels"] == state["labels"], image["Config"]
    assert not image.get("RepoDigests"), image.get("RepoDigests")
    container = json.loads(probe.cli("inspect", state["container"]).stdout)[0]
    assert container["Image"] == state["old_id"], container
    assert container["Config"]["Labels"] == state["labels"], container["Config"]
    assert probe.cli("start", "-a", state["container"]).stdout.decode() == state["output"]


def commit_and_run(probe, state, suffix):
    tag = probe.prefix + ":" + suffix
    probe.images.append(tag)
    result = probe.cli("commit", state["container"], tag)
    image_id = result.stdout.decode().strip()
    assert re.fullmatch(r"sha256:[0-9a-f]{64}", image_id), result.stdout
    probe.images.append(image_id)
    assert probe.inspect(tag)["Config"]["Labels"] == state["labels"]
    result = probe.cli("run", "--rm", "--network", "none",
                       "--name", probe.prefix + "-committed", tag)
    assert result.stdout.decode() == state["output"], result.stdout


def check(socket, base_name, receipt=None):
    probe = Probe(socket)
    retained = False
    try:
        base = probe.inspect(base_name)
        source = probe.create(probe.prefix + "-source", base_name, "true")
        rootfs = probe.cli("export", source).stdout
        assert rootfs, "the shared rootfs layer must contain real files"
        probe.cli("rm", source)
        probe.containers.remove(source)
        tag, alias = probe.prefix + ":moving", probe.prefix + ":retained"
        old_id, old_archive, output, layers = image_archive(base, rootfs, tag, probe.prefix, "old")
        new_id, new_archive, _, _ = image_archive(base, rootfs, tag, probe.prefix, "new")
        assert old_id != new_id
        probe.images.extend([old_id, new_id, tag, alias])
        probe.cli("load", "--quiet", data=old_archive)
        assert probe.inspect(tag)["Id"] == old_id
        cid = probe.create(probe.prefix + "-kept", tag)
        state = {
            "prefix": probe.prefix, "old_id": old_id, "container": cid,
            "layers": layers, "labels": {"fixture.retention": probe.prefix, "revision": "old"},
            "output": output.decode(), "alias": alias,
        }
        probe.cli("load", "--quiet", data=new_archive)
        assert probe.inspect(tag)["Id"] == new_id
        old = probe.inspect(old_id)
        assert not old.get("RepoTags") and not old.get("RepoDigests"), old
        dangling = probe.cli("images", "--filter", "dangling=true", "--no-trunc", "--quiet").stdout.decode().split()
        assert dangling.count(old_id) == 1, dangling
        verify_identity(probe, state)
        probe.passed("tag replacement retains the old image ID, config, layers and runnable container")

        probe.cli("image", "rm", tag)
        assert probe.cli("image", "inspect", new_id, ok=False).returncode != 0
        verify_identity(probe, state)
        probe.passed("removing the replacement preserves the shared rootfs and original container")

        saved = probe.cli("save", old_id).stdout
        with tarfile.open(fileobj=io.BytesIO(saved)) as archive:
            manifest = json.load(archive.extractfile("manifest.json"))
            assert len(manifest) == 1, manifest
            config = archive.extractfile(manifest[0]["Config"]).read()
            assert "sha256:" + hashlib.sha256(config).hexdigest() == old_id
        commit_and_run(probe, state, "committed")
        probe.passed("the displaced image can be saved and its original container committed")

        probe.cli("tag", old_id, alias)
        dangling = probe.cli("images", "--filter", "dangling=true", "--no-trunc", "--quiet").stdout.decode().split()
        assert old_id not in dangling, dangling
        assert probe.inspect(alias)["Id"] == old_id
        all_ids = probe.cli("images", "--no-trunc", "--quiet").stdout.decode().split()
        assert all_ids.count(old_id) == 1, all_ids
        probe.passed("tagging the retained image removes its redundant dangling record")

        probe.cli("tag", alias, tag)
        probe.cli("load", "--quiet", data=new_archive)
        assert probe.inspect(alias)["Id"] == old_id and probe.inspect(tag)["Id"] == new_id
        dangling = probe.cli("images", "--filter", "dangling=true", "--no-trunc", "--quiet").stdout.decode().split()
        assert old_id not in dangling, dangling
        probe.cli("image", "rm", tag)
        verify_identity(probe, state)
        probe.passed("an existing alias survives another tag replacement without a dangling duplicate")

        if receipt is not None:
            state["engine"] = probe.cli("info", "--format", "{{.ID}}").stdout.decode().strip()
            assert state["engine"]
            state["images"] = probe.images
            fd = os.open(receipt, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(fd, "w") as stream:
                json.dump(state, stream)
            retained = True
            probe.passed("the original container and retained image are ready for daemon restart")
    finally:
        if not retained:
            probe.cleanup()
    print(json.dumps({"probe": "image-tag-retention", "checks_passed": len(probe.checks),
                      "fixtures_retained": retained}), flush=True)


def verify_restart(socket, receipt):
    state = json.loads(Path(receipt).read_bytes())
    probe = Probe(socket, state["prefix"])
    assert re.fullmatch(r"[0-9a-f]{64}", state["container"])
    assert re.fullmatch(r"sha256:[0-9a-f]{64}", state["old_id"])
    assert state["alias"] == probe.prefix + ":retained"
    assert state["labels"] == {"fixture.retention": probe.prefix, "revision": "old"}
    assert state["output"] == probe.prefix + " old\n"
    assert all(re.fullmatch(r"sha256:[0-9a-f]{64}", value) or
               re.fullmatch(re.escape(probe.prefix) + r":[a-z]+", value)
               for value in state["images"])
    probe.images = state["images"]
    probe.containers = [state["container"]]
    try:
        assert probe.cli("info", "--format", "{{.ID}}").stdout.decode().strip() == state["engine"]
        assert probe.inspect(state["alias"])["Id"] == state["old_id"]
        verify_identity(probe, state)
        probe.passed("the original container and image metadata survive daemon restart")
        result = probe.cli("run", "--rm", "--network", "none",
                           "--name", probe.prefix + "-fresh", state["old_id"])
        assert result.stdout.decode() == state["output"], result.stdout
        commit_and_run(probe, state, "afterrestart")
        probe.passed("fresh containers and commits use the retained image after daemon restart")
    finally:
        probe.cleanup()
    Path(receipt).unlink()
    print(json.dumps({"probe": "image-tag-retention", "checks_passed": len(probe.checks),
                      "fixtures_retained": False}), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("base", nargs="?", default="alpine:3.21")
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--prepare-restart", type=Path)
    modes.add_argument("--verify-restart", type=Path)
    args = parser.parse_args()
    if args.verify_restart is not None:
        verify_restart(args.socket, args.verify_restart)
    else:
        check(args.socket, args.base, args.prepare_restart)
