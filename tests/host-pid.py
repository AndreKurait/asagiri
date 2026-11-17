#!/usr/bin/env python3
"""Host PID sharing, private namespaces, subreaping, and scoped cleanup.

Run on the Linux host of a disposable Asagiri engine, with an already cached
Alpine image. The sentinel is a child of this test, outside every test container.
"""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import uuid


ORPHAN_WORKLOAD = """\
cat > /tmp/orphan-script <<'SH'
while [ ! -e /tmp/release-orphan ]; do sleep 0.1; done
SH
sh -ec 'sh /tmp/orphan-script & echo "$!" > /tmp/orphan-pid'
printf 'host-pid-output\\n'
exec sleep 300
"""


def process(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return {"pid": pid, "state": fields[0], "parent": int(fields[1]), "start": int(fields[19])}
    except FileNotFoundError:
        return None


def namespace(pid, kind):
    return os.readlink(f"/proc/{pid}/ns/{kind}")


def until(probe, reason, timeout=10):
    deadline = time.monotonic() + timeout
    while True:
        value = probe()
        if value:
            return value
        assert time.monotonic() < deadline, reason
        time.sleep(0.05)


def check(args):
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1", "requires a disposable test engine"
    owner = "asagiri-host-pid-" + uuid.uuid4().hex
    owned, watched = [], []
    sentinel = None
    record = {"probe": "host-pid", "owner": owner, "status": "running",
              "started_at": datetime.now(timezone.utc).isoformat(), "checks": [], "cleanup": []}
    docker = ["docker", "--host", "unix://" + args.socket]

    def cli(*words, timeout=30):
        result = subprocess.run(docker + list(words), stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
        assert result.returncode == 0, (words, result.returncode, result.stdout.decode(errors="replace"),
                                       result.stderr.decode(errors="replace"))
        return result.stdout.decode().strip()

    def inspect(cid):
        info = json.loads(cli("inspect", cid))[0]
        assert info["Config"]["Labels"].get("dev.asagiri.test.owner") == owner
        return info

    def remove(cid):
        inspect(cid)
        cli("rm", "--force", "--volumes", cid)
        result = subprocess.run(docker + ["inspect", cid], capture_output=True, timeout=30)
        assert result.returncode != 0 and b"No such" in result.stderr, result
        owned.remove(cid)
        record["cleanup"].append({"container": cid, "removed": True})

    def watch(pid):
        assert pid > 1, pid
        info = process(pid)
        assert info and info["state"] != "Z", info
        watched.append(info)
        return info

    def gone(info):
        now = process(info["pid"])
        return now is None or now["start"] != info["start"]

    def create(options=(), command=("sleep", "300"), network="none"):
        cid = cli("create", "--name", owner + "-" + str(len(owned)),
                  "--label", "dev.asagiri.test.owner=" + owner, "--network", network,
                  "--read-only", "--memory", "64m", "--cpus", "0.5", "--pids-limit", "32",
                  "--tmpfs", "/tmp:rw,nosuid,size=2m", *options, args.image, *command)
        assert len(cid) == 64 and all(c in "0123456789abcdef" for c in cid), cid
        owned.append(cid)
        cli("start", cid)
        info = inspect(cid)
        watch(info["State"]["Pid"])
        return cid

    def execute(cid, *command):
        return cli("exec", cid, *command)

    def orphan(cid):
        text = until(lambda: execute(cid, "sh", "-ec", "cat /tmp/orphan-pid 2>/dev/null || true"),
                     "workload did not publish its orphan PID")
        assert text.isdecimal(), text
        return watch(int(text))

    def sentinel_alive():
        assert sentinel is not None and sentinel.poll() is None, "unrelated host sentinel was terminated"
        assert process(sentinel.pid)["start"] == record["sentinel"]["start"]

    def private_mounts(pid, host_network=False):
        for kind in ("mnt", "uts", "ipc", "cgroup"):
            assert namespace(pid, kind) != namespace(os.getpid(), kind), (pid, kind)
        assert (namespace(pid, "net") == namespace(os.getpid(), "net")) == host_network

    def passed(name):
        sentinel_alive()
        record["checks"].append(name)
        print("PASS " + name, flush=True)

    def interrupted(number, _frame):
        raise InterruptedError(f"received signal {number}")

    previous = {number: signal.signal(number, interrupted) for number in (signal.SIGINT, signal.SIGTERM)}
    try:
        version = json.loads(cli("version", "--format", "{{json .Server}}"))
        assert version["Platform"]["Name"] == "Asagiri", version
        record["engine"] = version
        cli("image", "inspect", args.image)
        sentinel = subprocess.Popen([sys.executable, "-I", "-B", "-c", "import time; time.sleep(300)", owner],
                                    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                    start_new_session=True)
        record["sentinel"] = process(sentinel.pid)
        host_ns = namespace(sentinel.pid, "pid")
        assert host_ns == namespace(os.getpid(), "pid")
        record["host_pid_namespace"] = host_ns

        control = create()
        private_pid = inspect(control)["State"]["Pid"]
        private_ns = namespace(private_pid, "pid")
        assert private_ns != host_ns
        assert inspect(control)["HostConfig"]["PidMode"] == ""
        assert execute(control, "readlink", "/proc/1/ns/pid") == private_ns
        assert execute(control, "cat", "/proc/1/comm") == "sleep"
        # An exec joins this private namespace before a host-mode start.
        assert execute(control, "readlink", "/proc/self/ns/pid") == private_ns
        # Numeric PIDs may collide across namespaces; any such process is
        # still in this private namespace, not the host sentinel's.
        assert execute(control, "sh", "-ec",
                       'if [ -e "/proc/$1/ns/pid" ]; then readlink "/proc/$1/ns/pid"; fi',
                       "probe", str(sentinel.pid)) in ("", private_ns)
        private_mounts(private_pid)
        passed("default PID isolation hides the host sentinel and preserves PID 1")

        shared = create(("--pid", "host", "--init"), ("sh", "-ec", ORPHAN_WORKLOAD))
        info = inspect(shared)
        shared_pid = info["State"]["Pid"]
        assert info["HostConfig"]["PidMode"] == "host"
        assert namespace(shared_pid, "pid") == host_ns
        assert execute(shared, "readlink", "/proc/self/ns/pid") == host_ns
        assert execute(shared, "cat", f"/proc/{sentinel.pid}/cmdline") == Path(f"/proc/{sentinel.pid}/cmdline").read_text().strip()
        assert sorted(int(fd) for fd in execute(shared, "ls", "-1", f"/proc/{shared_pid}/fd").split()) == [0, 1, 2]
        # A host PID container sees the other container's host PID, while
        # that container retains its own PID namespace and PID 1.
        assert execute(shared, "readlink", f"/proc/{private_pid}/ns/pid") == private_ns
        private_mounts(shared_pid)
        assert execute(shared, "sh", "-ec",
                       "if touch /root-write 2>/dev/null; then exit 1; fi; "
                       "cat /sys/fs/cgroup/pids.max /sys/fs/cgroup/memory.max /sys/fs/cgroup/cpu.max"
                       ).splitlines() == ["32", "67108864", "50000 100000"]
        cli("update", "--pids-limit", "16", shared)
        assert inspect(shared)["HostConfig"]["PidsLimit"] == 16
        assert execute(shared, "cat", "/sys/fs/cgroup/pids.max") == "16"
        passed("host PID visibility keeps private mounts, other namespaces, and container resource limits")

        adopted = orphan(shared)
        until(lambda: (process(adopted["pid"]) or {}).get("parent") == shared_pid,
              "--init did not adopt its orphan in the host PID namespace")
        execute(shared, "touch", "/tmp/release-orphan")
        until(lambda: gone(adopted), "--init left an exited orphan unreaped")
        cli("stop", "--time", "3", shared)
        stopped = inspect(shared)
        assert not stopped["State"]["Running"] and stopped["State"]["ExitCode"] == 143, stopped["State"]
        passed("host-mode init adopts and reaps orphans and forwards SIGTERM")

        cli("start", shared)
        restarted = inspect(shared)
        watch(restarted["State"]["Pid"])
        assert restarted["HostConfig"]["PidMode"] == "host"
        assert namespace(restarted["State"]["Pid"], "pid") == host_ns
        assert execute(shared, "readlink", "/proc/self/ns/pid") == host_ns
        assert execute(control, "readlink", "/proc/self/ns/pid") == private_ns
        passed("restart and alternating host/private exec preserve PID namespace selection")

        # Exercise the standard host-observer option combination with a tiny
        # inert workload. This does not attach tracing programs or read services.
        observer_options = ["--pid", "host", "--cap-drop", "ALL",
                            "--security-opt", "seccomp=unconfined", "--security-opt", "apparmor=unconfined"]
        for capability in ("BPF", "SYS_PTRACE", "NET_RAW", "CHECKPOINT_RESTORE", "DAC_READ_SEARCH",
                           "PERFMON", "NET_ADMIN", "SYS_ADMIN", "SYS_CHROOT", "SETUID", "SETGID", "DAC_OVERRIDE"):
            observer_options += ["--cap-add", capability]
        unsupervised = create(observer_options, ("sh", "-ec", ORPHAN_WORKLOAD), network="host")
        initial = inspect(unsupervised)["State"]["Pid"]
        agent = process(initial)["parent"]
        adopted_without_init = orphan(unsupervised)
        until(lambda: (process(adopted_without_init["pid"]) or {}).get("parent") == agent,
              "the agent did not adopt the host-mode orphan")
        assert namespace(initial, "pid") == host_ns
        private_mounts(initial, host_network=True)
        # This orphan still holds the output pipes. Stop must clean its
        # cgroup before waiting for EOF, while retaining already queued output.
        cli("stop", "--time", "3", unsupervised)
        assert inspect(unsupervised)["State"]["ExitCode"] == 143
        until(lambda: gone(adopted_without_init), "stop left a host-mode descendant holding output open")
        assert cli("logs", unsupervised) == "host-pid-output"
        cli("start", unsupervised)
        watch(inspect(unsupervised)["State"]["Pid"])
        adopted_after_restart = orphan(unsupervised)
        until(lambda: (process(adopted_after_restart["pid"]) or {}).get("parent") == agent,
              "the restarted orphan was not adopted")
        remove(unsupervised)
        until(lambda: gone(adopted_after_restart), "forced removal left a host-mode descendant")
        assert inspect(control)["State"]["Running"]
        assert inspect(shared)["State"]["Running"]
        passed("stop and forced host-mode removal drain output, reap only its cgroup, and preserve unrelated processes")
        after_host = create()
        after_host_pid = inspect(after_host)["State"]["Pid"]
        assert namespace(after_host_pid, "pid") not in (host_ns, private_ns)
        assert execute(after_host, "cat", "/proc/1/comm") == "sleep"
        assert execute(after_host, "readlink", "/proc/self/ns/pid") == namespace(after_host_pid, "pid")
        passed("a later private start creates a fresh PID namespace after host starts and execs")

        # Start the allocator only after recording the positive PID. The
        # existing 64 MiB container budget, not host memory pressure, kills it.
        oom = create(("--pid", "host"), ("sh", "-ec",
                     "while [ ! -e /tmp/oom-go ]; do sleep 0.05; done; exec tail /dev/zero"))
        execute(oom, "touch", "/tmp/oom-go")
        assert cli("wait", oom) == "137"
        assert inspect(oom)["State"]["OOMKilled"] is True
        remove(oom)
        passed("host-mode cleanup retains the real cgroup OOM result")
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error))
        raise
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for cid in list(reversed(owned)):
            try:
                remove(cid)
            except BaseException as error:
                errors.append(repr(error))
        for info in watched:
            try:
                until(lambda info=info: gone(info), f"process {info['pid']} was not reaped")
            except BaseException as error:
                errors.append(repr(error))
        if sentinel is not None:
            try:
                sentinel_alive()
                record["sentinel_survived_cleanup"] = True
            except BaseException as error:
                errors.append(repr(error))
            finally:
                if sentinel.poll() is None:
                    sentinel.terminate()
                try:
                    sentinel.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    sentinel.kill()
                    sentinel.wait(timeout=5)
        for number, handler in previous.items():
            signal.signal(number, handler)
        record["cleanup_errors"] = errors
        record["finished_at"] = datetime.now(timezone.utc).isoformat()
        if errors:
            record["status"] = "failed"
        if args.output:
            args.output.write_text(json.dumps(record, indent=2) + "\n")
        print(json.dumps(record, sort_keys=True), flush=True)
        if errors and not primary_failure:
            raise AssertionError(errors)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image")
    parser.add_argument("--output", type=Path)
    check(parser.parse_args())
