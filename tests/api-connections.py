#!/usr/bin/env python3
"""API slots support fleets, reject excess clients, and release closed streams."""
import argparse
import http.client
import json
import socket
import time
from urllib.parse import urlencode

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('socket')
parser.add_argument('image', nargs='?', default='alpine:3.21')
parser.add_argument('--capacity', type=int, default=0,
                    help='also hold this many mixed idle and wait connections')
parser.add_argument('--connection-limit', type=int,
                    help='fill this exact daemon limit and verify excess connections close')
parser.add_argument('--only-capacity', action='store_true',
                    help='run only the mixed-capacity and optional limit checks')
parser.add_argument('--name', help='name for the test-owned container')
args = parser.parse_args()
if args.capacity < 0 or args.capacity > 4096:
    parser.error('--capacity must be between 0 and 4096')
if args.only_capacity and not args.capacity:
    parser.error('--only-capacity requires --capacity')
if args.connection_limit is not None and not (0 < args.capacity < args.connection_limit <= 4096):
    parser.error('--connection-limit requires 0 < capacity < connection-limit <= 4096')


class Connection(http.client.HTTPConnection):
    def __init__(self):
        super().__init__('localhost', timeout=10)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(10)
        self.sock.connect(args.socket)
        self.transport = self.sock


def request(method, path, body=None, *, decode_json=True):
    conn = Connection()
    try:
        conn.request(method, path, json.dumps(body) if body is not None else None,
                     {'Content-Type': 'application/json'})
        response = conn.getresponse()
        data = response.read()
        assert response.status < 300, (response.status, data)
        return (json.loads(data) if data else None) if decode_json else data
    finally:
        conn.close()


def start(command):
    query = '?' + urlencode({'name': args.name}) if args.name else ''
    cid = request('POST', '/containers/create' + query, {
        'Image': args.image, 'Cmd': command, 'HostConfig': {'NetworkMode': 'none'},
    })['Id']
    print(f'OWNED_CONTAINER {cid}', flush=True)
    try:
        request('POST', f'/containers/{cid}/start')
    except BaseException:
        request('DELETE', f'/containers/{cid}?force=1')
        raise
    return cid


def stream(cid, endpoint):
    conn = Connection()
    try:
        conn.request('POST' if endpoint == 'wait' or endpoint.startswith('attach') else 'GET', f'/containers/{cid}/{endpoint}')
        response = conn.getresponse()
        assert response.status == 200, response.status
        return conn, response
    except BaseException:
        conn.close()
        raise


def close(pair):
    pair[1].close()
    pair[0].close()


def idle():
    conn = Connection()
    try:
        conn.request('GET', '/_ping')
        response = conn.getresponse()
        assert response.status == 200 and response.read() == b'OK'
        return conn, response
    except BaseException:
        conn.close()
        raise


def controls(cid):
    assert request('GET', '/_ping', decode_json=False) == b'OK'
    inspected = request('GET', f'/containers/{cid}/json')
    assert inspected['Id'] == cid and inspected['State']['Running'], inspected


def released_slot(check):
    deadline = time.monotonic() + 5
    while True:
        try:
            return check()
        except (http.client.RemoteDisconnected, ConnectionResetError, BrokenPipeError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


def rejected():
    conn = Connection()
    try:
        try:
            conn.request('GET', '/_ping')
            response = conn.getresponse()
        except (http.client.RemoteDisconnected, ConnectionResetError, BrokenPipeError):
            return
        response.close()
        raise AssertionError('A connection above the configured limit received an HTTP response')
    finally:
        conn.close()


def capacity(cid):
    clients = []
    try:
        for index in range(args.capacity):
            clients.append(idle() if index % 2 == 0 else stream(cid, 'wait'))
            if index % 64 == 0:
                controls(cid)
        controls(cid)
        print(f'PASS {len(clients)} simultaneous mixed idle/wait clients plus ping and inspect', flush=True)
        if args.connection_limit:
            while len(clients) < args.connection_limit:
                clients.append(idle())
            rejected()
            close(clients.pop())
            released_slot(lambda: controls(cid))
            clients.append(released_slot(idle))
            rejected()
            print(f'PASS limit {args.connection_limit}: excess closes, released slot is reusable', flush=True)
    finally:
        for client in clients:
            close(client)
        released_slot(lambda: controls(cid))
    print('PASS all capacity clients closed; ping and inspect still succeed', flush=True)


cid = start(['sleep', '600'])
try:
    if not args.only_capacity:
        for endpoint in ['wait', 'logs?follow=1&stdout=1', 'attach?stream=1&stdout=1&stderr=1']:
            for _ in range(8):
                for _ in range(40):
                    close(stream(cid, endpoint))
                time.sleep(0.4)
                request('GET', '/version')
            print(f'PASS 320 disconnected {endpoint} clients release their slots', flush=True)
    if args.capacity:
        capacity(cid)
finally:
    request('DELETE', f'/containers/{cid}?force=1')

for endpoint in ([] if args.only_capacity else ['wait', 'logs?follow=1&stdout=1']):
    cid = start(['sh', '-c', 'sleep 2; echo half-close-reply; exit 7'])
    try:
        conn, response = stream(cid, endpoint)
        conn.transport.shutdown(socket.SHUT_WR)
        data = response.read()
        if endpoint == 'wait':
            assert json.loads(data)['StatusCode'] == 7, data
        else:
            assert b'half-close-reply' in data, data
        close((conn, response))
        print(f'PASS {endpoint} write-half-close still receives terminal response', flush=True)
    finally:
        request('DELETE', f'/containers/{cid}?force=1')
