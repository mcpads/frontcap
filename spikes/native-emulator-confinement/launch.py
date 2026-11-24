"""macOS prerequisite probe: constrain an owned emulator's direct file writes.

This is not a production launcher or whole-Run quota. Existing external services
are outside its process subtree. Use only with the owned, mounted scratch recipe.
"""
from pathlib import Path
import argparse
import errno
import hashlib
import json
import os
import sys


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def open_descriptors():
    result = []
    for name in os.listdir('/dev/fd'):
        if name.isdigit() and int(name) > 2:
            try:
                os.fstat(int(name))
                result.append(int(name))
            except OSError as error:
                if error.errno != errno.EBADF:
                    raise
    return sorted(result)


def save_new(path, value):
    with path.open('x') as stream:
        json.dump(value, stream, indent=2)
        stream.write('\n')


def confined(entry_path):
    entry = json.loads(entry_path.read_text())
    scratch = Path(entry['scratch'])
    identity = scratch.stat()
    assert [identity.st_dev, identity.st_ino] == entry['identity']
    assert not open_descriptors(), 'Non-stdio authority survived exec'
    allowed = scratch / 'sandbox-write-control'
    with allowed.open('xb') as stream:
        stream.write(b'owned scratch control')
    assert allowed.read_bytes() == b'owned scratch control'
    allowed.unlink()
    outside = entry_path.parent / 'sandbox-forbidden-control'
    try:
        with outside.open('xb') as stream:
            stream.write(b'confinement failed')
    except OSError as error:
        assert error.errno == errno.EPERM, error
    else:
        raise RuntimeError('Outside write succeeded; refusing emulator launch')
    assert not outside.exists()
    assert Path(entry['descriptor_control']).read_bytes() == b''
    print(json.dumps(dict(kind='emulator_write_confinement_controls',
                          allowed_write=True, outside_write_errno=errno.EPERM,
                          nonstdio_descriptors=[], inherited_control_bytes=0,
                          identity=entry['identity'])), flush=True)
    executable = Path(entry['command'][0])
    assert digest(executable) == entry['emulator_sha256']
    os.execv(str(executable), entry['command'])


def launch(arguments):
    assert sys.platform == 'darwin', 'This prerequisite uses macOS sandbox-exec'
    evidence = arguments.evidence.resolve(strict=True)
    emulator = arguments.emulator.resolve(strict=True)
    assert digest(emulator) == arguments.sha256
    scratch = Path(os.environ['ANDROID_TMP']).resolve(strict=True)
    avd = Path(os.environ['ANDROID_AVD_HOME']).resolve(strict=True)
    assert avd.is_relative_to(scratch), 'AVD must be staged in owned scratch'
    assert scratch.stat().st_dev != evidence.stat().st_dev, 'Mounted scratch required'
    assert not (evidence / 'sandbox-forbidden-control').exists()
    home = scratch / 'sandbox-home'
    temporary = scratch / 'sandbox-tmp'
    home.mkdir()
    temporary.mkdir()
    profile = ('(version 1)\n(allow default)\n(deny file-write*)\n'
               '(allow file-write* (subpath ' + json.dumps(str(scratch)) + '))\n'
               '(allow file-write* (literal "/dev/null"))\n')
    policy = evidence / 'profile.sb'
    with policy.open('x') as stream:
        stream.write(profile)
    # Deliberately inject inheritable authority, then remove it with all other
    # non-stdio descriptors. The restricted entry independently checks the result.
    descriptor_control = evidence / 'descriptor-control.bin'
    descriptor = os.open(descriptor_control, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    os.set_inheritable(descriptor, True)
    descriptors = open_descriptors()
    assert descriptor in descriptors
    environment = dict(os.environ, HOME=str(home), ANDROID_USER_HOME=str(home),
                       ANDROID_EMULATOR_HOME=str(home), TMPDIR=str(temporary),
                       TMP=str(temporary), TEMP=str(temporary))
    command = arguments.command
    if command[:1] == ['--']:
        command = command[1:]
    entry_path = evidence / 'sandbox-entry.json'
    save_new(entry_path, dict(scratch=str(scratch), avd=str(avd),
                             identity=[scratch.stat().st_dev, scratch.stat().st_ino],
                             closed_descriptors=descriptors,
                             descriptor_control=str(descriptor_control),
                             profile_sha256=hashlib.sha256(profile.encode()).hexdigest(),
                             launcher_sha256=digest(Path(__file__)),
                             emulator_sha256=arguments.sha256,
                             command=[str(emulator), *command],
                             environment={key: environment[key] for key in
                                          ['HOME', 'ANDROID_USER_HOME', 'ANDROID_EMULATOR_HOME',
                                           'ANDROID_TMP', 'ANDROID_AVD_HOME', 'TMPDIR']}))
    for descriptor in descriptors:
        os.close(descriptor)
    os.execve('/usr/bin/sandbox-exec', ['sandbox-exec', '-f', str(policy),
              sys.executable, '-B', str(Path(__file__).resolve()), '--confined', str(entry_path)],
              environment)


if __name__ == '__main__':
    if sys.argv[1:2] == ['--confined']:
        confined(Path(sys.argv[2]))
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument('--evidence', type=Path, required=True)
        parser.add_argument('--emulator', type=Path, required=True)
        parser.add_argument('--sha256', required=True)
        parser.add_argument('command', nargs=argparse.REMAINDER)
        launch(parser.parse_args())
