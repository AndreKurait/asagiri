#!/usr/bin/env python3
"""Check immediate exec observes completed container mount setup on a disposable engine."""
import http.client
import json
import os
import socket
import sys
import time
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__('localhost', timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image):
    if os.environ.get('ASAGIRI_TEST_ENGINE') != '1':
        raise RuntimeError('Run only inside a disposable engine harness')
    owned = []

    def request(method, target, body=None, status=200):
        connection = Connection(path)
        try:
            connection.request(method, '/v1.47' + target,
                               None if body is None else json.dumps(body),
                               {'Content-Type': 'application/json'})
            response = connection.getresponse()
            data = response.read()
            if target == '/containers/create' and response.status == 201:
                owned.append(json.loads(data)['Id'])
            assert response.status == status, (method, target, response.status, data)
            return json.loads(data) if data else None
        finally:
            connection.close()

    try:
        for _ in range(20):
            hostname = 'ready-' + uuid.uuid4().hex
            cid = request('POST', '/containers/create', {
                'Image': image, 'Cmd': ['sleep', '120'], 'Hostname': hostname,
                'Env': ['EXPECTED_HOSTNAME=' + hostname],
                'HostConfig': {'NetworkMode': 'none',
                               'Tmpfs': {'/ready/' + str(i): 'size=64k' for i in range(100)}}}, 201)['Id']
            request('POST', '/containers/' + cid + '/start', status=204)
            # No wait for a workload command: a successful start is the barrier.
            eid = request('POST', '/containers/' + cid + '/exec', {
                'Cmd': ['sh', '-ec', 'test "$(cat /etc/hostname)" = "$EXPECTED_HOSTNAME"']}, 201)['Id']
            request('POST', '/exec/' + eid + '/start', {'Detach': True})
            deadline = time.monotonic() + 10
            while True:
                state = request('GET', '/exec/' + eid + '/json')
                if not state['Running']:
                    break
                assert time.monotonic() < deadline, 'exec did not finish'
                time.sleep(.01)
            assert state['ExitCode'] == 0, ('exec did not observe the completed container root', state)
            request('DELETE', '/containers/' + cid + '?force=1', status=204)
            owned.remove(cid)
        print('PASS 20 immediate execs observe completed container mount setup', flush=True)
    finally:
        for cid in owned:
            request('DELETE', '/containers/' + cid + '?force=1', status=204)


if __name__ == '__main__':
    check(sys.argv[1], sys.argv[2])
