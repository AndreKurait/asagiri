"""Exercise the shipping WSL hostrelay command across Windows stdio pipes."""
import struct
import subprocess
from contextlib import ExitStack

WSL = ['wsl.exe', '-d', 'asagiri', '-u', 'root', '--']
HEADER = struct.Struct('>BII')


def child(stack, *command):
    proc = subprocess.Popen(WSL + list(command), stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    def cleanup():
        if proc.poll() is None:
            proc.kill()
        proc.wait(timeout=10)
    stack.callback(cleanup)
    return proc


def receive(pipe):
    kind, ident, length = HEADER.unpack(pipe.read(HEADER.size))
    data = pipe.read(length)
    assert len(data) == length
    return kind, ident, data


def exercise():
    with ExitStack() as stack:
        relay = child(stack, '/usr/local/bin/asagiri', 'hostrelay')
        target = struct.pack('>H', 18080)
        relay.stdin.write(HEADER.pack(6, 0, len(target)) + target)
        relay.stdin.flush()
        kind, _, data = receive(relay.stdout)
        assert kind == 7 and data[:2] == target
        port, = struct.unpack('>H', data[2:])
        assert port
        client = child(stack, 'busybox', 'nc', '127.0.0.1', str(port))
        kind, ident, data = receive(relay.stdout)
        assert kind == 1 and data == target
        relay.stdin.write(HEADER.pack(2, ident, 4) + b'live')
        relay.stdin.flush()
        assert client.stdout.read(4) == b'live'
        relay.stdin.close()
        assert relay.wait(timeout=15) == 0
        assert client.stdout.read(1) == b''
        assert client.wait(timeout=15) == 0
        # A new listener must be able to bind the released port immediately.
        subprocess.run(WSL + ['busybox', 'httpd', '-p', f'127.0.0.1:{port}', '-h', '/tmp'], check=True, timeout=10)
        print(f'Active connection closed and WSL listener port {port} rebound', flush=True)


if __name__ == '__main__':
    exercise()
    exercise()
