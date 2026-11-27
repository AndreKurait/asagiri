#!/usr/bin/env python3
"""Stage a Debian checkpoint runtime for cargo xtask guest --arch ARCH."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


# This is the archive date used to build the guest image in scripts/ci/images.json.
SNAPSHOT = '20261104T212339Z'
ARCHES = {'arm64': ('arm64', 'aarch64-linux-gnu', 'lib/ld-linux-aarch64.so.1'),
          'x86_64': ('amd64', 'x86_64-linux-gnu', 'lib64/ld-linux-x86-64.so.2')}


def stage(arch):
    deb, triplet, loader = ARCHES[arch]
    installed = subprocess.check_output(['dpkg', '--print-architecture'], text=True).strip()
    if deb != installed:
        raise RuntimeError(f'expected {deb} Debian image, got {installed}')
    root = Path('/tmp/checkpoint')
    root.mkdir()
    owners = set()

    def copy(source, destination):
        target = root / destination
        if target.exists():
            return
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
        target.chmod(0o755)
        owners.add(Path(source).resolve())

    def executable(source, destination, libdir):
        copy(source, destination)
        deps = subprocess.check_output(['ldd', str(source)], text=True)
        if 'not found' in deps:
            raise RuntimeError(f'unresolved dependency for {source}: {deps}')
        for path in re.findall(r'(?:=>\s+|^\s*)(/\S+)\s+\(', deps, re.MULTILINE):
            name = Path(path).name
            if name != Path(loader).name:
                copy(path, libdir / name)

    executable('/usr/sbin/criu', 'usr/sbin/criu', Path('usr/lib') / triplet)
    copy('/' + loader, loader)
    helpers = Path('helpers')
    copy('/' + loader, helpers / loader)
    libs = helpers / 'lib' / triplet
    for command in ['tar', 'gzip', 'sh', 'ip', 'iptables', 'ip6tables', 'iptables-save',
                    'ip6tables-save', 'iptables-restore', 'ip6tables-restore']:
        path = shutil.which(command)
        if not path:
            raise RuntimeError(f'missing {command}')
        executable(path, helpers / Path(path).relative_to('/'), libs)
        if command in ('sh', 'tar', 'gzip'):
            for prefix in ('bin', 'usr/bin'):
                copy(path, helpers / prefix / command)
    for plugin in sorted(Path('/usr/lib', triplet, 'xtables').glob('*.so')):
        executable(plugin, helpers / plugin.relative_to('/'), libs)
    # Record owners of staged files (Debian's merged-/usr packages can record
    # either spelling), source versions and the copyright notices from the .debs.
    packages = set()
    for path in sorted(owners):
        spellings = [str(path)]
        if str(path).startswith('/usr/'):
            spellings.append(str(path)[4:])
        for spelling in spellings:
            owner = subprocess.run(['dpkg-query', '-S', spelling], capture_output=True, text=True)
            if owner.returncode == 0:
                packages.add(owner.stdout.split(': ', 1)[0])
                break
        else:
            raise RuntimeError(f'no Debian package owns {path}')
    records = []
    for package in sorted(packages):
        fields = subprocess.check_output(['dpkg-query', '-W', '-f=${Package} ${Version} ${source:Package} ${source:Version}', package], text=True).split()
        binary, version, source, source_version = fields
        copy(Path('/usr/share/doc', binary, 'copyright'), Path('licenses', binary, 'copyright'))
        records.append(dict(package=binary, version=version, source=source, source_version=source_version))
    (root / 'packages.json').write_text(json.dumps(dict(snapshot=SNAPSHOT, architecture=deb, packages=records), indent=2) + '\n')
    subprocess.run(['tar', '--sort=name', '--format=ustar', '--mtime=@0', '--owner=0', '--group=0',
                    '--numeric-owner', '-C', str(root), '-cf', '-', '.'], check=True)


def build(arch, output):
    recipe = Path(__file__).resolve()
    image = json.loads((recipe.parents[2] / 'scripts/ci/images.json').read_text())['guest']['image']
    if output.exists():
        raise FileExistsError(f'choose a new output file: {output}')
    output.parent.mkdir(parents=True, exist_ok=True)
    # APT authenticates the fixed historical indexes with the base image's archive
    # keyring. Expiry is disabled for these dated snapshots only.
    setup = '''set -eu
rm -f /etc/apt/sources.list.d/debian.sources
printf 'deb [check-valid-until=no signed-by=/usr/share/keyrings/debian-archive-keyring.pgp] http://snapshot.debian.org/archive/debian/%s/ sid main\\n' "$1" >/etc/apt/sources.list
apt-get -o APT::Update::Error-Mode=any -o Acquire::Retries=2 update >&2
DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends criu=4.2.1-1 iproute2 iptables python3 gzip tar dash >&2
exec python3 - --stage "$2"
'''
    cmd = ['docker', 'run', '--rm', '-i', '--platform', 'linux/' + ARCHES[arch][0], image,
           'sh', '-c', setup, 'checkpoint', SNAPSHOT, arch]
    with tempfile.TemporaryDirectory(dir=output.parent) as tmp:
        archive = Path(tmp) / 'runtime.tar'
        with archive.open('wb') as out:
            subprocess.run(cmd, input=recipe.read_bytes(), stdout=out, check=True)
        # Publish the complete archive without overwriting a concurrent build. Linux
        # plugin names such as libxt_mark.so and libxt_MARK.so must stay distinct.
        os.link(archive, output)
    print(output)


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--stage':
        stage(sys.argv[2])
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument('--arch', choices=ARCHES, required=True)
        parser.add_argument('--output', type=Path, required=True)
        args = parser.parse_args()
        build(args.arch, args.output.resolve())
