"""Live Python client for the existing Rust resource group.

Only a freshly spawned group's pipe grants authority. Saved records are outcomes,
never a way to reconnect, signal a PID or restart an abandoned command.
"""
import json
from pathlib import Path
import subprocess
import time
import threading

from native_host_process import write_json


def monotonic_ns():
    # Explicitly share Rust's host-wide clock rather than assuming Python's default.
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC)


class ResourceGroup:
    def __init__(self, executable, directory, *, host, target, evidence_root, lifetime_s, ports=(), temporary_storage_root=None, scratch_volume=None, scratch_volume_root=None):
        executable = Path(executable)
        directory, host, target, evidence_root = map(Path, (directory, host, target, evidence_root))
        if (not executable.is_absolute() or not executable.is_file()
                or not all(p.is_absolute() for p in (directory, host, target, evidence_root))
                or evidence_root.resolve(strict=True) != evidence_root
                or target.parent.resolve(strict=True) != target.parent
                or not target.parent.is_relative_to(evidence_root)
                or type(lifetime_s) is not int or not 1 <= lifetime_s <= 4520):
            raise ValueError('Resource group requires original absolute paths and a bounded lifetime')
        if (len(ports) > 16 or len(set(ports)) != len(ports)
                or any(type(port) is not int or not 1 <= port <= 65535 for port in ports)):
            raise ValueError('Resource ports must be distinct valid TCP ports')
        if temporary_storage_root is not None:
            temporary_storage_root = Path(temporary_storage_root)
            if (scratch_volume is not None or not temporary_storage_root.is_absolute()
                    or temporary_storage_root.resolve(strict=True) != temporary_storage_root
                    or not temporary_storage_root.is_dir()):
                raise ValueError('Temporary storage requires an original directory and no volume override')
        if scratch_volume_root is not None:
            scratch_volume_root = Path(scratch_volume_root)
            if (scratch_volume is None or not scratch_volume_root.is_absolute()
                    or scratch_volume_root.parent.resolve(strict=True) != scratch_volume_root.parent
                    or '..' in scratch_volume_root.parts or scratch_volume_root == scratch_volume_root.parent
                    or scratch_volume_root.is_relative_to(target) or target.is_relative_to(scratch_volume_root)):
                raise ValueError('Scratch volume requires a canonical fresh location outside the target')
        self.directory, self.evidence_root, self.target = directory, evidence_root, target
        self.deadline_ns = monotonic_ns() + lifetime_s*10**9
        self.number = 0
        self.child = self.authority = self.ready = None
        directory.mkdir(mode=0o700, exist_ok=False)
        (directory/'group.lock').touch(exist_ok=False)
        for name in ('commands', 'replies', 'resources'):
            (directory/name).mkdir()
        config = dict(host=str(host), target=str(target), evidence_root=str(evidence_root),
            resource_executable=str(executable), resource_arguments=['--resource-process'],
            ports=list(ports), deadline_ns=self.deadline_ns)
        if temporary_storage_root is not None:
            config['temporary_storage_root'] = str(temporary_storage_root)
        if scratch_volume is not None:
            config['scratch_volume'] = scratch_volume
        if scratch_volume_root is not None:
            config['scratch_volume_root'] = str(scratch_volume_root)
        write_json(directory/'config.json', config)
        try:
            with (directory/'group.log').open('xb') as log:
                self.child = subprocess.Popen([str(executable), '--resource-group', str(directory)],
                    stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=log,
                    start_new_session=True, close_fds=True)
            self.authority = self.child.stdin
            self.authority.write(b'1')
            self.authority.flush()
            self.ready = self._wait(directory/'ready.json', min(self.deadline_ns, monotonic_ns()+10*10**9))
            if self.ready['target'] != str(target) or self.ready['evidence_root'] != str(evidence_root):
                raise RuntimeError('Resource group readiness differs from its original binding')
        except BaseException:
            self.revoke()
            if self.child is not None:
                threading.Thread(target=self.child.wait, daemon=True).start()
            raise

    def _wait(self, path, deadline_ns):
        while True:
            if path.exists():
                return json.loads(path.read_text())
            if self.child.poll() is not None:
                raise RuntimeError('Original resource group exited without the requested outcome')
            if monotonic_ns() >= deadline_ns:
                raise TimeoutError('Original resource group observation expired; inspect its original outcome')
            time.sleep(.01)

    def revoke(self):
        if self.authority is not None:
            authority, self.authority = self.authority, None
            authority.close()

    def execute(self, operation, *, timeout):
        if self.authority is None:
            raise RuntimeError('Resource group authority already revoked')
        try:
            deadline = min(self.deadline_ns, monotonic_ns()+int(timeout*10**9))
            if timeout <= 0 or monotonic_ns() >= deadline:
                raise TimeoutError('Resource command budget expired before publication')
            if self.child.poll() is not None:
                raise RuntimeError('Original resource group is unavailable')
            self.number += 1
            # One sequence per attempt; failed publication/observation never retries an effect.
            write_json(self.directory/'commands'/f'{self.number}.json', operation)
            result = self._wait(self.directory/'replies'/f'{self.number}.json', deadline)
            if result['reply'] == 'failed':
                raise RuntimeError('Resource group rejected command: '+result['error'])
            return result
        except BaseException:
            self.revoke()
            raise

    def launch(self, arguments, *, directory, environment=None, timeout=None):
        if not arguments or not Path(arguments[0]).is_absolute() or not Path(directory).is_absolute():
            raise ValueError('Supervised process needs absolute executable and working directory')
        deadline = self.deadline_ns if timeout is None else min(self.deadline_ns, monotonic_ns()+int(timeout*10**9))
        return dict(program=str(arguments[0]), arguments=[str(a) for a in arguments[1:]],
            environment=dict(environment or {}), directory=str(directory), deadline_ns=deadline)

    def prepare_avd(self, source, name, inventory, *, timeout):
        source = Path(source)
        if not source.is_absolute() or source.resolve(strict=True) != source or not source.is_dir():
            raise ValueError('AVD input requires its original canonical source directory')
        result = self.execute(dict(operation='prepare_avd', source=str(source), name=name,
            inventory=inventory), timeout=timeout)
        expected = Path(self.ready['temporary_disks'])/'avd'
        if result['reply'] != 'avd_prepared' or result['directory'] != str(expected):
            self.revoke()
            raise RuntimeError('Prepared AVD differs from the original scratch owner')
        return expected

    def start_adb_server(self, identity, port, arguments, *, directory, environment=None, timeout=10, confine=False):
        # The Rust owner excludes existing listeners and retains this child through forwarding cleanup.
        if type(port) is not int or not 1024 <= port <= 65535 or port == 5037:
            raise ValueError('ADB requires a private unprivileged server port')
        result = self.execute(dict(operation='start_adb_server', id=identity, port=port, confine=confine,
            launch=self.launch(arguments, directory=directory, environment=environment)), timeout=timeout)
        if result['reply'] != 'started':
            self.revoke()
            raise RuntimeError('Original private ADB server has no start outcome')
        return result['process']

    def register_forward(self, identity, *, adb, emulator, serial, socket, port, evidence,
                         server_port=None, write_root=None, timeout=10):
        """Register cleanup before the one original, separately captured bind command."""
        adb, evidence = Path(adb), Path(evidence)
        relative = evidence.relative_to(self.evidence_root)
        if (not adb.is_absolute() or type(port) is not int or not 1 <= port <= 65535
                or '..' in relative.parts or evidence.parent.resolve(strict=True) != evidence.parent
                or evidence.exists()):
            raise ValueError('Forwarding requires an explicit port and unused original capture path')
        registration = dict(adb=str(adb), emulator=emulator, serial=serial, socket=socket,
            port=port, bind_evidence=str(relative))
        if server_port is not None:
            if type(server_port) is not int or not 1024 <= server_port <= 65535 or server_port == 5037:
                raise ValueError('Forwarding requires a private unprivileged ADB server port')
            registration['server_port'] = server_port
        if write_root is not None:
            write_root = Path(write_root)
            if write_root != Path(self.ready['temporary_disks']):
                raise ValueError('Forwarding writes must use the original group scratch')
            registration['write_root'] = str(write_root)
        result = self.execute(dict(operation='register_forward', id=identity,
            registration=registration), timeout=timeout)
        if result['reply'] != 'forward_registered':
            self.revoke()
            raise RuntimeError('Original forwarding registration has no accepted outcome')

    def start_emulator(self, identity, arguments, *, directory, environment=None, timeout=10):
        result = self.execute(dict(operation='start_emulator', id=identity,
            launch=self.launch(arguments, directory=directory, environment=environment)), timeout=timeout)
        if result['reply'] != 'started':
            self.revoke()
            raise RuntimeError('Original confined emulator has no start outcome')
        return result['process']

    def start_process(self, identity, arguments, *, directory, environment=None, timeout=10):
        result = self.execute(dict(operation='start', id=identity,
            launch=self.launch(arguments, directory=directory, environment=environment)), timeout=timeout)
        if result['reply'] != 'started':
            self.revoke()
            raise RuntimeError('Original resource process has no start outcome')
        return result['process']

    def capture(self, arguments, evidence, *, directory, timeout, environment=None, output_limit=4*1024**2, confine_adb=False):
        evidence = Path(evidence)
        relative = evidence.relative_to(self.evidence_root)
        if '..' in relative.parts or evidence.parent.resolve(strict=True) != evidence.parent:
            raise ValueError('Capture evidence must remain in the original evidence root')
        result = self.execute(dict(operation='capture', id=f'helper-{self.number+1}',
            launch=self.launch(arguments, directory=directory, environment=environment, timeout=timeout),
            evidence=str(relative), output_limit=output_limit, confine_adb=confine_adb), timeout=timeout)
        expected = evidence/'exchange.json'
        if result['reply'] != 'captured' or result['evidence'] != str(expected):
            self.revoke()
            raise RuntimeError('Capture reply differs from its original exchange')
        return expected

    def stop(self, timeout=90):
        self.revoke()
        if self.child is None:
            raise RuntimeError('Resource group never spawned')
        self.child.wait(timeout=timeout)
        result = json.loads((self.directory/'retirement.json').read_text())
        if not result['passed']:
            raise RuntimeError('Original resource group retirement unresolved: '+str(result['errors']))
        return result
