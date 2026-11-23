import http.server,json,pathlib,re
class Registry(http.server.BaseHTTPRequestHandler):
 def do_GET(self): self.reply(True)
 def do_HEAD(self): self.reply(False)
 def reply(self,body):
  path=self.path.split('?')[0];match=re.fullmatch(r'/v2/library/alpine/(manifests|blobs)/(3\.(?:19|20|21)|sha256:[0-9a-f]{64})',path)
  if path=='/v2/': data=b'{}';mime='application/json';digest=None
  elif match:
   kind,ref=match.groups();digest=pathlib.Path('index-'+ref).read_text() if ref.startswith('3.') else ref; f=pathlib.Path(digest)
   if not f.is_file(): self.send_error(404);return
   data=f.read_bytes();mime=json.loads(data)['mediaType'] if kind=='manifests' else 'application/octet-stream'
  else:self.send_error(404);return
  self.send_response(200);self.send_header('Content-Type',mime);self.send_header('Content-Length',str(len(data)));self.send_header('Docker-Distribution-Api-Version','registry/2.0')
  if digest:self.send_header('Docker-Content-Digest',digest)
  self.end_headers()
  if body:self.wfile.write(data)
http.server.ThreadingHTTPServer(('127.0.0.1',49102),Registry).serve_forever()
