#!/usr/bin/env python3
"""Docker reference/conformance: image labels in disk usage and at create.
Unique zero-layer images and never-started --network none containers only.
"""
import hashlib,http.client,io,json,os,re,socket,sys,tarfile,uuid
from pathlib import Path

def load_image(request, tag, config):
 blob=json.dumps(config).encode();filename=hashlib.sha256(blob).hexdigest()+'.json';archive=io.BytesIO()
 with tarfile.open(fileobj=archive,mode='w') as tar:
  for name,data in ((filename,blob),('manifest.json',json.dumps([{'Config':filename,'RepoTags':[tag],'Layers':[]}]).encode())):
   item=tarfile.TarInfo(name);item.size=len(data);tar.addfile(item,io.BytesIO(data))
 status,result=request('POST','/images/load',raw=archive.getvalue());assert status==200,(status,result)

def verify_images(request, images):
 ids={}
 for tag,expected in images.items():
  status,image=request('GET','/images/'+tag+'/json')
  assert status==200 and image['Config'].get('Labels')==expected,(tag,status,image.get('Config'))
  ids[image['Id']]=expected
 status,listed=request('GET','/images/json?all=1');assert status==200,status
 for iid,expected in ids.items():
  rows=[row for row in listed if row['Id']==iid]
  assert rows and all(row['Labels']==expected for row in rows),('image list',iid,rows,expected)
 status,usage=request('GET','/system/df');assert status==200,status
 for iid,expected in ids.items():
  rows=[row for row in usage['Images'] if row['Id']==iid]
  observed=[row.get('Labels') for row in rows]
  print('system df image labels '+json.dumps({'id':iid,'expected':expected,'observed':observed}),flush=True)
  assert rows and all(row['Labels']==expected for row in rows),('system df',iid,observed,expected)
 print('PASS labelled and unlabelled images agree across inspect, list and system df',flush=True)

def check(path, persist=None):
 def request(method,target,body=None,raw=None):
  data=raw if raw is not None else json.dumps(body).encode() if body is not None else b''
  connection=http.client.HTTPConnection('localhost',timeout=30)
  connection.sock=socket.socket(socket.AF_UNIX);connection.sock.settimeout(30);connection.sock.connect(path)
  try:
   connection.request(method,target,body=data,headers={'Content-Type':'application/x-tar' if raw is not None else 'application/json','Connection':'close'})
   reply=connection.getresponse();payload=reply.read();return reply.status,json.loads(payload) if payload and raw is None else payload
  finally:connection.close()
 unique=uuid.uuid4().hex;tag='asagiri-label-inheritance-'+unique+':test';containers=[];expected_rows={};retained=False
 config={'architecture':'amd64','os':'linux','comment':unique,'config':{'Cmd':['true'],'Labels':{'fixture.owner':'image','shared':'image'}},'rootfs':{'type':'layers','diff_ids':[]}}
 unlabelled='asagiri-label-inheritance-'+unique+':unlabelled'
 images={tag:config['config']['Labels'],unlabelled:None}
 try:
  load_image(request,tag,config)
  load_image(request,unlabelled,dict(config,config={'Cmd':['true']}))
  verify_images(request,images)
  cases=[('omitted',{},{}),('null',{'Labels':None},{}),('empty',{'Labels':{}},{}),('overrides',{'Labels':{'shared':'container','new':'explicit'}},{'shared':'container','new':'explicit'}),('empty-value',{'Labels':{'shared':''}},{'shared':''}),('null-value',{'Labels':{'shared':None}},{'shared':''})]
  for case,fields,overrides in cases:
   status,result=request('POST','/containers/create?name=asagiri-label-'+unique+'-'+case,dict(Image=tag,HostConfig={'NetworkMode':'none'},**fields));assert status==201,(case,status,result)
   cid=result['Id'];containers.append(cid);status,actual=request('GET','/containers/'+cid+'/json');expected=dict(config['config']['Labels'],**overrides)
   assert status==200 and actual['Config']['Labels']==expected,(case,actual.get('Config',{}).get('Labels'),expected)
   filters=json.dumps({'id':[cid],'label':['fixture.owner=image']},separators=(',',':'))
   from urllib.parse import quote
   status,listed=request('GET','/containers/json?all=1&filters='+quote(filters,safe=''))
   assert status==200 and len(listed)==1 and listed[0]['Id']==cid and listed[0]['Labels']==expected,(case,status,listed)
   expected_rows[cid]=expected
   print('PASS image label inheritance '+case,flush=True)
  for value in ('invalid',{'key':1}):
   status,result=request('POST','/containers/create?name=asagiri-label-'+unique+'-invalid',{'Image':tag,'Labels':value,'HostConfig':{'NetworkMode':'none'}})
   if status==201:containers.append(result['Id'])
   assert status==400,(value,status,result)
  print('PASS malformed labels rejected',flush=True)
  if persist is not None:
   status,info=request('GET','/info');assert status==200 and info.get('ID'),(status,info)
   fd=os.open(persist,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)
   with os.fdopen(fd,'w') as out:json.dump({'engine':info['ID'],'tag':tag,'images':images,'containers':expected_rows},out)
   retained=True
   print('PASS metadata fixtures retained for daemon restart',flush=True)
 finally:
  if not retained:
   for cid in containers:
    status,result=request('DELETE','/containers/'+cid+'?force=true');assert status in (204,404),(cid,status,result)
   for image in images:
    status,result=request('DELETE','/images/'+image);assert status in (200,404),(status,result)
