"""One bounded stack inspection of an owned, unreaped sampler; never recursive."""
import json
import os
from pathlib import Path
import re
import subprocess
import time

from native_process_resources import process_resources


def sampled_threads(path):
    if not path.exists():
        return 0
    return len(re.findall(r'^\s+\d+ Thread_\d+', path.read_text(), re.MULTILINE))


def unreaped_exit(process):
    """Observe a direct child's exit without releasing its PID to reuse."""
    return os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)


def blocked_symbol_threads(observation):
    snapshot = observation.get('thread_snapshot', {})
    if (observation.get('status') != 'available' or snapshot.get('status') != 'available'
            or not snapshot.get('identity_verified')):
        return []
    return [row['thread_id'] for row in snapshot['threads']
            if row['status'] == 'available' and row['raw']['pth_run_state'] == 4]


class SamplerInspection:
    """Caller must retain its primary child until close() has retired this child."""

    def __init__(self, prefix, deadline):
        self.prefix = Path(str(prefix) + '.sampler')
        self.deadline = deadline
        self.report = None
        self.process = None
        self.log = None

    @property
    def active(self):
        return self.process is not None and self.process.returncode is None

    def save(self):
        try:
            temporary = self.prefix.with_suffix('.sampler.tmp')
            temporary.write_text(json.dumps(self.report, indent=2) + '\n')
            temporary.replace(self.prefix.with_suffix('.sampler.json'))
        except InterruptedError:
            raise
        except OSError as error:
            self.report['metadata_error'] = str(error)

    def start(self, primary, identity, trigger):
        if self.report is not None:
            return
        self.report = dict(state='unavailable', target_identity=identity, trigger=trigger,
                           started_monotonic_ns=time.monotonic_ns())
        try:
            if primary.returncode is not None or unreaped_exit(primary):
                raise OSError('Primary sampler already exited')
            before = process_resources(primary.pid)
            self.report['target_before'] = before
            if (before.get('status') != 'available' or before.get('identity') != identity
                    or before['raw']['ri_proc_exit_abstime']):
                raise OSError('Primary sampler identity is unavailable or changed')
            started = time.monotonic()
            if self.deadline - started < 3:
                raise OSError('Fewer than three seconds remain in the primary deadline')
            self.deadline = min(self.deadline, started + 3)
            command = ['/usr/bin/sample', str(primary.pid), '1', '10', '-file',
                       str(self.prefix.with_suffix('.sampler.sample.txt'))]
            self.report.update(state='starting', command=command,
                               deadline_monotonic_ns=int(self.deadline * 1e9))
            self.save()
            self.log = self.prefix.with_suffix('.sampler.log').open('wb')
            self.process = subprocess.Popen(command, stdout=self.log, stderr=subprocess.STDOUT)
            self.report.update(state='running', pid=self.process.pid)
        except InterruptedError:
            raise
        except (OSError, AttributeError, subprocess.SubprocessError) as error:
            self.report.update(state='unavailable', error=str(error))
            if self.log is not None:
                self.log.close()
        finally:
            self.save()

    def update(self, stop_reason=None):
        try:
            self._update(stop_reason)
        except InterruptedError:
            raise
        except (OSError, UnicodeError, subprocess.SubprocessError) as error:
            self.report['error'] = str(error)
            self.retire()
            self.report.update(state='finished', exit_code=self.process.returncode,
                               finished_monotonic_ns=time.monotonic_ns())
            self.save()

    def _update(self, stop_reason):
        if self.process is None or self.report.get('state') != 'running':
            return
        if self.process.poll() is None:
            if not stop_reason and time.monotonic() < self.deadline:
                return
            self.report['stopped_because'] = stop_reason or 'inspection_deadline'
            self.process.kill()
            self.process.wait()
        self.report.update(state='finished', exit_code=self.process.returncode,
                           finished_monotonic_ns=time.monotonic_ns())
        try:
            path = self.prefix.with_suffix('.sampler.sample.txt')
            self.report['sampled_threads'] = sampled_threads(path)
            content = path.read_text() if path.exists() else ''
            pid = self.report['target_identity']['pid']
            self.report['target_header_matches'] = bool(re.search(
                r'^Process:\s+sample \[' + str(pid) + r'\]', content, re.MULTILINE))
            after = process_resources(pid)
            self.report['target_after'] = after
            self.report['identity_verified'] = (after.get('status') == 'available'
                                               and after.get('identity') == self.report['target_identity'])
            if (self.process.returncode or not self.report['sampled_threads']
                    or not self.report['target_header_matches'] or not self.report['identity_verified']):
                self.report['error'] = 'Inspection failed, lacks stacks or cannot verify its target'
        except InterruptedError:
            raise
        except OSError as error:
            self.report['error'] = str(error)
        finally:
            self.log.close()
            self.save()

    def close(self, reason):
        try:
            self.update(reason)
        finally:
            # Interruption during metadata/stack reading must still retire the child.
            self.retire()

    def retire(self):
        try:
            if self.active:
                self.process.kill()
                self.process.wait()
        finally:
            if self.log is not None:
                self.log.close()

    def primary_exited(self, primary):
        if 'primary_exit_observed_monotonic_ns' in self.report:
            return True
        try:
            exited = bool(unreaped_exit(primary))
            if exited:
                self.report['primary_exit_observed_monotonic_ns'] = time.monotonic_ns()
                self.save()
            return exited
        except InterruptedError:
            raise
        except OSError as error:
            # Observation failure must not become the original sample's failure.
            self.report['primary_observation_error'] = str(error)
            self.close('primary_observation_unavailable')
            self.save()
            return False
