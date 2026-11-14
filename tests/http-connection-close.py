#!/usr/bin/env python3
"""Clients must not reuse a connection a one-request HTTP proxy closes."""
import http.client
import os
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import uuid


def check(upstream):
    assert os.environ.get('ASAGIRI_TEST_ENGINE') == '1', 'Use a disposable engine only'

    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(5)
        stream.connect(upstream)
        for _ in range(3):
            stream.sendall(b'HEAD /_ping HTTP/1.1\r\nHost: docker\r\nConnection: keep-alive\r\n\r\n')
            response = http.client.HTTPResponse(stream, method='HEAD')
            response.begin()
            assert response.status == 200 and not response.will_close
            response.read()
            response.close()
        stream.sendall(b'HEAD /_ping HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n')
        response = http.client.HTTPResponse(stream, method='HEAD')
        response.begin()
        assert response.status == 200
        response.read()
        response.close()
        assert stream.recv(1) == b'', 'Connection: close was not honored'
    print('PASS direct keepalive serves successive requests then closes')

    class Forward(socketserver.BaseRequestHandler):
        def handle(self):
            try:
                data = b''
                while b'\r\n\r\n' not in data:
                    block = self.request.recv(8192)
                    if not block:
                        return
                    data += block
                head, body = data.split(b'\r\n\r\n', 1)
                lines = [line for line in head.split(b'\r\n') if not line.lower().startswith(b'connection:')]
                with socket.socket(socket.AF_UNIX) as remote:
                    remote.connect(upstream)
                    remote.sendall(b'\r\n'.join(lines) + b'\r\nConnection: close\r\n\r\n' + body)
                    while True:
                        block = remote.recv(65536)
                        if not block:
                            # Response delivery can precede propagation of the
                            # upstream EOF. A client must already know to close.
                            time.sleep(0.02)
                            return
                        self.request.sendall(block)
            except (BrokenPipeError, ConnectionResetError):
                pass

    class Server(socketserver.ThreadingUnixStreamServer):
        daemon_threads = True

    with tempfile.TemporaryDirectory(prefix='asagiri-close-') as directory:
        path = directory + '/docker.sock'
        with Server(path, Forward) as proxy:
            thread = threading.Thread(target=proxy.serve_forever, daemon=True)
            thread.start()
            try:
                failures = []
                for _ in range(20):
                    result = subprocess.run(['docker', '-H', 'unix://' + path, 'start',
                                             'asagiri-nonexistent-' + uuid.uuid4().hex],
                                            capture_output=True, text=True, timeout=15)
                    if result.returncode == 0 or 'No such container' not in result.stderr:
                        failures.append(result.stderr)
                print('CLI transport failures:', len(failures), failures[:2], flush=True)
                with socket.socket(socket.AF_UNIX) as stream:
                    stream.connect(path)
                    stream.sendall(b'HEAD /_ping HTTP/1.1\r\nHost: docker\r\n\r\n')
                    response = http.client.HTTPResponse(stream, method='HEAD')
                    response.begin()
                    assert response.status == 200
                    assert response.will_close, 'Response advertised reuse although the engine closes the connection'
                assert not failures, failures
                print('PASS close response prevents CLI reuse through a one-request proxy')
            finally:
                proxy.shutdown()
                thread.join()


if __name__ == '__main__':
    check(sys.argv[1])