def verify_restart(path, receipt):
 from urllib.parse import quote
 data=json.loads(Path(receipt).read_bytes());tag=data['tag'];rows=data['containers'];images=data['images']
 assert re.fullmatch(r'asagiri-label-inheritance-[0-9a-f]{32}:test',tag)
 assert len(rows)==6 and all(re.fullmatch(r'[0-9a-f]{64}',cid) for cid in rows)
 assert set(images)=={tag,tag.removesuffix(':test')+':unlabelled'}
 def request(method,target):
  connection=http.client.HTTPConnection('localhost',timeout=30)
  connection.sock=socket.socket(socket.AF_UNIX);connection.sock.settimeout(30);connection.sock.connect(path)
  try:
   connection.request(method,target,headers={'Connection':'close'});reply=connection.getresponse();payload=reply.read()
   return reply.status,json.loads(payload) if payload else None
  finally:connection.close()
 status,info=request('GET','/info');assert status==200 and info['ID']==data['engine'],'Restart changed engine identity'
 try:
  verify_images(request,images)
  for cid,expected in rows.items():
   status,actual=request('GET','/containers/'+cid+'/json')
   assert status==200 and actual['Config']['Image']==tag and actual['State']['Status']=='created' and actual['Config']['Labels']==expected,(cid,status,actual)
   filters=quote(json.dumps({'id':[cid],'label':['fixture.owner=image']}),safe='')
   status,listed=request('GET','/containers/json?all=1&filters='+filters)
   assert status==200 and len(listed)==1 and listed[0]['Id']==cid and listed[0]['Labels']==expected,(cid,status,listed)
  print('PASS all six inherited/overridden label maps and filters survive daemon restart',flush=True)
 finally:
  for cid in rows:
   status,result=request('DELETE','/containers/'+cid+'?force=true');assert status in (204,404),(cid,status,result)
  for image in images:
   status,result=request('DELETE','/images/'+image);assert status in (200,404),(status,result)
if __name__=='__main__':
 if len(sys.argv)==2:check(sys.argv[1])
 elif len(sys.argv)==4 and sys.argv[2]=='--prepare-restart':check(sys.argv[1],sys.argv[3])
 elif len(sys.argv)==4 and sys.argv[2]=='--verify-restart':verify_restart(sys.argv[1],sys.argv[3])
 else:raise SystemExit('Usage: image-label-inheritance.py SOCKET [--prepare-restart|--verify-restart RECEIPT]')
