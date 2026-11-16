"""Required cleanup must not block stop, unrelated exec/DNS, or reuse its paths early.

Requires Linux 6.8+, strace and a disposable privileged engine; injects a 15-second umount delay.
"""
import argparse,http.client,json,os,pathlib,platform,select,signal,socket,subprocess,tarfile,time,uuid
from concurrent.futures import ThreadPoolExecutor
from mount_identity import mount_id as statx_mount_id
p=argparse.ArgumentParser();p.add_argument('socket');p.add_argument('image');p.add_argument('--expect-blocked',action='store_true');p.add_argument('--copy',action='store_true');p.add_argument('--crash-agent',action='store_true');a=p.parse_args()
assert os.environ.get('ASAGIRI_TEST_ENGINE')=='1'
if a.crash_agent:
 assert os.environ.get('ASAGIRI_DATA_DIR')=='/test-store' and pathlib.Path('/.dockerenv').is_file()
 assert os.readlink('/proc/1/exe')=='/opt/asagiri/asagiri'
 assert not a.copy and not a.expect_blocked
d=['docker','-H','unix://'+a.socket]
def call(*args,timeout=30):return subprocess.check_output(d+list(args),timeout=timeout,stderr=subprocess.STDOUT)
def malformed_upload(cid):
 # A complete HTTP body with a bad tar checksum reaches the copy worker,
 # acquires its root, then fails. It cannot fail HTTP framing before the lease.
 header=tarfile.TarInfo('malformed-cleanup-probe').tobuf()
 c=http.client.HTTPConnection('localhost',timeout=40)
 c.sock=socket.socket(socket.AF_UNIX);c.sock.settimeout(40);c.sock.connect(a.socket)
 try:
  c.request('PUT','/containers/'+cid+'/archive?path=/',b'!'+header[1:],{'Content-Type':'application/x-tar','Connection':'close'})
  r=c.getresponse();return r.status,json.loads(r.read())
 finally:c.close()
