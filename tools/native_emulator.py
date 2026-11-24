"""Live-handle Android emulator ownership for Python probe runners.

Call under the shared native host lock. Identity files support independent cleanup
observations only; retirement always uses this invocation's Popen handle/group.
"""
from pathlib import Path
import os
import shutil
import signal
import socket
import subprocess
import time

from native_cleanup import wait_for_android_cleanup
from native_output_log import OutputLog
from native_host_process import signal_owned_group, write_json


class OwnedEmulator:
    def __init__(self, sdk, directory, env, *, avd, port=5580, cores=4,
                 data_image=None, encryption_key=None):
        self.sdk = Path(sdk)
        self.directory = Path(directory)
        self.env = env
        self.avd, self.port, self.cores = avd, port, cores
        if (data_image is None) != (encryption_key is None):
            raise ValueError('Prepared data requires its matching encryption key image')
        self.data_image = Path(data_image).resolve(strict=True) if data_image is not None else None
        self.encryption_key = Path(encryption_key).resolve(strict=True) if encryption_key is not None else None
        self.process = None
        self.launch_attempted = False
        self.log = self.directory/'emulator.log'
        self.log_stream = self.log_capture = None
        self.temporary = None
        self.retirement = None

    @property
    def serial(self):
        return f'emulator-{self.port}'

    def start(self):
        if not 1024 <= self.port <= 65534 or self.port % 2:
            raise ValueError('Emulator requires an even unprivileged console port')
        if not self.avd or self.avd.startswith('-') or self.cores <= 0:
            raise ValueError('Invalid emulator configuration')
        sockets = []
        try:
            for port in (self.port, self.port+1):
                probe = socket.socket()
                sockets.append(probe)
                probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                probe.bind(('127.0.0.1', port))
        finally:
            for probe in sockets:
                probe.close()
        Path(self.env.get('TMPDIR', str(Path.home()/'Library/Caches/TemporaryItems'))).mkdir(parents=True, exist_ok=True)
        temporary = self.directory.resolve()/'emulator-temporary'
        temporary.mkdir(mode=0o700)
        identity = temporary.lstat()
        self.temporary = dict(path=str(temporary), device=identity.st_dev, inode=identity.st_ino)
        write_json(self.directory/'emulator-temporary.json', self.temporary)
        # SDK overlays use ANDROID_TMP rather than TMPDIR. Keep the caller's environment intact.
        emulator_env = dict(self.env, ANDROID_TMP=str(temporary))
        command = [str(self.sdk/'emulator/emulator'), '-avd', self.avd,
                   '-cores', str(self.cores), '-no-window', '-no-audio', '-no-snapshot',
                   '-read-only', '-no-boot-anim', '-port', str(self.port)]
        if self.data_image is not None:
            command += ['-data', str(self.data_image), '-encryption-key', str(self.encryption_key)]
        write_json(self.directory/'emulator-command.json', command)
        self.log_stream = self.log.open('xb')
        try:
            self.launch_attempted = True
            self.process = subprocess.Popen(command, env=emulator_env, stdout=subprocess.PIPE,
                                            stderr=subprocess.STDOUT, start_new_session=True)
            self.log_capture = OutputLog(self.process.stdout, self.log_stream)
        except BaseException:
            self.log_stream.close()
            raise
        (self.directory/'emulator-pid').write_text(str(self.process.pid)+'\n')
        (self.directory/'emulator-port').write_text(str(self.port)+'\n')

    def check_output(self):
        if self.log_capture is not None:
            self.log_capture.check()

    def wait_for_boot(self, timeout=180):
        deadline = time.monotonic()+timeout
        observations = []
        while True:
            self.check_output()
            if self.process.poll() is not None:
                raise RuntimeError(f'Owned emulator exited: {self.process.returncode}')
            remaining = deadline-time.monotonic()
            if remaining <= 0:
                raise TimeoutError('Owned emulator boot deadline reached')
            try:
                result = subprocess.run([str(self.sdk/'platform-tools/adb'), '-s', self.serial,
                                         'shell', 'getprop', 'sys.boot_completed'], env=self.env,
                                        capture_output=True, text=True, timeout=min(5, remaining))
                row = dict(exit_code=result.returncode, stdout=result.stdout, stderr=result.stderr)
            except subprocess.TimeoutExpired:
                row = dict(error='boot query timeout')
            observations.append(row)
            write_json(self.directory/'boot-observations.json', observations)
            if row.get('exit_code') == 0 and row['stdout'].strip() == '1':
                self.check_output()
                if self.process.poll() is not None:
                    raise RuntimeError('Owned emulator exited during boot observation')
                return
            time.sleep(min(.5, max(0, deadline-time.monotonic())))

    def retire(self):
        if self.retirement is not None:
            return self.retirement
        try:
            cleanup = self._retire_resources()
        except Exception as error:
            cleanup = dict(passed=False, checks=[dict(name='emulator_retirement', passed=False,
                                                     detail=str(error))])
        # Disk/process cleanup must run even when diagnostic capture overflowed or failed.
        if self.log_capture is not None:
            try:
                report = self.log_capture.finish(self.log.with_name(self.log.name+'.output.json'))
                check = dict(name='emulator_output_complete',
                             passed=report['eof'] and report['error'] is None, report=report)
            except Exception as error:
                check = dict(name='emulator_output_complete', passed=False, detail=str(error))
            finally:
                self.log_stream.close()
            cleanup['checks'].append(check)
            cleanup['passed'] &= check['passed']
        self.retirement = cleanup
        return cleanup

    def _retire_resources(self):
        if self.process is None:
            cleanup = dict(passed=not self.launch_attempted,
                           checks=[dict(name='emulator_not_started', passed=not self.launch_attempted)])
        else:
            try:
                signal_owned_group(self.process.pid, signal.SIGTERM, self.log)
                try:
                    self.process.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    pass
            finally:
                signal_owned_group(self.process.pid, signal.SIGKILL, self.log)
                self.process.wait(timeout=5)
            cleanup = wait_for_android_cleanup(self.directory, self.env)
            # Read-only after reaping: descendants may still be retiring even after ports close.
            deadline = time.monotonic()+5
            while True:
                try:
                    os.killpg(self.process.pid, 0)
                    detail = 'emulator process group remains present'
                except ProcessLookupError:
                    detail = None
                    break
                except OSError as error:
                    detail = str(error)
                    break
                if time.monotonic() >= deadline:
                    break
                time.sleep(.05)
            cleanup['checks'].append(dict(name='emulator_group_exited', passed=detail is None, detail=detail))
            cleanup['passed'] &= detail is None
        if cleanup['passed'] and self.temporary is not None:
            try:
                self.retire_temporary_disks()
                check = dict(name='emulator_temporary_removed', passed=True)
            except (OSError, ValueError) as error:
                check = dict(name='emulator_temporary_removed', passed=False, detail=str(error))
            cleanup['checks'].append(check)
            cleanup['passed'] &= check['passed']
        return cleanup

    def retire_temporary_disks(self):
        """Only the live owner calls this, after process/group/port retirement."""
        root = Path(self.temporary['path'])
        identity = root.lstat()
        if (root.is_symlink() or not root.is_dir()
                or (identity.st_dev, identity.st_ino) != (self.temporary['device'], self.temporary['inode'])):
            raise ValueError('Emulator temporary directory identity changed')
        entries = []

        def inventory(directory, depth=0):
            if depth > 32:
                raise ValueError('Emulator temporary inventory depth exceeded')
            for path in sorted(directory.iterdir()):
                if len(entries) >= 10000:
                    raise ValueError('Emulator temporary inventory count exceeded')
                info = path.lstat()
                if info.st_dev != identity.st_dev:
                    raise ValueError('Emulator temporary directory contains another filesystem')
                kind = ('symlink' if path.is_symlink() else 'directory' if path.is_dir()
                        else 'file' if path.is_file() else 'other')
                entries.append(dict(path=str(path.relative_to(root)), kind=kind,
                                    bytes=info.st_size, allocated_bytes=info.st_blocks*512))
                if kind == 'directory':
                    inventory(path, depth+1)

        inventory(root)
        report = dict(directory=self.temporary, removed=False, entries=entries)
        write_json(self.directory/'emulator-temporary-cleanup.json', report)
        # rmtree does not follow symlinks; shared AVD backing files are never traversed.
        shutil.rmtree(root)
        report['removed'] = True
        write_json(self.directory/'emulator-temporary-cleanup.json', report)
