#!/usr/bin/env python3
"""Boot a real kind cluster and check node/pod DNS in a disposable Linux engine."""
from contextlib import ExitStack
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import uuid


VERSION = "v0.27.0"
NODE = "kindest/node:v1.32.2@sha256:f226345927d7e348497136874b6d207e0b32cc52154ad8323129352923a3142f"
BINARIES = {
    "x86_64": ("amd64", "a6875aaea358acf0ac07786b1a6755d08fd640f4c79b7a2e46681cc13f49a04b"),
    "aarch64": ("arm64", "5e4507a41c69679562610b1be82ba4f80693a7826f4e9c6e39236169a3e4f9d0"),
}


def check(path, _image):
    assert platform.system() == "Linux" and os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
    assert Path("/.dockerenv").is_file() and Path(path).is_socket()
    name = "asagiri-kind-" + uuid.uuid4().hex[:10]
    node = name + "-control-plane"
    docker = ["docker", "--host", "unix://" + path]
    env = {**os.environ, "DOCKER_HOST": "unix://" + path, "KIND_EXPERIMENTAL_PROVIDER": "docker"}
    env.pop("DOCKER_CONTEXT", None)
    peer = None
    with tempfile.TemporaryDirectory(prefix="asagiri-kind-") as directory, ExitStack() as routes:
        # Reproduce a VPN split default in the private engine's namespace.
        # Keep the existing gateway and interface so the node can pull images.
        default = subprocess.check_output(["ip", "-4", "route", "show", "default"], text=True).splitlines()[0].split()
        gateway = default[default.index("via") + 1]
        interface = default[default.index("dev") + 1]
        for prefix in ("0.0.0.0/1", "128.0.0.0/1"):
            route = [prefix, "via", gateway, "dev", interface]
            subprocess.run(["ip", "route", "add", *route], check=True)
            routes.callback(subprocess.run, ["ip", "route", "del", *route], check=True)
        print(subprocess.check_output(["ip", "-4", "route", "show"], text=True), flush=True)
        root = Path(directory)
        architecture, expected = BINARIES[platform.machine()]
        # Exact official v0.27.0 release binaries and the release's default
        # node image, matching the upstream Linux reports.
        url = f"https://github.com/kubernetes-sigs/kind/releases/download/{VERSION}/kind-linux-{architecture}"
        kind = root / "kind"
        subprocess.run([
            "curl", "--fail", "--silent", "--show-error", "--location", "--retry", "2",
            "--connect-timeout", "60", "--max-time", "120", "--max-filesize", str(32 << 20),
            "--output", str(kind), url,
        ], check=True)
        assert kind.stat().st_size <= 32 << 20 and hashlib.sha256(kind.read_bytes()).hexdigest() == expected
        kind.chmod(0o700)
        config = root / "cluster.json"
        config.write_text(json.dumps({
            "kind": "Cluster", "apiVersion": "kind.x-k8s.io/v1alpha4",
            "networking": {"apiServerAddress": "127.0.0.1"},
            "nodes": [{"role": "control-plane"}],
        }))

        def command(args, timeout=60):
            result = subprocess.run(args, env=env, text=True, capture_output=True, timeout=timeout)
            assert result.returncode == 0, (args, result.returncode, result.stdout[-12000:], result.stderr[-12000:])
            return result.stdout.strip()

        def kubectl(*args, timeout=60):
            return command([*docker, "exec", node, "kubectl", "--kubeconfig=/etc/kubernetes/admin.conf", *args],
                           timeout=timeout)

        # A reused kind network would bypass the automatic allocation under test.
        network_names = command([*docker, "network", "ls", "--format", "{{.Name}}"]).splitlines()
        assert "kind" not in network_names, network_names
        try:
            result = command([str(kind), "create", "cluster", "--name", name, "--image", NODE,
                              "--config", str(config), "--kubeconfig", str(root / "kubeconfig"),
                              "--retain", "--wait", "120s"], timeout=360)
            print(result[-4000:], flush=True)
            network = json.loads(command([*docker, "network", "inspect", "kind"]))[0]
            subnets = [entry["Subnet"] for entry in network["IPAM"]["Config"]]
            assert any(ipaddress.ip_network(subnet).version == 4 for subnet in subnets), subnets
            kubectl("wait", "--for=condition=Ready", "nodes", "--all", "--timeout=120s", timeout=130)
            kubectl("-n", "kube-system", "rollout", "status", "deployment/coredns",
                    "--timeout=120s", timeout=130)
            resolver = command([*docker, "exec", node, "cat", "/etc/resolv.conf"])
            assert "nameserver" in resolver, resolver
            command([*docker, "exec", node, "getent", "ahostsv4", "registry-1.docker.io"], timeout=30)
            image = json.loads(Path("/src/scripts/ci/images.json").read_text())["perf"]["image"]
            peer = command([*docker, "run", "-d", "--network", "kind", "--network-alias", "kind-registry",
                            image, "sleep", "600"])
            inspected = json.loads(command([*docker, "inspect", peer]))[0]
            address = inspected["NetworkSettings"]["Networks"]["kind"]["IPAddress"]
            assert address in command([*docker, "exec", node, "getent", "ahostsv4", "kind-registry"])
            # This image is pulled by the node's own containerd, and these
            # queries use Kubernetes CoreDNS from a real pod. The absolute
            # external alias avoids Kubernetes' service search suffixes.
            pod_image = "mirror.gcr.io/library/" + image
            kubectl("run", "registry-dns-proof", "--restart=Never", "--image=" + pod_image,
                    "--command", "--", "sh", "-ec",
                    "nslookup -type=a registry-1.docker.io; nslookup -type=a kind-registry.; echo kind-dns-ok")
            kubectl("wait", "--for=jsonpath={.status.phase}=Succeeded", "pod/registry-dns-proof",
                    "--timeout=120s", timeout=130)
            output = kubectl("logs", "registry-dns-proof")
            assert "kind-dns-ok" in output and address in output, output
            nodes = json.loads(kubectl("get", "nodes", "-o", "json"))
            # A pod's cgroup OOM must not mark the node OOM-killed when
            # the node subsequently exits during a requested stop.
            pod = "memory-limit-proof"
            limits = {"spec": {"containers": [{"name": pod, "image": pod_image,
                      "command": ["sh", "-c", "while [ ! -f /tmp/start-proof ]; do sleep 1; done; exec tail /dev/zero >/dev/null"],
                      "resources": {"limits": {"memory": "32Mi"}}}]}}
            kubectl("run", pod, "--restart=Never", "--image=" + pod_image, "--overrides=" + json.dumps(limits))
            kubectl("wait", "--for=condition=Ready", "pod/" + pod, "--timeout=90s", timeout=100)
            kubectl("exec", pod, "--", "touch", "/tmp/start-proof")
            kubectl("wait", "--for=jsonpath={.status.containerStatuses[0].state.terminated.reason}=OOMKilled",
                    "pod/" + pod, "--timeout=90s", timeout=100)
            kubectl("wait", "--for=condition=Ready", "nodes", "--all", "--timeout=30s")
            # Preserve the final status while kubectl can still reach the node.
            print(kubectl("get", "pod", pod, "-o", "json"), flush=True)
            command([*docker, "stop", "--time", "5", node])
            stopped = json.loads(command([*docker, "inspect", node]))[0]["State"]
            assert stopped["Status"] == "exited" and stopped["OOMKilled"] is False, stopped
            print(json.dumps({"probe": "kind-cluster", "kind": VERSION, "nodeImage": NODE,
                              "kindNetworkSubnets": subnets, "nodesReady": len(nodes["items"]), "nodeResolver": resolver.splitlines(),
                              "podOOMKilled": True, "stoppedNodeOOMKilled": stopped["OOMKilled"],
                              "nodeContainerdPull": "passed", "nodeAndPodExternalDNS": "passed",
                              "nodeBareContainerAliasDNS": "passed",
                              "podAbsoluteContainerAliasDNS": "passed"}), flush=True)
        except BaseException:
            for arguments in (["logs", "--tail", "45", node],
                              ["exec", node, "kubectl", "--kubeconfig=/etc/kubernetes/admin.conf",
                               "get", "pod", "memory-limit-proof", "-o", "json"],
                              ["exec", node, "journalctl", "--no-pager", "-n", "45"],
                              ["exec", node, "kubectl", "--kubeconfig=/etc/kubernetes/admin.conf",
                               "logs", "registry-dns-proof"]):
                result = subprocess.run([*docker, *arguments], env=env, text=True, capture_output=True, timeout=20)
                print((result.stdout + result.stderr)[-12000:], flush=True)
            raise
        finally:
            if peer:
                subprocess.run([*docker, "rm", "-f", peer], env=env, capture_output=True, timeout=30)
            result = subprocess.run([str(kind), "delete", "cluster", "--name", name],
                                    env=env, text=True, capture_output=True, timeout=60)
            if result.returncode:
                print((result.stdout + result.stderr)[-4000:], flush=True)
            # Remove only any retained node with this unique cluster label.
            remaining = subprocess.run([*docker, "ps", "-aq", "--filter", "label=io.x-k8s.kind.cluster=" + name],
                                       env=env, text=True, capture_output=True, timeout=20)
            if remaining.returncode == 0:
                for cid in remaining.stdout.split():
                    subprocess.run([*docker, "rm", "-f", cid], env=env, capture_output=True, timeout=30)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2])
