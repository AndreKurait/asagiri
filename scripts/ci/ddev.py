#!/usr/bin/env python3
"""Run a pinned DDEV PHP/MariaDB/HTTPS project on a private Linux engine."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[2]
SOCKET = "unix:///run/asagiri/docker.sock"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def download_inputs(pin, arch, directory):
    directory.mkdir()
    asset = pin["archives"][arch]
    archive = directory / asset["name"]
    url = "https://github.com/ddev/ddev/releases/download/" + pin["version"] + "/" + asset["name"]
    with urllib.request.urlopen(url, timeout=120) as response, archive.open("wb") as out:
        shutil.copyfileobj(response, out)
    if archive.stat().st_size != asset["bytes"] or digest(archive) != asset["sha256"]:
        raise ValueError("DDEV archive does not match the pinned digest and size")
    hashes = {}
    with tarfile.open(archive, "r:gz") as bundle:
        for name in ("ddev", "mkcert", "ddev-hostname"):
            matches = [item for item in bundle.getmembers() if item.name in (name, "./" + name)]
            if len(matches) != 1 or not matches[0].isfile() or matches[0].size > 100 * 1024**2:
                raise ValueError("Unexpected DDEV archive member: " + name)
            with bundle.extractfile(matches[0]) as source, (directory / name).open("wb") as out:
                shutil.copyfileobj(source, out)
            (directory / name).chmod(0o755)
            hashes[name] = digest(directory / name)
    return {"archive": asset, "binaries": hashes}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bin", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.bin = args.bin.resolve()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    if shutil.disk_usage(args.output).free < 8 * 1024**3:
        raise RuntimeError("DDEV test needs at least 8 GiB free")
    pin = json.loads((ROOT / "tests/ddev/pins.json").read_text())
    docker_arch = subprocess.check_output(["docker", "info", "--format", "{{.Architecture}}"], text=True).strip()
    arch = {"aarch64": "arm64", "arm64": "arm64", "x86_64": "amd64", "amd64": "amd64"}.get(docker_arch)
    if arch not in pin["archives"]:
        raise ValueError("No pinned DDEV archive for Docker host " + docker_arch)
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    if (args.bin / "SOURCE_COMMIT").read_text().strip() != source:
        raise ValueError("DDEV must test binaries built from this checkout's commit")
    expected = dict(line.split(maxsplit=1)[::-1] for line in (args.bin / "SHA256SUMS").read_text().splitlines())
    binaries = {name: digest(args.bin / name) for name in ("asagiri", "asagiri-init")}
    if any(expected.get(name) != value for name, value in binaries.items()):
        raise ValueError("Engine binary digest mismatch")
    inputs = args.output / "inputs"
    checked_inputs = download_inputs(pin, arch, inputs)
    name = "asagiri-ddev-" + uuid.uuid4().hex[:12]
    record = {
        "schema": "asagiri.ddev.v1", "source_commit": source,
        "worktree_clean": not subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True).strip(),
        "started_at": datetime.now(timezone.utc).isoformat(),
        "architecture": arch, "binaries": binaries, "inputs": checked_inputs,
        "ddev": pin["version"], "ddev_source": pin["source_commit"],
        "host_image": pin["host_image"], "pinned_images": pin["images"],
        "container": name, "outer_ports_published": False, "user_docker_socket_mounted": False,
        "status": "running", "steps": [],
    }
    log = (args.output / "test.log").open("w")
    created = False

    def save():
        (args.output / "verification.json").write_text(json.dumps(record, indent=2) + "\n")

    def step(label, command, timeout=180):
        print(label, flush=True)
        log.write("\nSTEP " + label + "\n")
        log.flush()
        start = time.monotonic()
        result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, timeout=timeout)
        record["steps"].append({"name": label, "exit": result.returncode, "seconds": time.monotonic() - start})
        save()
        if result.returncode:
            raise RuntimeError(label + " failed")
        if shutil.disk_usage(args.output).free < 3 * 1024**3:
            raise RuntimeError("DDEV test free-space guard reached")

    def user(*command):
        return ["docker", "exec", "-w", "/home/ddevtest/project", name, "sudo", "-H", "-u", "ddevtest", "env",
                "DOCKER_HOST=" + SOCKET, "DDEV_NONINTERACTIVE=true", "DDEV_NO_INSTRUMENTATION=true", *command]

    def dock(*command):
        return ["docker", "exec", name, "docker", "-H", SOCKET, *command]

    def inspect_images():
        images = json.loads(subprocess.check_output(dock("image", "inspect", *pin["images"]), text=True))
        for (tag, expected_digest), image in zip(pin["images"].items(), images, strict=True):
            if not any(item.endswith("@" + expected_digest) for item in image["RepoDigests"]):
                raise ValueError("DDEV base image changed: " + tag)
        (args.output / "pinned-image-inspect.json").write_text(json.dumps(images, indent=2) + "\n")
        return images

    try:
        # Set before docker run: a failed start may still create a container.
        created = True
        step("create isolated Linux engine", [
            "docker", "run", "-d", "--name", name, "--privileged", "--cgroupns=private",
            "--tmpfs", "/run", "--tmpfs", "/tmp",
            "--sysctl", "net.ipv6.conf.all.disable_ipv6=0", "--sysctl", "net.ipv6.conf.default.disable_ipv6=0",
            "-e", "ASAGIRI_SOCKET=/run/asagiri/docker.sock", "-e", "ASAGIRI_DATA_DIR=/test-store", "-e", "ASAGIRI_TEST_ENGINE=1",
            "-v", str(args.bin) + ":/opt/asagiri:ro", "-v", str(inputs) + ":/ddev-inputs:ro",
            "-v", str(ROOT / "tests/ddev") + ":/fixture:ro",
            "-v", "/var/lib/asagiri", "-v", "/test-store", "-v", "/home/ddevtest",
            pin["host_image"], "sh", "-ec", """
                apk add -q --no-cache python3 bash curl git sudo openssl nss-tools util-linux coreutils
                adduser -D -s /bin/bash ddevtest
                printf 'ddevtest ALL=(ALL) NOPASSWD: ALL\\n' > /etc/sudoers.d/ddevtest
                chmod 440 /etc/sudoers.d/ddevtest
                install -m 755 /ddev-inputs/ddev /ddev-inputs/mkcert /ddev-inputs/ddev-hostname /usr/local/bin/
                mkdir -p /home/ddevtest/project/public
                cp /fixture/index.php /home/ddevtest/project/public/index.php
                chown -R ddevtest:ddevtest /home/ddevtest
                exec /opt/asagiri/asagiri daemon
            """,
        ])
        inspected = json.loads(subprocess.check_output(["docker", "inspect", name], text=True))[0]
        allowed = {str(args.bin), str(inputs), str(ROOT / "tests/ddev")}
        if inspected["HostConfig"]["PortBindings"] or inspected["HostConfig"]["CgroupnsMode"] != "private":
            raise ValueError("DDEV host isolation mismatch")
        if any(mount["Source"] not in allowed or mount["RW"] for mount in inspected["Mounts"] if mount["Type"] == "bind"):
            raise ValueError("Unexpected DDEV host bind mount")
        (args.output / "host-inspect.json").write_text(json.dumps(inspected, indent=2) + "\n")
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            if subprocess.run(dock("version"), capture_output=True).returncode == 0:
                break
            time.sleep(1)
        else:
            raise TimeoutError("Private DDEV engine did not start")
        step("grant the test user access to its private engine", [
            "docker", "exec", name, "sh", "-ec", "chgrp ddevtest /run/asagiri/docker.sock && chmod 660 /run/asagiri/docker.sock",
        ])
        step("record DDEV and engine identities", user("bash", "-ec", "ddev version -j\ndocker version\nuname -a"))
        for tag, image_digest in pin["images"].items():
            reference = tag + "@" + image_digest
            step("pull pinned " + tag, dock("pull", reference), timeout=600)
            step("assign the DDEV image tag " + tag, dock("tag", reference, tag))
        inspect_images()
        step("configure disposable DDEV preferences and CA", user("bash", "-ec", """
            ddev config global --instrumentation-opt-in=false --omit-containers=ddev-ssh-agent
            mkcert -install
        """))
        step("configure PHP 8.4 and MariaDB 11.8", user(
            "ddev", "config", "--project-name=asagiri-ddev", "--project-type=php",
            "--docroot=public", "--php-version=8.4", "--database=mariadb:11.8",
            "--router-http-port=8080", "--router-https-port=8443",
        ))
        step("start DDEV database, web service and router", user("ddev", "start"), timeout=900)
        bases = inspect_images()
        # DDEV builds a small user-specific layer. It must preserve the
        # verified base's complete diff-ID prefix.
        for tag, base in zip(pin["images"], bases, strict=True):
            if "ddev-webserver:" not in tag and "ddev-dbserver-mariadb-11.8:" not in tag:
                continue
            derived = json.loads(subprocess.check_output(dock("image", "inspect", tag + "-asagiri-ddev-built"), text=True))[0]
            layers = base["RootFS"]["Layers"]
            if not layers or derived["RootFS"]["Layers"][:len(layers)] != layers:
                raise ValueError("DDEV derived image does not contain the pinned base layers")
        request = r'''
            test "$(curl --fail --silent --show-error --max-time 30 \
                --resolve asagiri-ddev.ddev.site:8080:127.0.0.1 \
                http://asagiri-ddev.ddev.site:8080/)" = asagiri-ddev:42
            test "$(curl --fail --silent --show-error --max-time 30 \
                --cacert "$(mkcert -CAROOT)/rootCA.pem" \
                --resolve asagiri-ddev.ddev.site:8443:127.0.0.1 \
                https://asagiri-ddev.ddev.site:8443/)" = asagiri-ddev:42
            ddev describe -j
            echo "PASS HTTP and trusted HTTPS PHP/MariaDB requests"
        '''
        step("verify HTTP, trusted HTTPS and the SQL result", user("bash", "-ec", request))
        step("restart DDEV and recopy router configuration", user("ddev", "restart"), timeout=600)
        step("verify HTTP, HTTPS and SQL after restart", user("bash", "-ec", request))
        inspect_images()
        step("start a second project on the shared DDEV network", user("bash", "-ec", """
            mkdir -p ../peer/public
            cd ../peer
            ddev config --project-name=asagiri-peer --project-type=php --docroot=public \
                --php-version=8.4 --database=mariadb:11.8 --router-http-port=8080 --router-https-port=8443
            ddev start
        """), timeout=900)
        # Seed each database directly so a wrong DNS answer cannot also seed the wrong marker.
        for project in ("asagiri-ddev", "asagiri-peer"):
            step("seed database marker for " + project, dock(
                "exec", "ddev-" + project + "-db", "mariadb", "-udb", "-pdb", "db", "-e",
                "CREATE TABLE dns_marker (project VARCHAR(32)); "
                + f"INSERT INTO dns_marker VALUES ('{project}')",
            ))
        def check_project_databases():
            for project in ("asagiri-ddev", "asagiri-peer"):
                step("check web-to-database routing for " + project, dock(
                    "exec", "ddev-" + project + "-web", "php", "-r",
                    '$pdo = new PDO("mysql:host=db;dbname=db", "db", "db"); '
                    '$marker = $pdo->query("SELECT project FROM dns_marker")->fetchColumn(); '
                    'echo $marker, PHP_EOL; exit($marker === $argv[1] ? 0 : 1);', project,
                ))

        check_project_databases()
        step("restart the first project while its peer stays up", user("ddev", "restart"), timeout=600)
        check_project_databases()
        step("stop the project and router", user("ddev", "poweroff"))
        record["status"] = "passed"
    except BaseException as error:
        record["status"] = "failed"
        record["error"] = type(error).__name__ + ": " + str(error)
        raise
    finally:
        if created:
            try:
                with (args.output / "engine.log").open("w") as stream:
                    subprocess.run(["docker", "logs", name], stdout=stream, stderr=subprocess.STDOUT, timeout=30)
                for filename, command in (("containers.json", ("ps", "-a", "--no-trunc", "--format", "{{json .}}")),
                                          ("images.json", ("image", "ls", "--digests", "--no-trunc", "--format", "{{json .}}"))):
                    with (args.output / filename).open("w") as stream:
                        subprocess.run(dock(*command), stdout=stream, stderr=subprocess.STDOUT, timeout=30)
            except subprocess.TimeoutExpired as error:
                record["diagnostic_error"] = str(error)
            finally:
                try:
                    removed = subprocess.run(["docker", "rm", "-fv", name], capture_output=True, text=True, timeout=60)
                    record["private_host_removed"] = removed.returncode == 0
                    if removed.returncode:
                        record["cleanup_error"] = removed.stderr
                        record["status"] = "failed"
                except subprocess.TimeoutExpired as error:
                    record["private_host_removed"] = False
                    record["cleanup_error"] = str(error)
                    record["status"] = "failed"
        log.close()
        record["completed_at"] = datetime.now(timezone.utc).isoformat()
        record["log_sha256"] = digest(args.output / "test.log")
        save()
    if record["status"] != "passed":
        raise RuntimeError("DDEV verification or private-host cleanup failed")
    print("DDEV PHP/MariaDB/router, trusted HTTPS, restart and cleanup passed")


if __name__ == "__main__":
    main()
