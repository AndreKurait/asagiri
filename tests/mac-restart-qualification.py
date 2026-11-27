#!/usr/bin/env python3
"""Verify manual activation of a replaced release bundle on an ephemeral Mac."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import plistlib
import signal
import socket
import struct
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
RELEASES = (
    ('0.1.15', 'd53714fb4d28647edfa811b63608c788f80bdc2bd0da6d151846821e0d7e0918'),
    ('0.1.16-dev.349', '3d8b05725e0c22558b3528524bd2e8975ee668c6a67717702c08d613d1274426'),
)


def release_app(directory, version, expected):
    directory.mkdir()
    asset = f'Asagiri-{version}-arm64.tar.gz'
    subprocess.run(['gh', 'release', 'download', 'v'+version, '--repo', 'AndreKurait/asagiri',
                    '--pattern', asset, '--dir', directory], check=True)
    digest = hashlib.sha256()
    with (directory / asset).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    assert digest.hexdigest() == expected
    subprocess.run(['/usr/bin/tar', '-xzf', directory/asset, '-C', directory], check=True)
    app = directory / 'Asagiri.app'
    subprocess.run(['codesign', '--verify', '--deep', '--strict', app], check=True)
    assert plistlib.loads((app/'Contents/Info.plist').read_bytes())['AsagiriVersion'] == version
    assert subprocess.check_output([app/'Contents/MacOS/asagiri','--version'], text=True).strip() == 'Asagiri '+version
    print(f'Verified published {asset} {expected}', flush=True)
    return app


def engine(endpoint):
    client = http.client.HTTPConnection('localhost')
    try:
        client.sock = socket.socket(socket.AF_UNIX)
        client.sock.settimeout(2)
        client.sock.connect(str(endpoint))
        # macOS SOL_LOCAL / LOCAL_PEERPID identifies the actual socket server.
        pid = struct.unpack('i', client.sock.getsockopt(0, 2, 4))[0]
        client.request('GET', '/version')
        response = client.getresponse()
        assert response.status == 200
        return pid, json.loads(response.read())['AsagiriVersion']
    finally:
        client.close()


def wait_for_version(endpoint, version):
    deadline = time.monotonic() + 65
    while time.monotonic() < deadline:
        try:
            pid, observed = engine(endpoint)
            if observed == version:
                return pid
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(.2)
    raise RuntimeError(f'Engine did not report {version}')


def terminate(process):
    if process is not None and process.poll() is None:
        process.terminate()
        process.wait(timeout=20)


def qualify():
    if os.environ.get('ASAGIRI_RUNNER_ENVIRONMENT') != 'github-hosted' or os.environ.get('RUNNER_OS') != 'macOS':
        raise RuntimeError('This qualification uses an unmodified production bundle on a disposable hosted Mac')
    with tempfile.TemporaryDirectory(prefix='asa99-', dir='/private/tmp') as tmp:
        work = Path(tmp)
        old_app = release_app(work/'old', *RELEASES[0])
        update = release_app(work/'new', *RELEASES[1])
        installed = work/'Asagiri.app'
        old_app.rename(installed)
        binary = installed/'Contents/MacOS/asagiri'
        endpoint = work/'run/docker.sock'
        endpoint.parent.mkdir(mode=0o700)
        home = work/'home'
        settings = home/'.asagiri'
        settings.mkdir(parents=True)
        (settings/'ui.json').write_text(json.dumps({'lang':'en','langAsked':True,'domainsAsked':True,
            'setupSkip':True,'updateAuto':False,'stopEngineOnQuit':False}))
        env = {**os.environ, 'HOME':str(home), 'ASAGIRI_SOCKET':str(endpoint),
               'ASAGIRI_DATA_DIR':str(work/'store'), 'ASAGIRI_NO_FINDER':'1', 'ASAGIRI_IDLE_SECS':'0'}
        env.pop('GH_TOKEN', None)
        env.pop('ASAGIRI_UPDATE_URL', None)
        with (work/'old.log').open('w') as log:
            old = subprocess.Popen([binary, 'daemon'], env=env, stdout=log, stderr=log)
            ui = None
            try:
                original_pid = wait_for_version(endpoint, RELEASES[0][0])
                assert original_pid == old.pid
                installed.rename(work/'retired.app')
                update.rename(installed)
                ui = subprocess.Popen([binary, 'ui'], env=env)
                assert engine(endpoint) == (original_pid, RELEASES[0][0])
                print(f'New app open with old daemon {original_pid} serving {RELEASES[0][0]}', flush=True)
                subprocess.run(['swift', ROOT/'tests/mac-restart-qualification.swift', str(ui.pid)],
                               env=env, check=True, timeout=75)
                current_pid = wait_for_version(endpoint, RELEASES[1][0])
                assert current_pid != original_pid and old.wait(timeout=20) == 0
                print(f'Restart engine replaced PID {original_pid} with {current_pid}, socket reports {RELEASES[1][0]}', flush=True)
                subprocess.run(['swift', ROOT/'tests/mac-restart-qualification.swift', str(ui.pid), 'quit'],
                               env=env, check=True, timeout=35)
                assert ui.wait(timeout=25) == 0
                ui = subprocess.Popen([binary, 'ui'], env=env)
                time.sleep(3)
                assert ui.poll() is None and engine(endpoint) == (current_pid, RELEASES[1][0])
                print('Reopening the app preserved the new daemon PID and version.', flush=True)
            finally:
                terminate(ui)
                try:
                    pid, _ = engine(endpoint)
                except (OSError, http.client.HTTPException):
                    pass
                else:
                    command = subprocess.check_output(['ps','-p',str(pid),'-o','command='],text=True).strip()
                    if command in (f'{binary} daemon', f'{work}/retired.app/Contents/MacOS/asagiri daemon'):
                        os.kill(pid, signal.SIGTERM)
                terminate(old)
                for saved in [work/'old.log', work/'daemon.log', *home.rglob('ui.log'), *home.rglob('daemon.log')]:
                    if saved.is_file():
                        print(saved.name, saved.read_text(errors='replace')[-2500:])


if __name__ == '__main__':
    qualify()
