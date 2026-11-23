#!/usr/bin/env python3
"""Docker inspection preserves requested bindings and exposed protocols."""
import http.client
import json
import os
import socket
import subprocess
import sys
import time
import uuid


def check(path, image):
    assert os.environ.get('ASAGIRI_TEST_ENGINE') == '1', 'Use a disposable engine only'
    docker = ['docker', '-H', 'unix://' + path]
    owned = []

    def call(*args):
        return subprocess.check_output(docker + list(args), text=True, timeout=30).strip()

    def inspect(cid):
        return json.loads(call('inspect', cid))[0]

    def free_port():
        with socket.socket() as stream:
            stream.bind(('127.0.0.1', 0))
            return stream.getsockname()[1]

    def serves(port):
        connection = http.client.HTTPConnection('127.0.0.1', port, timeout=1)
        try:
            connection.request('GET', '/')
            response = connection.getresponse()
            return response.status == 200 and response.read() == b'loopback-binding-test'
        except (OSError, http.client.HTTPException):
            return False
        finally:
            connection.close()

    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as route:
        route.connect(('192.0.2.1', 9))  # Route lookup only; no packet is sent.
        private_address = route.getsockname()[0]
    assert private_address != '127.0.0.1'
    try:
        for requested in (str(free_port()), '0'):
            cid = call('create', '--name', 'inspect-binding-' + uuid.uuid4().hex,
                       '--label', 'asagiri.test=inspect-port-bindings',
                       '--expose', '5353/udp', '--expose', '9000/tcp',
                       '-p', '127.0.0.1:' + requested + ':8080', '--entrypoint', '/bin/sh', image,
                       '-c', "while :; do printf 'HTTP/1.1 200 OK\\r\\nContent-Length: 21\\r\\nConnection: close\\r\\n\\r\\nloopback-binding-test' | nc -l -p 8080 >/dev/null; done")
            owned.append(cid)
            expected = {'8080/tcp': [{'HostIp': '127.0.0.1', 'HostPort': requested}]}
            for phase in ('created', 'running', 'exited'):
                if phase == 'running':
                    call('start', cid)
                elif phase == 'exited':
                    call('stop', '-t', '0', cid)
                row = inspect(cid)
                assert row['State']['Status'] == phase, row['State']
                assert row['HostConfig'].get('PortBindings') == expected, (phase, row['HostConfig'].get('PortBindings'), expected)
                exposed = row['Config'].get('ExposedPorts')
                assert exposed == {'8080/tcp': {}, '5353/udp': {}, '9000/tcp': {}}, (phase, exposed)
                ports = row['NetworkSettings']['Ports']
                if phase != 'running':
                    assert not ports, (phase, ports)
                    continue
                assert ports['5353/udp'] is None and ports['9000/tcp'] is None, ports
                actual = ports['8080/tcp']
                assert len(actual) == 1 and actual[0]['HostIp'] == '127.0.0.1', actual
                port = int(actual[0]['HostPort'])
                assert port > 0 and (requested == '0' or port == int(requested)), actual
                deadline = time.monotonic() + 10
                while not serves(port):
                    assert time.monotonic() < deadline, 'Loopback endpoint did not serve'
                    time.sleep(0.05)
                for address in ('127.0.0.2', private_address):
                    with socket.socket() as probe:
                        probe.settimeout(1)
                        assert probe.connect_ex((address, port)) != 0, ('Published outside requested loopback address', address, port)
                # Verify the actual listener, in addition to reachability.
                if sys.platform == 'darwin':
                    output = subprocess.check_output(['lsof', '-nP', '-a', f'-iTCP:{port}', '-sTCP:LISTEN', '-Fn'], text=True)
                    addresses = {line[1:] for line in output.splitlines() if line.startswith('n')}
                    assert addresses == {f'127.0.0.1:{port}'}, addresses
                else:
                    with open('/proc/net/tcp') as table:
                        listeners = [line.split()[1] for line in table if len(line.split()) > 3 and line.split()[3] == '0A']
                    assert f'0100007F:{port:04X}' in listeners, listeners
            call('rm', '-v', cid)
            owned.remove(cid)
            print('PASS requested ' + ('ephemeral' if requested == '0' else 'fixed') + ' loopback binding through create/start/stop; actual port isolated')
        cid = call('create', '--network', 'none', '--entrypoint', '/bin/true', image)
        owned.append(cid)
        assert 'ExposedPorts' not in inspect(cid)['Config'], 'An absent exposed-port map must be omitted'
        call('rm', '-v', cid)
        owned.remove(cid)
        print('PASS empty exposed-port map omitted')
    finally:
        for cid in owned:
            subprocess.run(docker + ['rm', '-fv', cid], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)


if __name__ == '__main__':
    check(sys.argv[1], sys.argv[2])
