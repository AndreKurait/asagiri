"""An --init supervisor retains stdio, not the agent's sockets or handles."""
import os
import subprocess
import sys

assert os.environ.get('ASAGIRI_TEST_ENGINE') == '1'
socket, image = sys.argv[1:]
docker = ['docker', '-H', 'unix://' + socket]


def call(*args):
    return subprocess.check_output(docker + list(args), timeout=30, stderr=subprocess.STDOUT)


keeper = call('run', '-d', image, 'sleep', '120').decode().strip()
try:
    # A live unrelated container gives the agent pipes/network handles that
    # a forked supervisor must not keep alive.
    for options in [[], ['-t'], ['--user', '1000:1000']]:
        cid = call('run', '-d', '--init', *options, image, 'sleep', '120').decode().strip()
        try:
            descriptors = call('exec', '--user', '0', cid, 'sh', '-c', 'ls -1 /proc/1/fd').split()
            assert sorted(int(fd) for fd in descriptors) == [0, 1, 2], (
                'init retained non-stdio descriptors', options, len(descriptors))
            assert call('exec', keeper, 'sh', '-c', 'printf still-running') == b'still-running'
            call('stop', '-t', '2', cid)
            assert call('inspect', '-f', '{{.State.ExitCode}}', cid).strip() == b'143'
            print('PASS init stdio-only descriptors, unrelated I/O and SIGTERM', options, flush=True)
        finally:
            call('rm', '-f', cid)
finally:
    call('rm', '-f', keeper)
