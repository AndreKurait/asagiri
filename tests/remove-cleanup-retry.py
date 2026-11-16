"""A native cleanup timeout keeps the stopped container available for retry."""
import http.client,json,os,pathlib,platform,select,signal,socket,subprocess,sys,tarfile,time,uuid
from concurrent.futures import ThreadPoolExecutor
assert os.environ.get('ASAGIRI_TEST_ENGINE')=='1'
path,image=sys.argv[1:]
d=['docker','-H','unix://'+path]
def call(*args,timeout=30):return subprocess.check_output(d+list(args),timeout=timeout,stderr=subprocess.STDOUT)
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
def malformed_upload(cid):
 # Keep HTTP framing valid, but make this acquired-root copy fail its tar checksum.
 header=tarfile.TarInfo('malformed-cleanup-probe').tobuf()
 c=http.client.HTTPConnection('localhost',timeout=60)
 c.sock=socket.socket(socket.AF_UNIX);c.sock.settimeout(60);c.sock.connect(path)
 try:
  c.request('PUT','/containers/'+cid+'/archive?path=/',b'!'+header[1:],{'Content-Type':'application/x-tar','Connection':'close'})
  r=c.getresponse();return r.status,json.loads(r.read())
 finally:c.close()
trace=None;cid=None;survivor=None;worker_fd=None;pool=ThreadPoolExecutor(max_workers=1)
try:
 since=int(time.time())-1
 survivor=call('run','-d','--network','none','--label','fixture.cleanup='+uuid.uuid4().hex,image,'sleep','120').decode().strip()
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
 rootfs='/var/lib/asagiri/containers/'+cid+'/rootfs'
 def mounts():
  return [line.split() for line in pathlib.Path('/proc',str(agents[0]),'mountinfo').read_text().splitlines() if line.split()[4]==rootfs]
 assert len(mounts())==1,mounts()
 call('stop','-t','0',cid);call('diff',cid)
 with open('/tmp/remove-cleanup-retry.trace','wb') as log:
  trace=subprocess.Popen(['strace','-f','-P',rootfs,'-e','trace=umount2','-e','inject=umount2:delay_enter=40s','-p',str(agents[0])],stdout=log,stderr=log)
  deadline=time.monotonic()+10
  while 'TracerPid:\t'+str(trace.pid)+'\n' not in pathlib.Path('/proc',str(agents[0]),'status').read_text():
   assert time.monotonic()<deadline,'strace did not attach';time.sleep(.02)
  # A normal stopped root stays ready for reuse. A failed upload instead
  # requires cleanup, giving DELETE a real pre-existing barrier to wait for.
  upload=pool.submit(malformed_upload,cid)
  blocked=None;deadline=time.monotonic()+5
  while blocked is None and time.monotonic()<deadline:
   for item in pathlib.Path('/proc').iterdir():
    if not item.name.isdigit():continue
    try:
     if 'TracerPid:\t'+str(trace.pid)+'\n' not in (item/'status').read_text():continue
     syscall=(item/'syscall').read_text().split()
     if syscall and syscall[0]=={'x86_64':'166','aarch64':'39'}[platform.machine()]:blocked=int(item.name);break
    except OSError:pass
   time.sleep(.01)
  assert blocked is not None and blocked!=agents[0],'No asynchronous required-cleanup umount barrier observed'
  assert os.readlink('/proc/'+str(blocked)+'/ns/mnt')==os.readlink('/proc/'+str(agents[0])+'/ns/mnt')
  mounted=mounts();assert len(mounted)==1,mounted
  mount_id=mounted[0][0];worker_fd=os.pidfd_open(blocked)
  assert not upload.done(),'Failed upload acknowledged before cleanup'
  # The syscall entry precedes the agent's CleanupPending event. An ordinary
  # round trip through that agent orders the event before DELETE, and proves
  # unrelated work remains responsive without using a timing sleep.
  responsive_at=time.monotonic();call('exec',survivor,'true',timeout=3)
  unrelated_seconds=time.monotonic()-responsive_at;assert unrelated_seconds<3,unrelated_seconds
  started=time.monotonic();status,body=request('DELETE','/containers/'+cid+'?force=true');elapsed=time.monotonic()-started
  assert status==500 and 'still completing filesystem cleanup' in body.get('message',''),(status,body,elapsed)
  assert 25<elapsed<35,elapsed
  assert not destroys(cid,since),'failed removal published destroy'
  status,body=request('GET','/containers/'+cid+'/json')
  assert status==200 and body['Id']==cid and body['State']['Status']=='exited',('cleanup timeout lost retryable container',status,body)
  assert not upload.done(),'Failed upload acknowledged while required cleanup is still delayed'
  trace.send_signal(signal.SIGINT);trace.wait(timeout=15);trace=None
  status,body=upload.result(timeout=15)
  assert status==400 and 'checksum' in body.get('message','').lower(),(status,body)
  poll=select.poll();poll.register(worker_fd,select.POLLIN)
  assert poll.poll(0),'Failed upload completed before cleanup worker exit'
  assert all(row[0]!=mount_id for row in mounts()),'Failed upload left its overlay mounted'
  # The original cleanup must complete before either start or removal can reuse its paths.
  call('start',cid)
  assert call('exec',cid,'cat','/removal-proof')==b'retained'
  status,body=request('DELETE','/containers/'+cid+'?force=true');assert status==204,(status,body)
  status,body=request('GET','/containers/'+cid+'/json');assert status==404,(status,body)
  assert len(destroys(cid,since))==1,'successful retry must publish exactly one destroy'
  deadline=time.monotonic()+30
  while pathlib.Path(rootfs).parent.exists():
   assert time.monotonic()<deadline,'Successful retry left its removed tree';time.sleep(.05)
  assert not mounts(),'Successful retry left its overlay mounted'
  cid=None
  print(json.dumps({'probe':'remove-cleanup-retry','cleanupTrigger':'malformed archive','blockedPid':blocked,'retiringMountId':mount_id,'injectedDelaySeconds':40,'unrelatedExecSeconds':unrelated_seconds,'nativeCleanupTimeoutSeconds':elapsed,'identityAndWritableDataRetained':True,'failedUploadAndCleanupDrained':True,'retryRemoved':True,'removedTreeAbsent':True}),flush=True)
finally:
 if trace is not None:
  trace.send_signal(signal.SIGINT);trace.wait(timeout=15)
 pool.shutdown(wait=True,cancel_futures=True)
 if worker_fd is not None:os.close(worker_fd)
 if cid is not None:subprocess.run(d+['rm','-f',cid],timeout=40,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 if survivor is not None:subprocess.run(d+['rm','-f',survivor],timeout=40,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
