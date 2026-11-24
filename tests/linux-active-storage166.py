import subprocess as s, pathlib,tempfile,time,os,json
lab=pathlib.Path(tempfile.mkdtemp(dir='/tmp',prefix='as166-api-'));home=lab/'home';home.mkdir();sock=lab/'s';dest=lab/'target';dest.mkdir();env=dict(os.environ,HOME=str(home),CFFIXED_USER_HOME=str(home),ASAGIRI_SOCKET=str(sock));env.pop('ASAGIRI_DATA_DIR',None)
def api(path,body=None):
 args=['curl','-fsS','--max-time','4','--unix-socket',str(sock),'http://localhost'+path];return json.loads(s.check_output(args+(['-H','Content-Type: application/json','-d',json.dumps(body)] if body is not None else []),stderr=s.DEVNULL))
env['ASAGIRI_TEST_ENGINE']='1'
def docker(*args):return s.check_output(['docker','-H','unix://'+str(sock),*args],text=True,timeout=90).strip()
p=s.Popen(['/tmp/storage-bin/asagiri','daemon'],env=env,stdout=(lab/'log').open('w'),stderr=s.STDOUT)
try:
 for i in range(80):
  if sock.is_socket():break
  time.sleep(.1)
 before=api('/asagiri/storage');assert before['state']=='ok' and str(home) in before['path'],before
 assert api('/volumes/create',{'Name':'storage166'})['Name']=='storage166'
 print(docker('load','-i','/tmp/as166-alpine.tar'));cid=docker('run','-d','--network','none','-v','storage166:/proof','alpine:3.21','sh','-c','exec sleep 600');docker('exec',cid,'sh','-c','echo roundtrip > /proof/file');assert docker('exec',cid,'cat','/proof/file')=='roundtrip'
 print('check',api('/asagiri/storage/check',{'path':str(dest)}));print('start',api('/asagiri/storage',{'path':str(dest)}))
 for i in range(150):
  try: after=api('/asagiri/storage')
  except (ValueError,s.CalledProcessError):time.sleep(.2);continue
  if after['error']:raise RuntimeError(after['error'])
  if after['path']==str(dest) and after['state']=='ok':break
  time.sleep(.2)
 assert after['path']==str(dest) and after['custom'],after;assert api('/volumes/storage166')['Name']=='storage166';print('volume recovered',after['path'])
 for i in range(60):
  if docker('inspect','-f','{{.State.Running}}',cid)=='true':break
  time.sleep(.5)
 assert docker('exec',cid,'cat','/proof/file')=='roundtrip';print('running container recovered')
 print('reset', api('/asagiri/storage/reset',{}))
 for i in range(150):
  try: restored=api('/asagiri/storage')
  except (ValueError,s.CalledProcessError):time.sleep(.2);continue
  if restored['error']:raise RuntimeError(restored['error'])
  if restored['path']==before['path'] and restored['state']=='ok':break
  time.sleep(.2)
 assert restored['path']==before['path'] and not restored['custom'] and restored['old']==str(dest),restored
 assert api('/volumes/storage166')['Name']=='storage166'
 cleaned=json.loads(s.check_output(['curl','-fsS','-X','DELETE','--unix-socket',str(sock),'http://localhost/asagiri/storage/old']))
 assert cleaned['old'] is None and not dest.exists() and pathlib.Path(before['path']).is_dir(),cleaned
 assert api('/volumes/storage166')['Name']=='storage166'; print('Reset and cleanup passed',restored['path'])
finally:
 if p.poll() is None:p.terminate()
 p.wait(timeout=20);print('fixture',lab)
