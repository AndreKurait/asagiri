"""Check actual daemon access from a UID absent from the host account database."""
import grp
import os
import pwd
import socket
import stat
import sys

variant = sys.argv[1]
path = sys.argv[2] if len(sys.argv) > 2 else "/run/kiri/docker.sock"
# Only the service socket lets a group in without one being named.
open_to_group = variant != "custom-path"
metadata = os.stat(path)
assert stat.S_IMODE(os.stat(os.path.dirname(path)).st_mode) == 0o755
assert stat.S_IMODE(metadata.st_mode) == (0o660 if open_to_group else 0o600), oct(metadata.st_mode)
assert metadata.st_uid == 0
expected_gid = grp.getgrnam("docker").gr_gid if variant == "docker-group" else 0
assert metadata.st_gid == expected_gid, (metadata.st_gid, expected_gid)
uid = 424242
try:
    pwd.getpwuid(uid)
except KeyError:
    pass
else:
    raise AssertionError("probe UID must not have a host account")


def probe(groups, expected):
    pid = os.fork()
    if pid == 0:
        try:
            os.setgroups(groups)
            os.setgid(uid)
            os.setuid(uid)
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(5)
                client.connect(path)
                client.sendall(b"GET /_ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                response = client.recv(4096)
                allowed = b"200 OK" in response
        except (PermissionError, ConnectionResetError, BrokenPipeError):
            allowed = False
        except BaseException as error:
            print(repr(error), flush=True)
            os._exit(2)
        os._exit(0 if allowed == expected else 1)
    _, status = os.waitpid(pid, 0)
    assert os.waitstatus_to_exitcode(status) == 0, (variant, groups, expected, status)


probe([expected_gid], variant in ("docker-group", "fallback", "fallback-gid"))
if variant == "fallback-gid":
    # The group the daemon ran with, which the socket was made with.
    probe([int(sys.argv[3])], False)
if variant == "custom-path":
    probe([grp.getgrnam("docker").gr_gid], False)
probe([], False)
print(f"PASS {variant}: socket ownership, group-only container UID access, unrelated UID rejection")
