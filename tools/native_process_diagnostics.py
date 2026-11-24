"""Bounded evidence from an owned probe group before timeout cleanup."""
from pathlib import Path
import json
import subprocess
import sys
import time

from native_ui_diagnostics import sampled_threads, host_context


def group_processes(group, observation=None):
    observation = {} if observation is None else observation
    started = time.monotonic()
    observation.update(state='collecting', group=group, started=time.time(), timeout_s=2)
    try:
        result = subprocess.run(['ps', '-axo', 'pid=,pgid=,stat=,etime=,command='],
                                capture_output=True, text=True, timeout=2)
        observation.update(exit_code=result.returncode, stderr=result.stderr)
        result.check_returncode()
        rows = []
        for line in result.stdout.splitlines():
            if not line.strip():
                continue
            fields = line.strip().split(None, 4)
            if len(fields) != 5:
                raise ValueError('Incomplete process inventory')
            if int(fields[1]) == group:
                rows.append(dict(pid=int(fields[0]), group=int(fields[1]), state=fields[2],
                                 elapsed=fields[3], command=fields[4]))
        observation.update(state='complete', members=len(rows))
        return rows
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        observation.update(state='failed', error=str(error), error_type=type(error).__name__)
        raise
    finally:
        observation.update(finished=time.time(), elapsed_s=time.monotonic() - started)


def capture_timeout(group, log):
    report = dict(group=group, started=time.time(), processes=[], samples=[], inventory={})
    try:
        report['processes'] = group_processes(group, report['inventory'])
        candidates = [row for row in report['processes']
                      if Path(row['command'].split(' ', 1)[0]).name == 'xcodebuild']
        # At most one owned xcodebuild, five seconds for sampling. Never sample the host services.
        if candidates and Path('/usr/bin/sample').exists():
            target = candidates[0]
            report['identity_inventory'] = {}
            current = group_processes(group, report['identity_inventory'])
            if not any(row['pid'] == target['pid'] and row['command'] == target['command']
                       for row in current):
                report['error'] = 'Owned xcodebuild exited before sampling'
            else:
                output = log.with_name('timeout-xcodebuild.sample.txt')
                sample = dict(pid=target['pid'], command=target['command'])
                report['samples'].append(sample)
                with log.with_name('timeout-xcodebuild.log').open('w') as stream:
                    result = subprocess.run(['/usr/bin/sample', str(target['pid']), '1', '10',
                                             '-file', str(output)], stdout=stream,
                                            stderr=subprocess.STDOUT, timeout=5)
                sample.update(exit_code=result.returncode, sampled_threads=sampled_threads(output))
                if result.returncode or not sample['sampled_threads']:
                    sample['error'] = 'Sampler failed or returned no thread stacks'
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        report['error'] = str(error)
    finally:
        report['host_context'] = host_context(log.parent)
        report['finished'] = time.time()
        try:
            log.with_name('timeout-processes.json').write_text(json.dumps(report, indent=2) + '\n')
        except InterruptedError:
            raise
        except OSError as error:
            report['report_error'] = str(error)
            # The worker retains stderr even when this case's report cannot be published.
            print('Cannot save timeout diagnostic report; retaining its outcome in the worker log',
                  file=sys.stderr, flush=True)
            print(json.dumps(report), file=sys.stderr, flush=True)
