#!/usr/bin/env python3
"""Embedded DNS must leave wildcard port 53 available to container DNS servers."""
import contextlib
import ctypes
import json
import os
import socket
import struct
import subprocess
import sys
import uuid


@contextlib.contextmanager
def namespace(pid):
    libc = ctypes.CDLL(None, use_errno=True)
    own = os.open('/proc/self/ns/net', os.O_RDONLY)
    target = os.open('/proc/' + str(pid) + '/ns/net', os.O_RDONLY)
    try:
        if libc.setns(target, 0x40000000):
            raise OSError(ctypes.get_errno(), 'enter netns')
        yield
    finally:
        if libc.setns(own, 0x40000000):
            raise OSError(ctypes.get_errno(), 'restore netns')
        os.close(target)
        os.close(own)


def check(path, image):
    assert os.environ.get('ASAGIRI_TEST_ENGINE') == '1', 'Use a disposable engine only'
    docker = ['docker', '-H', 'unix://' + path]
    def call(*args):
        return subprocess.check_output(docker + list(args), text=True, timeout=30).strip()
    suffix = uuid.uuid4().hex[:12]
    network = 'dns-port-' + suffix
    containers = []
    call('network', 'create', network)
    try:
        peer = 'dns-peer-' + suffix
        for name in (peer, 'dns-server-' + suffix):
            cid = call('run', '-d', '--name', name, '--network', network, image, 'sleep', '120')
            containers.append(cid)
        rows = [json.loads(call('inspect', cid))[0] for cid in containers]
        expected = rows[0]['NetworkSettings']['Networks'][network]['IPAddress']
        server_ip = rows[1]['NetworkSettings']['Networks'][network]['IPAddress']
        with namespace(rows[1]['State']['Pid']):
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as application, socket.socket() as tcp:
                application.bind(('0.0.0.0', 53))  # Private container namespace, no published port.
                application.settimeout(3)
                tcp.bind(('0.0.0.0', 53))
                tcp.listen(1)
                with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as query:
                    query.settimeout(3)
                    wire = struct.pack('!6H', 0x1234, 0x0100, 1, 0, 0, 0)
                    wire += b''.join(bytes([len(part)]) + part.encode() for part in peer.split('.')) + b'\0\0\1\0\1'
                    query.sendto(wire, ('127.0.0.11', 53))
                    response, source = query.recvfrom(4096)
                    assert source == ('127.0.0.11', 53), source
                    assert response[:2] == b'\x12\x34' and socket.inet_aton(expected) in response, response
                    # The application remains the listener at ordinary port 53.
                    query.sendto(b'application', (server_ip, 53))
                    data, client = application.recvfrom(4096)
                    assert data == b'application', data
                    application.sendto(b'application-reply', client)
                    response, source = query.recvfrom(4096)
                    assert response == b'application-reply' and source == (server_ip, 53), (response, source)
                with socket.create_connection((server_ip, 53), timeout=3) as client:
                    accepted, _ = tcp.accept()
                    with accepted:
                        client.sendall(b'tcp-query')
                        assert accepted.recv(64) == b'tcp-query'
                        accepted.sendall(b'tcp-reply')
                        assert client.recv(64) == b'tcp-reply'
                print('PASS wildcard UDP/TCP port53 and embedded alias DNS coexist; replies retain source port53')
    finally:
        for cid in containers:
            subprocess.run(docker + ['rm', '-fv', cid], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        call('network', 'rm', network)


if __name__ == '__main__':
    check(sys.argv[1], sys.argv[2])
