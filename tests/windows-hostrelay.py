"""Exercise the shipping WSL hostrelay command across Windows stdio pipes."""
import struct
import subprocess
import time
from threading import Timer
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


def read_with_deadline(proc, size, stage):
    expired = False

    def interrupt():
        nonlocal expired
        expired = True
        print(f'Timed out waiting for {stage}', flush=True)
        proc.kill()

    timer = Timer(15, interrupt)
    timer.start()
    try:
        data = proc.stdout.read(size)
    finally:
        timer.cancel()
        timer.join()
    if expired:
        raise TimeoutError(f'Timed out waiting for {stage}')
    return data


def receive(proc):
    kind, ident, length = HEADER.unpack(read_with_deadline(proc, HEADER.size, 'relay frame header'))
    data = read_with_deadline(proc, length, 'relay frame body')
    assert len(data) == length
    return kind, ident, data


def client_connections(port):
    table = subprocess.check_output(WSL + ['cat', '/proc/net/tcp'], timeout=5)
    remote = f'0100007F:{port:04X}'.encode()
    return {fields[1]: fields[3] for row in table.splitlines()[1:]
            if (fields := row.split())[2] == remote}


def exercise():
    with ExitStack() as stack:
        relay = child(stack, '/usr/local/bin/asagiri', 'hostrelay')
        target = struct.pack('>H', 18080)
        relay.stdin.write(HEADER.pack(6, 0, len(target)) + target)
        relay.stdin.flush()
        kind, _, data = receive(relay)
        assert kind == 7 and data[:2] == target
        port, = struct.unpack('>H', data[2:])
        assert port
        client = child(stack, 'busybox', 'nc', '127.0.0.1', str(port))
        kind, ident, data = receive(relay)
        assert kind == 1 and data == target
        relay.stdin.write(HEADER.pack(2, ident, 4) + b'live')
        relay.stdin.flush()
        assert read_with_deadline(client, 4, 'data delivery') == b'live'
        sockets = client_connections(port)
        established, = [local for local, state in sockets.items() if state == b'01']
        relay.stdin.close()
        assert relay.wait(timeout=15) == 0
        # Netcat can keep copying stdin after receiving a TCP FIN. Check the
        # specific guest socket before closing stdin so our cleanup cannot
        # produce the observed disconnect.
        for _ in range(20):
            state = client_connections(port).get(established)
            if state is None or state == b'08':  # removed or CLOSE_WAIT
                break
            time.sleep(0.1)
        else:
            raise RuntimeError(f'Guest connection remained in TCP state {state!r}')
        client.stdin.close()
        assert read_with_deadline(client, 1, 'netcat exit after disconnect') == b''
        assert client.wait(timeout=15) == 0
        # A new listener must be able to bind the released port immediately.
        listener = child(stack, 'busybox', 'nc', '-l', '-p', str(port), '-s', '127.0.0.1')
        for _ in range(20):
            if listener.poll() is not None:
                raise RuntimeError(f'Could not rebind WSL port {port}')
            probe = subprocess.run(WSL + ['busybox', 'nc', '-w', '1', '127.0.0.1', str(port)],
                                   input=b'rebound', capture_output=True, timeout=5)
            if probe.returncode == 0:
                break
            time.sleep(0.1)
        else:
            raise RuntimeError(f'Rebound listener on {port} did not accept a connection')
        assert read_with_deadline(listener, 7, 'rebound listener payload') == b'rebound'
        print(f'Active connection closed and WSL listener port {port} rebound', flush=True)


if __name__ == '__main__':
    exercise()
    exercise()
