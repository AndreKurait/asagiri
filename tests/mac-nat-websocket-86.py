import base64,hashlib,os,select,socket,subprocess,time
from pathlib import Path
root=Path.cwd(); name='asagiri-ws-idle-86'
key=base64.b64encode(os.urandom(16)).decode(); accept=base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
server=root/'ws-handler.sh'; server.write_text('#!/bin/sh\nwhile IFS= read -r line; do [ "$line" = "$(printf \'\\r\')" ] && break; done\nprintf \'HTTP/1.1 101 Switching Protocols\\r\\nUpgrade: websocket\\r\\nConnection: Upgrade\\r\\nSec-WebSocket-Accept: '+accept+'\\r\\n\\r\\n\'\ndd bs=1 count=7 of=/tmp/frame 2>/dev/null\n[ "$(od -An -tx1 /tmp/frame | xargs)" = "89 81 01 02 03 04 79" ] && printf \'\\212\\001x\'\n'); server.chmod(0o755)
def docker(*args):return subprocess.check_output(['docker',*args],text=True).strip()
docker('create','--name',name,'-p','127.0.0.1::8087','alpine:3.21','nc','-lk','-p','8087','-e','/handler');
try:
 docker('cp',str(server),name+':/handler');docker('start',name);port=int(docker('port',name,'8087/tcp').rsplit(':',1)[1]);s=socket.create_connection(('127.0.0.1',port),15);s.sendall(f'GET / HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n\r\n'.encode());f=s.makefile('rb');assert b'101 Switching' in f.readline(); headers=[]
 while (line:=f.readline()) not in (b'\r\n',b''):headers.append(line.strip())
 assert ('Sec-WebSocket-Accept: '+accept).encode() in headers;print('PASS WebSocket upgrade',flush=True);start=time.monotonic();assert not select.select([s],[],[],425)[0];s.sendall(bytes.fromhex('89810102030479'));assert f.read(3)==b'\x8a\x01x';print('PASS masked ping and matching pong after',time.monotonic()-start,flush=True)
finally:docker('rm','-f',name)
