#!/usr/bin/env python3
"""Require full Docker load HTTP-body completion before an image is published."""
import bz2
import gzip
import http.client
import io
import json
import os
import select
import socket
import subprocess
import sys
import tarfile
import uuid


def check(path):
    assert os.environ.get('ASAGIRI_TEST_ENGINE') == '1', 'Use a disposable engine only'
    names = []

    def archive():
        name = 'asagiri-load-completion-' + uuid.uuid4().hex + ':test'
        names.append(name)
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode='w') as tar:
            for member, value in [('config.json', {'os': 'linux', 'architecture': 'amd64',
                    'config': {'Env': ['CASE=' + name]}, 'rootfs': {'type': 'layers', 'diff_ids': []}}),
                    ('manifest.json', [{'Config': 'config.json', 'RepoTags': [name], 'Layers': []}])]:
                data = json.dumps(value).encode()
                info = tarfile.TarInfo(member)
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
        return name, output.getvalue()

    def connect(head):
        stream = socket.socket(socket.AF_UNIX)
        stream.settimeout(10)
        stream.connect(path)
        stream.sendall(b'POST /v1.47/images/load HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n' + head + b'\r\n')
        return stream

    def response(stream):
        reply = http.client.HTTPResponse(stream)
        reply.begin()
        return reply.status, reply.read()

    def inspect(name):
        return subprocess.run(['docker', '-H', 'unix://' + path, 'image', 'inspect', name],
                              capture_output=True, timeout=15)

    try:
        for chunked in (False, True):
            name, data = archive()
            head = b'Transfer-Encoding: chunked\r\n' if chunked else b'Content-Length: ' + str(len(data)+1024).encode() + b'\r\n'
            with connect(head) as stream:
                stream.sendall((hex(len(data))[2:].encode()+b'\r\n'+data+b'\r\n') if chunked else data)
                stream.shutdown(socket.SHUT_WR)  # Missing declared padding or terminal chunk.
                status, body = response(stream)
                assert status == 400, (chunked, status, body)
                assert inspect(name).returncode != 0, 'Incomplete HTTP upload published an image'
            print('PASS truncated ' + ('chunked' if chunked else 'content-length') + ' load publishes no image')
        name, data = archive()
        padding = bytes(32768)
        with connect(b'Content-Length: '+str(len(data)+len(padding)).encode()+b'\r\n') as stream:
            stream.sendall(data)
            # Protocol check: no success response while advertised bytes remain unsent.
            assert not select.select([stream], [], [], 0.2)[0], 'Load replied before the whole body arrived'
            assert inspect(name).returncode != 0, 'Load published before the whole body arrived'
            stream.sendall(padding)
            status, body = response(stream)
            assert status == 200, (status, body)
            assert inspect(name).returncode == 0
        print('PASS padded load waits for full body, then publishes image')
        for case, encode, expected in (
            ('gzip', gzip.compress, 200),
            ('multiple gzip members', lambda data: gzip.compress(data[:777]) + gzip.compress(data[777:]), 200),
            ('damaged gzip checksum', lambda data: gzip.compress(data)[:-8] + bytes(8), 400),
            ('truncated gzip trailer', lambda data: gzip.compress(data)[:-3], 400),
            ('unsupported bzip2', bz2.compress, 400),
        ):
            name, data = archive()
            payload = encode(data)
            with connect(b'Content-Length: ' + str(len(payload)).encode() + b'\r\n') as stream:
                stream.sendall(payload)
                status, body = response(stream)
            assert status == expected, (case, status, body)
            assert (inspect(name).returncode == 0) == (expected == 200), case
            print('PASS', case, 'load publication matches response status')
    finally:
        for name in names:
            subprocess.run(['docker', '-H', 'unix://' + path, 'image', 'rm', name],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)


if __name__ == '__main__':
    check(sys.argv[1])
