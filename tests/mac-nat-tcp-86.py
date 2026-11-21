import socket,subprocess,time,select
name='asagiri-idle-86'
subprocess.run(['docker','run','-d','--name',name,'-p','127.0.0.1::8086','alpine:3.21','nc','-lk','-p','8086','-e','/bin/cat'],check=True)
try:
 port=int(subprocess.check_output(['docker','port',name,'8086/tcp'],text=True).strip().rsplit(':',1)[1]);
 with socket.create_connection(('127.0.0.1',port),timeout=15) as sock:
  stream=sock.makefile('rb');sock.sendall(b'before idle\n');assert stream.readline()==b'before idle\n';start=time.monotonic();print('PASS initial echo',port,flush=True)
  ready,_,_=select.select([sock],[],[],425);elapsed=time.monotonic()-start
  if ready:raise AssertionError(f'Unexpected data or FIN at {elapsed:.2f}s: {sock.recv(64)!r}')
  sock.sendall(b'after idle\n');assert stream.readline()==b'after idle\n';print(f'PASS echo after {elapsed:.2f}s idle',flush=True)
finally:
 subprocess.run(['docker','rm','-f',name],check=True)
