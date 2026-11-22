import hashlib,json,pathlib,urllib.request
base='https://registry-1.docker.io/v2/library/alpine/'
token=json.load(urllib.request.urlopen('https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/alpine:pull'))['token']
def get(path):
 req=urllib.request.Request(base+path,headers={'Authorization':'Bearer '+token,'Accept':'application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json'})
 with urllib.request.urlopen(req,timeout=30) as r: data=r.read()
 digest='sha256:'+hashlib.sha256(data).hexdigest()
 if path.split('/')[-1].startswith('sha256:'): assert path.endswith(digest)
 pathlib.Path(digest).write_bytes(data);return digest,data
for tag in ['3.19','3.20','3.21']:
 index,data=get('manifests/'+tag);pathlib.Path('index-'+tag).write_text(index);print(index)
 for desc in json.loads(data)['manifests']:
  p=desc['platform'];
  if p['os']!='linux' or p['architecture'] not in ('amd64','arm64'):continue
  digest,manifest=get('manifests/'+desc['digest']);m=json.loads(manifest)
  for blob in [m['config'],*m['layers']]:get('blobs/'+blob['digest'])
  print(p['architecture'], digest,flush=True)
