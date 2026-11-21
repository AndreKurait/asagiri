import uuid,os,sys,http.client,socket,hashlib,io,json,subprocess,tarfile
assert os.environ.get('ASAGIRI_TEST_ENGINE') == '1'
path = sys.argv[1]
tag = 'asagiri-oci-' + uuid.uuid4().hex + ':test'
cli=['docker','-H','unix://'+path]; files={}
def blob(value,kind):
 data=json.dumps(value).encode(); digest=hashlib.sha256(data).hexdigest();files['blobs/sha256/'+digest]=data
 return dict(digest='sha256:'+digest,size=len(data),mediaType='application/vnd.oci.image.'+kind+'.v1+json')
for arch in ('arm64','amd64'):
 cfg=blob(dict(os='linux',architecture=arch,rootfs=dict(type='layers',diff_ids=[])),'config'); m=blob(dict(schemaVersion=2,config=cfg,layers=[]),'manifest'); m['platform']=dict(os='linux',architecture=arch); m['annotations']={'org.opencontainers.image.ref.name':tag};files.setdefault('index.json',[]).append(m)
files['index.json']=json.dumps(dict(schemaVersion=2,manifests=files['index.json'])).encode();files['oci-layout']=b'{"imageLayoutVersion":"1.0.0"}'
with io.BytesIO() as buf:
 with tarfile.open(fileobj=buf,mode='w') as t:
  for name,data in files.items():
   entry=tarfile.TarInfo(name);entry.size=len(data);t.addfile(entry,io.BytesIO(data))
 archive=buf.getvalue()
try:
 subprocess.run(cli+['load'],input=archive,check=True)
 host_arch = {'aarch64': 'arm64', 'arm64': 'arm64', 'x86_64': 'amd64'}[os.uname().machine]
 assert subprocess.check_output(cli+['image','inspect',tag,'--format','{{.Architecture}}'],text=True,timeout=15).strip()==host_arch
 print('PASS OCI CLI load preferred host platform for a shared tag', host_arch)
 for arch in ('amd64','arm64'):
  with socket.socket(socket.AF_UNIX) as stream:
   stream.connect(path)
   stream.sendall(f'POST /v1.47/images/load?platform=linux%2F{arch} HTTP/1.1\r\nHost: docker\r\nContent-Length: {len(archive)}\r\n\r\n'.encode()+archive)
   reply=http.client.HTTPResponse(stream);reply.begin();body=reply.read();assert reply.status==200 and b'Loaded image:' in body, (reply.status,body)
  assert subprocess.check_output(cli+['image','inspect',tag,'--format','{{.Architecture}}'],text=True).strip()==arch
  print('PASS OCI API load selected',arch)
 for arch in ('amd64','arm64'):
  subprocess.run(cli+['load','--platform','linux/'+arch],input=archive,check=True,timeout=30)
  assert subprocess.check_output(cli+['image','inspect',tag,'--format','{{.Architecture}}'],text=True,timeout=15).strip()==arch
  print('PASS OCI CLI load selected',arch)
finally:
 subprocess.run(cli+["image","rm",tag],capture_output=True,timeout=15)


def check_cli_variant(docker_format):
    """Load a single named ARM variant through the negotiated Docker API."""
    test_tag = f"asagiri-variant-{uuid.uuid4().hex}:test"
    members = {}
    descriptors = []
    for variant in ("v7", "v6"):
        config = json.dumps({
            "os": "linux", "architecture": "arm", "variant": variant,
            "rootfs": {"type": "layers", "diff_ids": []},
        }).encode()
        if docker_format:
            path = f"{variant}.json"
            members[path] = config
            descriptors.append({"Config": path, "RepoTags": [test_tag], "Layers": []})
            continue

        def add_blob(data, kind):
            digest = hashlib.sha256(data).hexdigest()
            members[f"blobs/sha256/{digest}"] = data
            return {
                "digest": f"sha256:{digest}", "size": len(data),
                "mediaType": f"application/vnd.oci.image.{kind}.v1+json",
            }

        manifest = json.dumps({
            "schemaVersion": 2, "config": add_blob(config, "config"), "layers": [],
        }).encode()
        descriptor = add_blob(manifest, "manifest")
        descriptor["annotations"] = {"org.opencontainers.image.ref.name": test_tag}
        descriptor["platform"] = {"os": "linux", "architecture": "arm", "variant": variant}
        descriptors.append(descriptor)

    if docker_format:
        members["manifest.json"] = json.dumps(descriptors).encode()
    else:
        members["index.json"] = json.dumps({"schemaVersion": 2, "manifests": descriptors}).encode()
        members["oci-layout"] = b'{"imageLayoutVersion":"1.0.0"}'

    with io.BytesIO() as buf:
        with tarfile.open(fileobj=buf, mode="w") as tar:
            for name, data in members.items():
                entry = tarfile.TarInfo(name)
                entry.size = len(data)
                tar.addfile(entry, io.BytesIO(data))
        payload = buf.getvalue()

    try:
        missing = subprocess.run(
            cli + ["load", "--platform", "linux/arm/v5"], input=payload,
            capture_output=True, timeout=30,
        )
        assert missing.returncode and b"linux/arm/v5" in missing.stderr, missing
        absent = subprocess.run(cli + ["image", "inspect", test_tag], capture_output=True, timeout=15)
        assert absent.returncode, absent.stdout
        subprocess.run(cli + ["load", "--platform", "linux/arm/v7"], input=payload, check=True, timeout=30)
        image = json.loads(subprocess.check_output(cli + ["image", "inspect", test_tag], timeout=15))[0]
        assert (image["Os"], image["Architecture"], image["Variant"]) == ("linux", "arm", "v7"), image
        print("PASS Docker CLI selected ARM v7 from", "Docker archive" if docker_format else "OCI layout")
    finally:
        subprocess.run(cli + ["image", "rm", test_tag], capture_output=True, timeout=15)


check_cli_variant(docker_format=False)
check_cli_variant(docker_format=True)
