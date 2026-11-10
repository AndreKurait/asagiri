#!/usr/bin/env python3
"""Check seccomp security options (default, unconfined, custom profiles) through the Engine API."""
import http.client
import json
import socket
import sys


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


# What a shell needs to start under either engine and run the probes below.
# chmod and its variants are left out on purpose.
ALLOWED = """
accept accept4 access arch_prctl bind brk capget capset chdir chown clock_getres clock_gettime clock_nanosleep
clone clone3 close close_range connect dup dup2 dup3 epoll_create1 epoll_ctl epoll_pwait epoll_wait eventfd2
execve execveat exit exit_group faccessat faccessat2 fchdir fchown fchownat fcntl fstat fstatfs fsync futex
getcwd getdents64 getegid geteuid getgid getgroups getpgid getpgrp getpid getppid getrandom getrlimit
getsockname getsockopt gettid gettimeofday getuid ioctl kill lseek lstat madvise membarrier mkdir mkdirat mmap
mprotect mremap munmap nanosleep newfstatat open openat pipe pipe2 poll ppoll prctl pread64 prlimit64 pselect6
read readlink readlinkat readv restart_syscall rseq rt_sigaction rt_sigprocmask rt_sigreturn rt_sigsuspend
rt_sigtimedwait sched_getaffinity sched_yield select set_robust_list set_tid_address setgid setgroups
setitimer setpgid setresgid setresuid setsid setuid sigaltstack socket stat statfs statx sysinfo tgkill tkill
umask uname unlink unlinkat utimensat wait4 waitid write writev
""".split()

# chmod is `chmod(path, mode)` or `fchmodat(dirfd, path, mode)` by architecture.
CHMOD = ["chmod", "fchmodat", "fchmodat2"]


def profile(*rules, default="SCMP_ACT_ALLOW", **fields):
    return "seccomp=" + json.dumps({"defaultAction": default, "syscalls": list(rules), **fields})


def deny_chmod(**fields):
    return {"names": CHMOD, "action": "SCMP_ACT_ERRNO", **fields}


def frames(data):
    """The text of a multiplexed (non-TTY) output stream."""
    text, at = b"", 0
    while at + 8 <= len(data):
        size = int.from_bytes(data[at + 4:at + 8], "big")
        text += data[at + 8:at + 8 + size]
        at += 8 + size
    return text.decode()


