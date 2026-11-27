#!/usr/bin/env python3
"""Collect the Debian sources for both packaged guest checkpoint runtimes."""
import argparse
from email.parser import Parser
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile

from build import ARCHES, SNAPSHOT


ROOT = Path(__file__).resolve().parents[2]


def verify_sources(path, versions):
    """Match every source version and payload to its Debian source descriptor."""
    found = set()
    with tarfile.open(path) as archive:
        members = {m.name: m for m in archive if m.isfile() and m.name.startswith('./debian/')}
        verified = set()
        for name, member in members.items():
            if not name.endswith('.dsc'):
                continue
            text = archive.extractfile(member).read().decode('utf-8')
            if text.startswith('-----BEGIN PGP SIGNED MESSAGE-----'):
                text = text.split('\n\n', 1)[1].split('\n-----BEGIN PGP SIGNATURE-----', 1)[0]
            desc = Parser().parsestr(text)
            version = f"{desc['Source']}={desc['Version']}"
            if version in found or not desc.get('Checksums-Sha256', '').strip():
                raise ValueError(f'incomplete or duplicate source descriptor: {name}')
            found.add(version)
            verified.add(name)
            for row in desc['Checksums-Sha256'].strip().splitlines():
                expected, size, basename = row.split()
                payload = members['./debian/' + basename]
                if payload.size != int(size) or hashlib.file_digest(archive.extractfile(payload), 'sha256').hexdigest() != expected:
                    raise ValueError(f'source payload differs from {name}: {basename}')
                verified.add(payload.name)
        if found != versions or verified != members.keys():
            raise ValueError(f'source archive differs from inventory: {found ^ versions}, {members.keys() - verified}')


def collect(arm64, x86_64, output):
    versions = set()
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output.parent) as tmp:
        work = Path(tmp)
        inputs = work / 'inputs.tar'
        with tarfile.open(inputs, 'w') as inputs_tar:
            hashes = {}
            for arch, runtime in [('arm64', arm64), ('x86_64', x86_64)]:
                with runtime.open('rb') as file:
                    hashes[arch] = hashlib.file_digest(file, 'sha256').hexdigest()
                    file.seek(0)
                    with tarfile.open(fileobj=file) as archive:
                        manifest = json.load(archive.extractfile('./packages.json'))
                if manifest['architecture'] != ARCHES[arch][0] or manifest['snapshot'] != SNAPSHOT or not manifest['packages']:
                    raise ValueError(f'expected a {SNAPSHOT} {arch} checkpoint package inventory')
                for package in manifest['packages']:
                    name, version = package['source'], package['source_version']
                    if not re.fullmatch(r'[a-z0-9][a-z0-9+.-]+', name) or not re.fullmatch(r'[0-9][A-Za-z0-9.+:~\-]*', version):
                        raise ValueError(f'invalid Debian source specification: {name}={version}')
                    versions.add(f'{name}={version}')
                inventory = work / f'{arch}-packages.json'
                inventory.write_text(json.dumps(manifest, indent=2) + '\n')
                inputs_tar.add(inventory, arcname=inventory.name)
            digests = work / 'runtime-sha256.json'
            digests.write_text(json.dumps(hashes, indent=2) + '\n')
            inputs_tar.add(digests, arcname=digests.name)
            for path in ['packaging/checkpoint/build.py', 'packaging/checkpoint/source.py', 'packaging/README.md', 'scripts/ci/images.json']:
                inputs_tar.add(ROOT / path, arcname=path)

        # Some installed libraries come from the pinned trixie base image. Query
        # its signed source indexes as well as sid's for each exact source version.
        setup = '''set -eu
mkdir -p /work/debian
cd /work
tar -xf -
rm -f /etc/apt/sources.list.d/debian.sources
for spec in debian:sid debian:trixie debian:trixie-updates debian-security:trixie-security; do
    printf 'deb-src [check-valid-until=no signed-by=/usr/share/keyrings/debian-archive-keyring.pgp] http://snapshot.debian.org/archive/%s/%s/ %s main\\n' "${spec%:*}" "$1" "${spec#*:}"
done >/etc/apt/sources.list
shift
apt-get -o APT::Update::Error-Mode=any -o Acquire::Retries=2 update >&2
chown _apt /work/debian
cd /work/debian
apt-get -o Acquire::Retries=2 --only-source --download-only source "$@" >&2
sha256sum -- * >../debian-SHA256SUMS
cd /work
tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner -cf - .
'''
        image = json.loads((ROOT / 'scripts/ci/images.json').read_text())['guest']['image']
        staged = work / 'source.tar'
        with inputs.open('rb') as meta, staged.open('wb') as out:
            subprocess.run(['docker', 'run', '--rm', '-i', image, 'sh', '-c', setup, 'sources', SNAPSHOT,
                            *sorted(versions)], stdin=meta, stdout=out, check=True)
        verify_sources(staged, versions)
        os.link(staged, output)
    print(f'{output}: {len(versions)} Debian source package versions')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--arm64', type=Path, required=True)
    parser.add_argument('--x86-64', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    collect(args.arm64, args.x86_64, args.output)
