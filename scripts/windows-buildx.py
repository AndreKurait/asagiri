"""Build the Windows Docker driver with a valid logical gRPC authority (#100)."""
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def build(arch, destination, expected):
    commit = '0dbe87f36ed472fe36b6796c3f70b4210a731fc5'
    destination = Path(destination).resolve()
    with tempfile.TemporaryDirectory(dir=destination.parent) as work:
        source = Path(work) / 'src'
        subprocess.run(['git', 'clone', '--quiet', '--depth', '1', '--branch', 'v0.38.1',
                        '--config', 'core.autocrlf=false', 'https://github.com/docker/buildx.git', str(source)], check=True)
        revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=source, text=True).strip()
        if revision != commit:
            raise ValueError(f'Unexpected Buildx source revision {revision}')
        driver = source / 'driver/docker/driver.go'
        before = b'return client.New(ctx, "", opts...)'
        contents = driver.read_bytes()
        if contents.count(before) != 1:
            raise ValueError('Unexpected Buildx Docker driver implementation')
        # The existing dialer still opens the Docker engine pipe. The URL only
        # supplies the logical authority used in HTTP/2 requests on that pipe.
        driver.write_bytes(contents.replace(before, b'return client.New(ctx, "tcp://localhost", opts...)'))
        result = Path(work) / 'docker-buildx.exe'
        env = dict(os.environ, GOTOOLCHAIN='go1.26.10', GOOS='windows', GOARCH=arch,
                   GOAMD64='v1', GOARM64='v8.0', GOEXPERIMENT='', CGO_ENABLED='0',
                   GOFLAGS='-mod=vendor', GOWORK='off')
        package = 'github.com/docker/buildx/version'
        flags = f'-s -w -X {package}.Version=v0.38.1-asagiri.1 -X {package}.Revision={commit}'
        subprocess.run(['go', 'build', '-trimpath', '-buildvcs=false', '-ldflags', flags,
                        '-o', str(result), './cmd/buildx'], cwd=source, env=env, check=True)
        with result.open('rb') as stream:
            digest = hashlib.file_digest(stream, 'sha256').hexdigest()
        if digest != expected:
            raise ValueError(f'Windows {arch} Buildx SHA-256 {digest}, expected {expected}')
        os.replace(result, destination)


if __name__ == '__main__':
    build(*sys.argv[1:])
