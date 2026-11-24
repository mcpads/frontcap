"""Build frozen native-test sources without trusting timestamp-based workspace cache reuse.

Snapshot archives intentionally have zero mtimes. Keep registry dependencies cached, but clean
workspace packages before every build and bind the resulting binaries to verified source bytes.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import shutil


def build(source, request, target, binaries, jobs, report, examples=(), features=()):
    source = source.resolve(strict=True)
    target = target.resolve()
    manifest = json.loads(request.read_text())['source']
    for name, expected in manifest['files'].items():
        path = source/name
        if (not path.resolve().is_relative_to(source) or path.is_symlink()
                or hashlib.sha256(path.read_bytes()).hexdigest() != expected):
            raise ValueError('Frozen source differs: '+name)
    target.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, CARGO_TARGET_DIR=str(target))
    with (target/'frontcap-build.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        metadata = json.loads(subprocess.check_output(
            ['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'],
            cwd=source, env=env, text=True))
        member_ids = set(metadata['workspace_members'])
        members = [p['name'] for p in metadata['packages'] if p['id'] in member_ids]
        clean = ['cargo', 'clean']
        for member in members:
            clean += ['--package', member]
        subprocess.run(clean, cwd=source, env=env, check=True)
        command = ['cargo', 'build', '--locked', '-j', str(jobs)]
        for feature in features:
            command += ['--features', feature]
        for binary in binaries:
            command += ['--bin', binary]
        for example in examples:
            command += ['--example', example]
        subprocess.run(command, cwd=source, env=env, check=True)
        frozen = report.parent/'binaries'
        frozen.mkdir(exist_ok=False)
        for name in binaries:
            shutil.copy2(target/'debug'/name, frozen/name)
        for name in examples:
            shutil.copy2(target/'debug/examples'/name, frozen/name)
        identities = {name: hashlib.sha256((frozen/name).read_bytes()).hexdigest()
                      for name in [*binaries, *examples]}
        report.write_text(json.dumps(dict(source_sha256=manifest['archive_sha256'],
            workspace_packages=members, binaries=identities, workspace_cleaned=True, features=list(features)), indent=2)+'\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('source', 'request', 'target', 'report'):
        parser.add_argument(name, type=Path)
    parser.add_argument('--bin', dest='binaries', action='append', required=True)
    parser.add_argument('--example', dest='examples', action='append', default=[])
    parser.add_argument('--features', action='append', default=[])
    parser.add_argument('--jobs', type=int, default=4)
    args = parser.parse_args()
    names = [*args.binaries, *args.examples]
    if args.jobs < 1 or len(set(names)) != len(names) or any(Path(name).name != name or name in ('.', '..') for name in names):
        parser.error('Expected positive jobs and plain binary names')
    build(args.source, args.request, args.target, args.binaries, args.jobs, args.report, args.examples, args.features)
