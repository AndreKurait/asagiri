#!/usr/bin/env python3
"""Run the reported Java SDK/MySQL case with checks and Ryuk enabled."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


def main():
    assert sys.platform == "linux" and os.geteuid() == 0 and Path("/.dockerenv").is_file()
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1" and os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
    assert len(sys.argv) in (2, 4) and sys.argv[1] == "/run/asagiri/docker.sock"
    source = Path(__file__).resolve().parent / "testcontainers"
    images = json.loads((source / "images.json").read_text())
    sock = sys.argv[1]
    alpine, mysql = sys.argv[2:] if len(sys.argv) == 4 else (images["alpine"], images["mysql"])
    assert all("@sha256:" in image for image in (alpine, mysql)), "Use pinned image inputs"
    assert not (Path.home() / ".testcontainers.properties").exists(), "Use a fresh SDK configuration"
    assert shutil.which("mvn") and shutil.which("java"), "This probe requires Maven and a Java 17+ JDK"
    docker = ["docker", "--host", "unix://" + sock]
    for image in (alpine, mysql):
        subprocess.run(docker + ["pull", "-q", image], check=True, timeout=240)
    environment = dict(os.environ, DOCKER_HOST="unix://" + sock,
                       TESTCONTAINERS_DOCKER_SOCKET_OVERRIDE=sock,
                       TESTCONTAINERS_RYUK_CONTAINER_IMAGE=images["ryuk"])
    for key in ("DOCKER_CONTEXT", "TESTCONTAINERS_RYUK_DISABLED", "TESTCONTAINERS_CHECKS_DISABLE"):
        environment.pop(key, None)
    with tempfile.TemporaryDirectory(prefix="asagiri-testcontainers-") as directory:
        work = Path(directory)
        for name in ("pom.xml", "AsagiriTestcontainers.java"):
            shutil.copy2(source / name, work / name)
        cache = Path(os.environ.get("ASAGIRI_TEST_MAVEN_CACHE", str(work / "maven")))
        subprocess.run([
            "mvn", "--batch-mode", "--no-transfer-progress", "--strict-checksums",
            "-Dmaven.repo.local=" + str(cache),
            "org.apache.maven.plugins:maven-dependency-plugin:3.8.1:build-classpath",
            "-Dmdep.outputFile=" + str(work / "classpath"),
        ], cwd=work, env=environment, check=True, timeout=360)
        result = subprocess.run([
            "java", "-Xmx256m", "-classpath", (work / "classpath").read_text().strip(),
            str(work / "AsagiriTestcontainers.java"), alpine, mysql,
        ], cwd=work, env=environment, capture_output=True, text=True, timeout=300)
        print(result.stdout, flush=True)
        print(result.stderr, flush=True)
        assert result.returncode == 0, "The real Java SDK/MySQL probe failed"
        resources = [line.split()[1] for line in result.stdout.splitlines()
                     if line.startswith("ASAGIRI_TESTCONTAINERS_RESOURCE ")]
        assert len(resources) == 1
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            inspect = subprocess.run(docker + ["inspect", resources[0]], capture_output=True, text=True, timeout=10)
            if inspect.returncode:
                assert "No such" in inspect.stderr
                break
            time.sleep(0.5)
        else:
            raise AssertionError("Ryuk did not remove the deliberately unclosed SDK resource")
        version = json.loads(subprocess.check_output(
            docker + ["version", "--format", "{{json .Server}}"], text=True, timeout=10))
        assert version["Version"]
        print("PASS Testcontainers 1.20.3, MySQL, default checks, live log readiness and Ryuk resource cleanup", flush=True)


if __name__ == "__main__":
    main()
