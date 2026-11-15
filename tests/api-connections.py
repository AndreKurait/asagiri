#!/usr/bin/env python3
"""Closed quiet streams release API slots; half-closed readers keep theirs."""
import argparse
import http.client
import json
import socket
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('socket')
parser.add_argument('image', nargs='?', default='alpine:3.21')
parser.add_argument('--capacity', type=int, default=0,
                    help='also hold this many live waits (requires matching daemon capacity)')
args = parser.parse_args()


class Connection(http.client.HTTPConnection):
    def __init__(self):
        super().__init__('localhost', timeout=10)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(10)
        self.sock.connect(args.socket)
        self.transport = self.sock


def request(method, path, body=None):
    conn = Connection()
    try:
        conn.request(method, path, json.dumps(body) if body is not None else None,
                     {'Content-Type': 'application/json'})
        response = conn.getresponse()
        data = response.read()
        assert response.status < 300, (response.status, data)
        return json.loads(data) if data else None
    finally:
        conn.close()


def start(command):
    cid = request('POST', '/containers/create', {'Image': args.image, 'Cmd': command})['Id']
    request('POST', f'/containers/{cid}/start')
    return cid


def stream(cid, endpoint):
    conn = Connection()
    conn.request('POST' if endpoint == 'wait' or endpoint.startswith('attach') else 'GET', f'/containers/{cid}/{endpoint}')
    response = conn.getresponse()
    assert response.status == 200, response.status
    return conn, response


def close(pair):
    pair[1].close()
    pair[0].close()


cid = start(['sleep', '600'])
try:
    for endpoint in ['wait', 'logs?follow=1&stdout=1', 'attach?stream=1&stdout=1&stderr=1']:
        for _ in range(8):
            for _ in range(40):
                close(stream(cid, endpoint))
            time.sleep(0.4)
            request('GET', '/version')
        print(f'PASS 320 disconnected {endpoint} clients release their slots', flush=True)
    if args.capacity:
        clients = []
        try:
            for index in range(args.capacity):
                clients.append(stream(cid, 'wait'))
                if index % 64 == 0:
                    request('GET', '/version')
            request('GET', '/version')
            print(f'PASS {len(clients)} simultaneous live waits plus control requests', flush=True)
        finally:
            for client in clients:
                close(client)
        time.sleep(0.5)
        request('GET', '/version')
finally:
    request('DELETE', f'/containers/{cid}?force=1')

for endpoint in ['wait', 'logs?follow=1&stdout=1']:
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