def check(path, image):
    owned = []

    def request(method, target, body=None, statuses=(200,), raw=False):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            if target == "/containers/create" and response.status == 201:
                owned.append(json.loads(data)["Id"])
            assert response.status in statuses, (method, target, response.status, data)
            return data if raw else json.loads(data) if data else None
        finally:
            connection.close()

    def create(options, command, **host):
        return request("POST", "/containers/create",
                       {"Image": image, "Cmd": ["sh", "-c", command],
                        "HostConfig": {"NetworkMode": "none", "SecurityOpt": options, **host}}, (201,))["Id"]

    def run(options, command, **host):
        """Runs a shell command to completion: (exit status, output)."""
        cid = create(options, command, **host)
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        status = request("POST", "/containers/" + cid + "/wait")["StatusCode"]
        output = frames(request("GET", "/containers/" + cid + "/logs?stdout=1&stderr=1", raw=True))
        request("DELETE", "/containers/" + cid + "?force=true", statuses=(204,))
        return status, output

    def refused(options):
        """A profile the engine cannot apply: accepted at create, an error at start."""
        cid = create(options, "true")
        message = request("POST", "/containers/" + cid + "/start", statuses=(500,))["message"]
        state = request("GET", "/containers/" + cid + "/json")["State"]
        assert not state["Running"], state
        request("DELETE", "/containers/" + cid + "?force=true", statuses=(204,))
        return message

    def attached(eid):
        """The output of an exec, read from the connection the engine hands over."""
        body = json.dumps({"Detach": False}).encode()
        head = ("POST /v1.47/exec/" + eid + "/start HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n"
                "Connection: Upgrade\r\nUpgrade: tcp\r\nContent-Length: " + str(len(body)) + "\r\n\r\n").encode()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(60)
            stream.connect(path)
            stream.sendall(head + body)
            data = b""
            while chunk := stream.recv(65536):
                data += chunk
        head, _, output = data.partition(b"\r\n\r\n")
        assert head.startswith(b"HTTP/1.1 101"), data
        return frames(output)

    # Creating a user namespace is something the default profile refuses to a
    # container without CAP_SYS_ADMIN; chmod is something it allows.
    probe = "touch /f; chmod 600 /f; echo chmod=$?; unshare -U true; echo unshare=$?"
    try:
        for options in ([], ["seccomp=builtin"], ["seccomp=unconfined", "seccomp=builtin"]):
            status, output = run(options, probe)
            assert status == 0 and "chmod=0" in output and "unshare=1" in output, (options, output)
            assert "not permitted" in output, output
        print("PASS the default profile refuses a new user namespace", flush=True)

        for options in (["seccomp=unconfined"], ["seccomp:unconfined"], ["seccomp={}"],
                        [profile(deny_chmod()), "seccomp=unconfined"]):
            status, output = run(options, probe)
            assert status == 0 and "chmod=0" in output and "unshare=0" in output, (options, output)
        print("PASS unconfined and an empty profile lift it", flush=True)

        # A custom profile replaces the default one, for a privileged container too.
        status, output = run([profile(deny_chmod())], probe)
        assert "chmod=1" in output and "not permitted" in output and "unshare=0" in output, output
        status, output = run([profile(deny_chmod(errnoRet=13))], probe)
        assert "chmod=1" in output and "Permission denied" in output, output
        status, output = run([profile({"name": CHMOD[1], "action": "SCMP_ACT_ERRNO", "errnoRet": 13},
                                      {"names": ["chmod", "no_such_syscall"], "action": "SCMP_ACT_ERRNO", "errnoRet": 13})],
                             probe)
        assert "chmod=1" in output and "Permission denied" in output, output
        for options, host in ((["seccomp=unconfined", profile(deny_chmod(errnoRet=13))], {}),
                              ([profile(deny_chmod(errnoRet=13))], {"Privileged": True}),
                              (["no-new-privileges", profile(deny_chmod(errnoRet=13))], {})):
            status, output = run(options, probe, **host)
            assert "chmod=1" in output and "Permission denied" in output, (options, host, output)
        # A profile's keys are read in any case.
        capitals = {"DefaultAction": "SCMP_ACT_ALLOW", "Syscalls": [{"Names": CHMOD, "Action": "SCMP_ACT_ERRNO", "ErrnoRet": 13}]}
        status, output = run(["seccomp=" + json.dumps(capitals)], probe)
        assert "chmod=1" in output and "Permission denied" in output and "unshare=0" in output, output
        # An errno of 0 makes the call report success without running.
        status, output = run([profile(deny_chmod(errnoRet=0))], "touch /f; chmod 600 /f; echo chmod=$?; stat -c %a /f")
        assert output.split() == ["chmod=0", "644"], output
        print("PASS a custom profile denies a syscall with its errno", flush=True)

        allow = {"names": ALLOWED, "action": "SCMP_ACT_ALLOW"}
        status, output = run([profile(allow, default="SCMP_ACT_ERRNO", defaultErrnoRet=13)],
                             "mkdir /d && echo made; touch /f; chmod 600 /f; echo chmod=$?")
        assert status == 0 and "made" in output and "chmod=1" in output and "Permission denied" in output, output
        print("PASS an allow list runs a shell and denies the rest", flush=True)

        rules = []
        for name, argument in (("chmod", 1), ("fchmodat", 2)):
            rules.append({"names": [name], "action": "SCMP_ACT_ERRNO", "errnoRet": 13,
                          "args": [{"index": argument, "op": "SCMP_CMP_EQ", "value": 0o700}]})
            rules.append({"names": [name], "action": "SCMP_ACT_ERRNO",
                          "args": [{"index": argument, "op": "SCMP_CMP_MASKED_EQ", "value": 0o002, "valueTwo": 0o002}]})
        status, output = run([profile(*rules)],
                             "touch /f; chmod 600 /f; echo plain=$?; chmod 700 /f; echo equal=$?; chmod 606 /f; echo masked=$?")
        assert "plain=0" in output and "equal=1" in output and "masked=1" in output, output
        assert "Permission denied" in output and "not permitted" in output, output

        # An argument is 64 bits wide and compared as a whole: the length
        # ftruncate is given, on both sides of 2^32.
        lengths = (0xffffffff, 0x100000004, 0x100000005, 0x100000016, 0x200000005, 0x200000006)
        for op, value, more, denied in (("SCMP_CMP_LT", 0x100000005, {}, lengths[:2]),
                                        ("SCMP_CMP_GT", 0x200000005, {}, lengths[5:]),
                                        ("SCMP_CMP_MASKED_EQ", 0xff000000f0, {"valueTwo": 0x100000010}, lengths[3:4])):
            rule = {"names": ["ftruncate"], "action": "SCMP_ACT_ERRNO", "errnoRet": 28,
                    "args": [{"index": 1, "op": op, "value": value, **more}]}
            status, output = run([profile(rule)], "; ".join("truncate -s %d /f 2>&1; echo =%x" % (size, size) for size in lengths))
            for size in lengths:
                said, _, output = output.partition("=%x\n" % size)
                assert ("No space left" in said) if size in denied else not said, (op, hex(size), said)
        print("PASS rules conditional on syscall arguments", flush=True)

        conditional = profile(deny_chmod(includes={"caps": ["CAP_SYS_ADMIN"]}),
                              deny_chmod(includes={"minKernel": "99.0"}),
                              deny_chmod(includes={"arches": ["s390x"]}),
                              deny_chmod(excludes={"caps": ["CAP_CHOWN"]}))
        status, output = run([conditional], probe)
        assert "chmod=0" in output, output
        status, output = run([conditional], probe, CapAdd=["SYS_ADMIN"])
        assert "chmod=1" in output, output
        print("PASS rules conditional on capabilities, kernel and architecture", flush=True)

        message = refused(['seccomp={"defaultAction":'])
        assert message.startswith("Decoding seccomp profile failed"), message
        assert refused(["seccomp=[]"]).startswith("Decoding seccomp profile failed")
        for options in ([profile(default="SCMP_ACT_BOGUS")],
                        [profile({"names": CHMOD, "action": "SCMP_ACT_BOGUS"})],
                        [profile({"names": CHMOD})],
                        ["seccomp=" + json.dumps({"syscalls": [deny_chmod()]})]):
            message = refused(options)
            assert "not a valid action for seccomp" in message, (options, message)
        for options, reason in (
                ([profile(architectures=["SCMP_ARCH_BOGUS"])], "not a valid arch for seccomp"),
                ([profile(architectures=["SCMP_ARCH_AARCH64"], archMap=[{"architecture": "SCMP_ARCH_AARCH64"}])], "use either 'architectures' or 'archMap'"),
                ([profile({"name": "chmod", "names": ["fchmodat"], "action": "SCMP_ACT_ERRNO"})], "use either 'name' or 'names'"),
                ([profile({"names": [""], "action": "SCMP_ACT_ERRNO"})], "empty string is not a valid syscall"),
                ([profile(deny_chmod(args=[{"index": 6, "op": "SCMP_CMP_EQ", "value": 0}]))], "up to 6 arguments"),
                ([profile(deny_chmod(args=[{"index": 0, "op": "SCMP_CMP_BOGUS", "value": 0}]))], "not a valid operator for seccomp"),
                ([profile(deny_chmod(includes={"minKernel": "bogus"}))], "invalid kernel version"),
                ([profile(flags=["SECCOMP_FILTER_FLAG_BOGUS"])], "SECCOMP_FILTER_FLAG_BOGUS is not known"),
                ([profile({"names": ["fchmodat"], "action": "SCMP_ACT_NOTIFY"})], "listener")):
            message = refused(options)
            assert reason in message, (options, message)
        print("PASS invalid profiles fail the start with their reason", flush=True)

        options = ["no-new-privileges", profile(deny_chmod(errnoRet=13), flags=["SECCOMP_FILTER_FLAG_LOG"], listenerPath="/run/unused.sock")]
        cid = create(options, "touch /f; exec sleep 120")
        assert request("GET", "/containers/" + cid + "/json")["HostConfig"]["SecurityOpt"] == options
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        for privileged in (False, True):
            eid = request("POST", "/containers/" + cid + "/exec",
                          {"Cmd": ["sh", "-c", "touch /g; chmod 600 /g; echo chmod=$?"], "Privileged": privileged,
                           "AttachStdout": True, "AttachStderr": True}, (201,))["Id"]
            output = attached(eid)
            assert "chmod=1" in output and "Permission denied" in output, (privileged, output)
            assert request("GET", "/exec/" + eid + "/json")["ExitCode"] == 0
        print("PASS inspect returns the options and exec inherits the profile", flush=True)
        print("8 seccomp scenarios passed")
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for cid in reversed(owned):
            try:
                request("DELETE", "/containers/" + cid + "?force=true", statuses=(204, 404))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not primary_failure:
            raise RuntimeError("Could not remove all seccomp-test containers") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
