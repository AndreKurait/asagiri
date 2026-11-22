"""Check the published browser service against a real Windows named pipe."""
import base64
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import time
import urllib.request

bundle = Path(os.environ['BROWSER_BUNDLE'])
cli = bundle / 'bin/asagiri.exe'
pipe = r'\\.\pipe\asagiri-browser-106'
docker = [bundle / 'bin/docker.exe', '--host', 'npipe:////./pipe/asagiri-browser-106']
env = dict(os.environ, ASAGIRI_PIPE=pipe)
app = subprocess.Popen([bundle / 'asagiri-win.exe', '--distro', 'Asagiri-browser-106', '--pipe', pipe])
cid = None
web = None
try:
    for attempt in range(30):
        if not subprocess.run(docker + ['info'], stdout=subprocess.DEVNULL, timeout=30).returncode:
            break
        time.sleep(2)
    else:
        raise RuntimeError('Windows engine did not answer')
    name = 'browser-' + secrets.token_hex(6)
    cid = subprocess.check_output(docker + ['create', '--name', name, 'alpine:3.21', 'true'], text=True).strip()
    password = secrets.token_urlsafe(24)
    subprocess.run([cli, 'web', 'password', '--password-stdin'], env=env, input=password + '\n', text=True, check=True)
    web = subprocess.Popen([cli, 'web', '--port', '0'], env=env, stdout=subprocess.PIPE, text=True)
    origin = re.search(r'http://127\.0\.0\.1:\d+', web.stdout.readline())[0]
    auth = base64.b64encode(('asagiri:' + password).encode()).decode()
    request = urllib.request.Request(origin + '/api/call', data=json.dumps({'name': 'asagiri_list_containers', 'arguments': {'limit': 100}}).encode(), headers={'Origin': origin, 'Authorization': 'Basic ' + auth, 'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=30) as response:
        result = json.load(response)['result']
    assert name in json.dumps(result) and cid[:12] in json.dumps(result), result
    print('Browser returned the container created through the native Windows engine:', name)
finally:
    if web:
        web.terminate()
        web.wait(timeout=15)
    if cid:
        subprocess.run(docker + ['rm', cid], timeout=30, check=True)
    app.terminate()
    app.wait(timeout=15)
