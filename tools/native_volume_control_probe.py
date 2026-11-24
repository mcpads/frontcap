"""Opt-in actual ENOSPC on an isolated volume worker control filesystem.

No app or shared filesystem is filled. Successful runs retain only a compact summary.
Failure preserves the original owners and records for inspection; never replay the run.
"""
import argparse
import json
import os
from pathlib import Path
import plistlib
import select
import shutil
import subprocess
import sys

from native_command_capture import capture
from native_owned_volume import OwnedVolume
from native_storage_volume_probe import write_until_full, digest
from native_owned_volume_worker import run


def probe(output):
    output.mkdir(parents=True, exist_ok=False)
    root = output/'run'
    root.mkdir()
    summary = dict(passed=False)
    child = None
    number = 0

    def command(argv):
        nonlocal number
        number += 1
        directory = root/f'command-{number}'
        result = capture(argv, directory, timeout=90, output_limit=1024*1024)
        if result['error'] or not result['child_retired']:
            raise RuntimeError('Original volume command unresolved: '+str(directory))
        return (directory/'stdout').read_bytes()

    def reply():
        if not select.select([child.stdout], [], [], 90)[0]:
            raise TimeoutError('Original worker reply unavailable; preserve original process')
        line = child.stdout.readline(4097)
        if not line.endswith(b'\n') or len(line) > 4096:
            raise RuntimeError('Original worker reply invalid')
        return json.loads(line)

    try:
        outer = OwnedVolume.create(root/'control-volume', 512, command)
        mount = outer.attach()
        assert mount.stat().st_dev != root.stat().st_dev
        worker = Path(__file__).with_name('native_owned_volume_worker.py').resolve()
        directory = mount/'worker'
        summary.update(worker_sha256=digest(worker), probe_sha256=digest(Path(__file__)))
        child = subprocess.Popen([sys.executable, '-B', str(Path(__file__).resolve()),
                                  str(directory), '--target-volume', str(root/'target-volume')],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        summary['worker_pid'] = child.pid
        child.stdin.write(b'1'); child.stdin.flush()
        ready = reply()
        assert ready['owner_pid'] == child.pid
        inner_mount = Path(ready['mount'])
        assert inner_mount.stat().st_dev != mount.stat().st_dev
        # Only the outer control filesystem is exhausted; the target has no live writers.
        filler = mount/'fill'
        write_until_full(filler, limit_mib=512)
        summary.update(filler_bytes=filler.stat().st_size,
                       control_free_at_failure=os.statvfs(mount).f_bavail*os.statvfs(mount).f_frsize)
        child.stdin.write(b'{"operation":"retire","writers_retired":true,"remove":true}\n')
        child.stdin.flush()
        result = reply()
        assert result == dict(passed=True, detached=True, removed=True)
        assert child.wait(timeout=30) == 0
        child.stdin.close(); child.stdout.close()
        assert json.loads((directory/'retirement.json').read_text()) == result
        assert json.loads((directory/'state.json').read_text())['state'] == 'retired'
        assert not inner_mount.exists() and not Path(ready['image']).exists()
        images = plistlib.loads(command(['hdiutil', 'info', '-plist']))['images']
        assert not any(i.get('image-path') == ready['image'] for i in images)
        summary.update(original_worker_retirement=result, original_worker_exited=True)
        outer.detach(writers_retired=True)
        outer.remove()
        assert not outer.mount.exists() and not outer.image.exists()
        summary.update(passed=True, control_volume_removed=True)
    except BaseException as error:
        summary['error'] = repr(error)
        raise
    finally:
        if summary['passed']:
            summary['pruned_bytes'] = sum(p.stat().st_size for p in root.rglob('*') if p.is_file())
            shutil.rmtree(root)
        (output/'summary.json').write_text(json.dumps(summary, indent=2)+'\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=lambda value: Path(value).resolve())
    parser.add_argument('--target-volume', type=lambda value: Path(value).resolve(),
                        help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.target_volume is not None:
        # macOS rejects nested disk-image attachment. The existing owner_type seam
        # places the real image alongside control storage; all worker commands,
        # control records and retirement logic remain the production implementation.
        class SeparateImage:
            @classmethod
            def create(cls, directory, size_mib, command):
                return OwnedVolume.create(args.target_volume, size_mib, command)
        raise SystemExit(run(args.output, 256, 8, sys.stdin.buffer, sys.stdout.buffer,
                             owner_type=SeparateImage))
    probe(args.output)