ids=[];trace=None;stop=None;restart=None;worker_fd=None;network='cleanup-'+uuid.uuid4().hex[:8]
pool=ThreadPoolExecutor(max_workers=1)
try:
 call('network','create',network)
 for name in ('survivor','victim','stopper'):
  cid=call('run','-d','--network',network,'--network-alias',name,a.image,'sleep','120').decode().strip();ids.append(cid)
 survivor,victim,stopper=ids
 call('exec',victim,'sh','-c','printf preserved > /cleanup-proof')
 def exec_check(timeout=3):return call('exec',survivor,'true',timeout=timeout)
 def dns_check(timeout=3):return call('exec',survivor,'nslookup','survivor','127.0.0.11',timeout=timeout)
 exec_check();dns_check()
 init=[]
 for item in pathlib.Path('/proc').iterdir():
  if item.name.isdigit():
   try:
    fields=(item/'stat').read_text().rsplit(')',1)[1].split()
    if (item/'comm').read_text().strip()=='asagiri-init' and fields[0]!='Z' and int(fields[1])==1:init.append(int(item.name))
   except (OSError,ValueError):pass
 assert len(init)==1,init
 # Delay only this cleanup mount. Injecting every descendant's umount2 also
 # delays the restarted container's pivot-root teardown, doubling the fault
 # to 30 seconds and racing the client's independent 30-second deadline.
 roots={line.split()[4] for line in pathlib.Path('/proc',str(init[0]),'mountinfo').read_text().splitlines() if line.split()[4].endswith('/'+victim+'/rootfs')}
 assert len(roots)==1,roots
 rootfs=roots.pop()
 rootfs_in_agent='/proc/'+str(init[0])+'/root'+rootfs
 # Successful process exit and copy now retain an idle root. Drain any old
 # exit cleanup before tracing; only the failed upload below must retire it.
 call('stop','-t','0',victim);call('diff',victim)
 def mounts():
  return [line.split() for line in pathlib.Path('/proc',str(init[0]),'mountinfo').read_text().splitlines() if line.split()[4]==rootfs]
 log=open('/tmp/cleanup-delay.trace','wb')
 trace=subprocess.Popen(['strace','-f','-P',rootfs,'-e','trace=umount2','-e','inject=umount2:delay_enter=15s','-p',str(init[0])],stdout=log,stderr=log)
 deadline=time.monotonic()+10
 while time.monotonic()<deadline:
  status=pathlib.Path('/proc',str(init[0]),'status').read_text()
  if 'TracerPid:\t'+str(trace.pid)+'\n' in status:break
  time.sleep(.02)
 else:raise AssertionError('strace did not attach')
 # Establish responsiveness AFTER attaching, before injecting cleanup delay.
 exec_check();dns_check()
 began=time.monotonic();upload=pool.submit(malformed_upload,victim)
 blocked=None;deadline=time.monotonic()+5
 while time.monotonic()<deadline:
  for item in pathlib.Path('/proc').iterdir():
   if not item.name.isdigit():continue
   try:
    if 'TracerPid:\t'+str(trace.pid)+'\n' not in (item/'status').read_text():continue
    syscall=(item/'syscall').read_text().split()
    if syscall and syscall[0]=={'x86_64':'166','aarch64':'39'}[platform.machine()]:blocked=int(item.name);break
   except OSError:pass
  if blocked:break
  time.sleep(.01)
 assert blocked, 'No traced umount2 entry barrier observed'
 observed=time.monotonic();mounted=mounts();assert len(mounted)==1,mounted
 mount_id=mounted[0][0];worker_fd=os.pidfd_open(blocked)
 assert statx_mount_id(rootfs_in_agent,False)==int(mount_id)
 mount_unique_id=statx_mount_id(rootfs_in_agent,True)
 report={'schema':'asagiri.cleanup-responsive.v1','initPid':init[0],'blockedPid':blocked,'barrierSeconds':observed-began,'injectedDelaySeconds':15,'injectionPath':rootfs,'expectedBlocked':a.expect_blocked,'cleanupTrigger':'malformed archive','retiringMountId':mount_id,'retiringMountUniqueId':mount_unique_id}
 # Stop a genuinely running process while the victim's required cleanup is
 # blocked, rather than issuing an idempotent stop on the already exited ID.
 stop_at=time.monotonic();stop=subprocess.Popen(d+['stop','-t','0',stopper],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 started=time.monotonic()
 try:exec_check();report['unrelatedExec']='completed'
 except subprocess.TimeoutExpired:report['unrelatedExec']='timeout'
 report['unrelatedExecSeconds']=time.monotonic()-started
 if a.expect_blocked:assert report['unrelatedExec']=='timeout',report
 else:
  assert blocked!=init[0],report
  assert os.readlink('/proc/'+str(blocked)+'/ns/mnt')==os.readlink('/proc/'+str(init[0])+'/ns/mnt'), 'Worker must share parent mount namespace'
  report['sharedMountNamespace']=True
  assert not upload.done(),'Failed upload acknowledged before required cleanup'
  assert report['unrelatedExec']=='completed' and report['unrelatedExecSeconds']<3,report
  started=time.monotonic();dns_check();report['unrelatedDnsSeconds']=time.monotonic()-started
  assert report['unrelatedDnsSeconds']<3,report
  # stop is about the process, so it stays fast even if cleanup exceeds
  # the engine's ten-second post-SIGKILL timeout.
  out,err=stop.communicate(timeout=3);assert stop.returncode==0,(out,err)
  report['stopSeconds']=time.monotonic()-stop_at;assert report['stopSeconds']<5,report
  if a.crash_agent:
   # pidfds identify these exact processes even if the kernel reuses a PID.
   agent_fd=os.pidfd_open(init[0])
   try:
    fields=pathlib.Path('/proc',str(blocked),'stat').read_text().rsplit(')',1)[1].split()
    assert int(fields[1])==init[0] and os.readlink('/proc/'+str(blocked)+'/exe')=='/opt/asagiri/asagiri-init'
    signal.pidfd_send_signal(agent_fd,signal.SIGKILL)
    poll=select.poll();poll.register(agent_fd,select.POLLIN)
    assert poll.poll(3000),'Agent did not exit'
    poll=select.poll();poll.register(worker_fd,select.POLLIN)
    if not poll.poll(100):
     # strace can retain its injected syscall-entry delay even for SIGKILL
     # (a tracing stop with "delayed wait data set already"). Verify the
     # parent's death really sent SIGKILL before releasing our own tracer.
     status=dict(line.split(':',1) for line in pathlib.Path('/proc',str(blocked),'status').read_text().splitlines() if ':' in line)
     assert int(status['TracerPid'])==trace.pid,status
     pending=int(status['SigPnd'],16)|int(status['ShdPnd'],16)
     assert pending & (1 << (signal.SIGKILL-1)),'Parent death did not terminate the cleanup worker'
     report['deathSignalPendingWhileTraced']=True
    if trace.poll() is None:trace.kill()
    trace.wait(timeout=3);trace=None
    assert poll.poll(3000),'Cleanup worker did not exit after releasing the tracer'
    report['workerStoppedOnAgentLoss']=True
   finally:os.close(agent_fd)
  else:
   restart_at=time.monotonic()
   operation=['cp',victim+':/cleanup-proof','/tmp/cleanup-proof-result'] if a.copy else ['start',victim]
   restart=subprocess.Popen(d+operation,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
   time.sleep(.2);assert restart.poll() is None,'Restart bypassed cleanup barrier'
   assert not upload.done(),'Failed upload reply bypassed cleanup barrier'
   out,err=restart.communicate(timeout=30);assert restart.returncode==0,(out,err)
   report['lifecycleOperation']='copy' if a.copy else 'restart'
   report['lifecycleWaitSeconds']=time.monotonic()-restart_at
   assert report['lifecycleWaitSeconds']>9,report
   if a.copy:
    assert pathlib.Path('/tmp/cleanup-proof-result').read_bytes()==b'preserved'
    pathlib.Path('/tmp/cleanup-proof-result').unlink()
    report['copyBytesAndEof']='passed'
   poll=select.poll();poll.register(worker_fd,select.POLLIN)
   assert poll.poll(0),'Lifecycle operation completed before cleanup worker exited'
   # mountinfo IDs can be reused immediately after umount. The restarted
   # process or completed copy now retains a new overlay at this same path.
   # A boot-unique statx ID distinguishes it from the retired mount.
   replacement=mounts();assert len(replacement)==1,replacement
   replacement_id=statx_mount_id(rootfs_in_agent,False)
   assert replacement_id==int(replacement[0][0])
   replacement_unique_id=statx_mount_id(rootfs_in_agent,True)
   assert replacement_unique_id!=mount_unique_id,'Retired overlay still mounted after lifecycle completion'
   report.update(replacementMountId=str(replacement_id),replacementMountUniqueId=replacement_unique_id,mountIdReused=replacement_id==int(mount_id))
   report['cleanupWorkerAndMountDrained']=True
 if a.expect_blocked:
  out,err=stop.communicate(timeout=25);report['stopSeconds']=time.monotonic()-stop_at;assert stop.returncode==0,(out,err)
 status,body=upload.result(timeout=30)
 assert status==(500 if a.crash_agent else 400) and body.get('message'),(status,body)
 if not a.crash_agent:assert 'checksum' in body['message'].lower(),body
 report['uploadFailure']={'status':status,'message':body['message']}
 if trace is not None:
  if trace.poll() is None:trace.send_signal(signal.SIGINT)
  trace.wait(timeout=10);trace=None
 log.close()
 call('start',victim);assert call('exec',victim,'echo','restarted').strip()==b'restarted'
 assert call('exec',victim,'cat','/cleanup-proof')==b'preserved'
 call('rm','-f',victim);ids.remove(victim)
 deadline=time.monotonic()+30
 while pathlib.Path(rootfs).parent.exists():
  assert time.monotonic()<deadline,'Removed container tree did not drain';time.sleep(.05)
 report['removedTreeAbsent']=True
 report['sameIdRestartAndRemove']='passed';print(json.dumps(report,indent=2),flush=True)
finally:
 if trace is not None:trace.send_signal(signal.SIGINT);trace.wait(timeout=10)
 if stop is not None and stop.poll() is None:stop.communicate(timeout=30)
 if restart is not None and restart.poll() is None:restart.communicate(timeout=30)
 pool.shutdown(wait=True,cancel_futures=True)
 if worker_fd is not None:os.close(worker_fd)
 for cid in ids:subprocess.run(d+['rm','-f',cid],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=30)
 subprocess.run(d+['network','rm',network],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=30)
