#!/usr/bin/env python3
"""Archives see stopped-container volumes and binds without starting the process."""
import io
import json
import os
import pathlib
import subprocess
import sys
import tarfile
import tempfile
import uuid


def check(path, image):
    assert os.environ.get('ASAGIRI_TEST_ENGINE') == '1', 'Use a disposable engine only'
    docker = ['docker', '-H', 'unix://' + path]
    def call(*args, input=None):
        return subprocess.check_output(docker + list(args), input=input, timeout=30)
    suffix = uuid.uuid4().hex[:12]
    volume = 'archive-volume-' + suffix
    containers = []
    call('volume', 'create', volume)
    try:
        with tempfile.TemporaryDirectory(prefix='archive-bind-') as source:
            payload = b'nameserver 127.0.0.11\n'
            local = pathlib.Path(source) / 'input'
            local.write_bytes(payload)
            for kind, mount in [('volume', volume + ':/new-mount'), ('bind', source + ':/new-mount')]:
                cid = call('create', '--network', 'none', '-v', mount, image, 'sleep', '120').decode().strip()
                containers.append(cid)
                call('cp', str(local), cid + ':/new-mount/resolv.conf')
                row = json.loads(call('inspect', cid))[0]
                assert row['State']['Status'] == 'created' and not row['State']['Running'], row['State']
                archive = call('cp', cid + ':/new-mount/resolv.conf', '-')
                with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
                    assert tar.extractfile(tar.getmembers()[0]).read() == payload
                exported = call('export', cid)
                with tarfile.open(fileobj=io.BytesIO(exported)) as tar:
                    assert not any(m.name.lstrip('./') == 'new-mount/resolv.conf' for m in tar.getmembers())
                call('start', cid)
                assert call('exec', cid, 'cat', '/new-mount/resolv.conf') == payload
                call('stop', '-t', '0', cid)
                assert call('cp', cid + ':/new-mount/resolv.conf', '-')
                readonly = call('create', '--network', 'none', '-v', mount + ':ro', image, 'sleep', '120').decode().strip()
                containers.append(readonly)
                assert call('cp', readonly + ':/new-mount/resolv.conf', '-')
                denied = subprocess.run(docker + ['cp', str(local), readonly + ':/new-mount/denied'], capture_output=True, timeout=30)
                assert denied.returncode != 0, denied.stdout
                print('PASS stopped ' + kind + ' copy-in/out, runtime persistence, export exclusion, and read-only enforcement')
            # #245: Supabase checks an existing database volume through a new
            # container before its process has ever started.
            call('run', '--rm', '--network', 'none', '-v', volume + ':/data', image,
                 'sh', '-c', 'printf "17\\n" > /data/PG_VERSION')
            cid = call('create', '--read-only', '--network', 'none', '-v', volume + ':/data', image, 'true').decode().strip()
            containers.append(cid)
            for phase in ('created', 'exited'):
                if phase == 'exited':
                    call('start', '-a', cid)
                row = json.loads(call('inspect', cid))[0]
                assert row['State']['Status'] == phase and not row['State']['Running'], row['State']
                marker = 'probe-' + phase
                root_file = cid + ':/etc/' + marker
                denied = subprocess.run(docker + ['cp', str(local), root_file], capture_output=True, timeout=30)
                assert denied.returncode != 0, 'copy unexpectedly wrote to the read-only root'
                missing = subprocess.run(docker + ['cp', root_file, '-'], capture_output=True, timeout=30)
                assert missing.returncode != 0, 'failed upload left a file on the read-only root'
                volume_file = cid + ':/data/' + marker
                call('cp', str(local), volume_file)
                with tarfile.open(fileobj=io.BytesIO(call('cp', volume_file, '-'))) as tar:
                    assert tar.extractfile(tar.getmembers()[0]).read() == payload
                archive = call('cp', cid + ':/data/PG_VERSION', '-')
                with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
                    assert tar.extractfile(tar.getmembers()[0]).read() == b'17\n'
                print('PASS existing volume PG_VERSION from a ' + phase + ' container (#245)')
    finally:
        for cid in containers:
            subprocess.run(docker + ['rm', '-fv', cid], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        call('volume', 'rm', volume)


if __name__ == '__main__':
    check(sys.argv[1], sys.argv[2])
