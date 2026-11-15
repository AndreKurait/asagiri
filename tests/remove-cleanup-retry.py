"""A native cleanup timeout keeps the stopped container available for retry."""
import http.client,json,os,pathlib,signal,socket,subprocess,sys,time,uuid
assert os.environ.get('ASAGIRI_TEST_ENGINE')=='1'
path,image=sys.argv[1:]
d=['docker','-H','unix://'+path]
def call(*args):return subprocess.check_output(d+list(args),timeout=30,stderr=subprocess.STDOUT)
def request(method,target):
 c=http.client.HTTPConnection('localhost',timeout=40)
 c.sock=socket.socket(socket.AF_UNIX);c.sock.settimeout(40);c.sock.connect(path)
 try:
  c.request(method,target,headers={'Connection':'close'});r=c.getresponse();body=r.read()
  return r.status,json.loads(body) if body else None
 finally:c.close()
def destroys(cid,since):
 rows=call('events','--since',str(since),'--until',str(time.time()),'--filter','container='+cid,'--format','{{json .}}').splitlines()
 return [json.loads(row) for row in rows if json.loads(row).get('Action')=='destroy']
trace=None;cid=None
try:
 since=int(time.time())-1
 cid=call('run','-d','--network','none','--label','fixture.cleanup='+uuid.uuid4().hex,image,'sleep','120').decode().strip()
 call('exec',cid,'sh','-c','printf retained > /removal-proof')
 agents=[]
 for p in pathlib.Path('/proc').iterdir():
  if not p.name.isdigit():continue
  try:
   fields=(p/'stat').read_text().rsplit(')',1)[1].split()
   if (p/'comm').read_text().strip()=='asagiri-init' and fields[0]!='Z' and int(fields[1])==1:agents.append(int(p.name))
  except (OSError,ValueError):pass
 assert len(agents)==1,agents
 with open('/tmp/remove-cleanup-retry.trace','wb') as log:
  trace=subprocess.Popen(['strace','-f','-e','trace=umount2','-e','inject=umount2:delay_enter=40s','-p',str(agents[0])],stdout=log,stderr=log)
  deadline=time.monotonic()+10
  while 'TracerPid:\t'+str(trace.pid)+'\n' not in pathlib.Path('/proc',str(agents[0]),'status').read_text():
   assert time.monotonic()<deadline,'strace did not attach';time.sleep(.02)
  call('stop','-t','0',cid)
  started=time.monotonic();status,body=request('DELETE','/containers/'+cid+'?force=true');elapsed=time.monotonic()-started
  assert status==500 and 'still completing filesystem cleanup' in body.get('message',''),(status,body,elapsed)
  assert 25<elapsed<35,elapsed
  assert not destroys(cid,since),'failed removal published destroy'
  status,body=request('GET','/containers/'+cid+'/json')
  assert status==200 and body['Id']==cid and body['State']['Status']=='exited',('cleanup timeout lost retryable container',status,body)
  trace.send_signal(signal.SIGINT);trace.wait(timeout=15);trace=None
  # The original cleanup must complete before either start or removal can reuse its paths.
  call('start',cid)
  assert call('exec',cid,'cat','/removal-proof')==b'retained'
  status,body=request('DELETE','/containers/'+cid+'?force=true');assert status==204,(status,body)
  status,body=request('GET','/containers/'+cid+'/json');assert status==404,(status,body)
  assert len(destroys(cid,since))==1,'successful retry must publish exactly one destroy'
  cid=None
  print(json.dumps({'probe':'remove-cleanup-retry','nativeCleanupTimeoutSeconds':elapsed,'identityAndWritableDataRetained':True,'retryRemoved':True}),flush=True)
finally:
 if trace is not None:
  trace.send_signal(signal.SIGINT);trace.wait(timeout=15)
 if cid is not None:subprocess.run(d+['rm','-f',cid],timeout=40,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
