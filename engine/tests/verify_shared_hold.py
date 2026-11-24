#!/usr/bin/env python3
"""Build a native before/after test overlay; never install an engine artifact.
Requires depot_tools on PATH, a clean matching checkout and existing out/frontcap.
"""
import argparse
import fcntl
import hashlib
import json
import os
import re
from pathlib import Path
import subprocess
import time
import xml.etree.ElementTree as ET

p = argparse.ArgumentParser()
for name in ['src', 'repo', 'evidence']:
    p.add_argument('--' + name, type=Path, required=True)
p.add_argument('--jobs', type=int, default=12)
a = p.parse_args()
src, repo, evidence = a.src.resolve(), a.repo.resolve(), a.evidence.resolve()
evidence.mkdir(parents=True, exist_ok=True)
status = evidence / 'native-status.json'
assert not status.exists(), 'Use a new evidence directory'
state = dict(pid=os.getpid(), started=time.time(), state='preflight', commands=[])
def save():
    status.write_text(json.dumps(state, indent=2) + '\n')
def git(checkout, *argv):
    return subprocess.check_output(['git', '-C', str(checkout), *argv])
def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()
def command(name, argv, expected=0):
    state['state'] = name
    save()
    with (evidence/(name+'.log')).open('wb') as log:
        child = subprocess.Popen(argv, cwd=src, stdout=log, stderr=subprocess.STDOUT)
        state['child_pid'] = child.pid
        save()
        code = child.wait()
    state.pop('child_pid', None)
    state['commands'].append(dict(name=name, argv=argv, exit_code=code))
    save()
    assert code == expected, (name, code, expected)

lock = (src/'out/frontcap/.frontcap-build.lock').open('a')
fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
applied, backups = [], {}
try:
    for checkout in [src, src/'v8']:
        assert not git(checkout, 'diff', '--binary', 'HEAD'), checkout
    version = dict(line.split('=', 1) for line in (src/'chrome/VERSION').read_text().splitlines() if '=' in line)
    assert '.'.join(version[k] for k in ['MAJOR', 'MINOR', 'BUILD', 'PATCH']) == re.search(r'^version = "([^"]+)"', (repo/'browsers.toml').read_text(), re.M).group(1)
    state['chromium_revision'] = git(src, 'rev-parse', 'HEAD').decode().strip()
    state['v8_revision'] = git(src/'v8', 'rev-parse', 'HEAD').decode().strip()
    assert state['v8_revision'] == (repo/'engine/v8/base-revision').read_text().strip()
    patches = [(src, f) for f in sorted((repo/'engine/patches').glob('*.patch'))]
    patches += [(src/'v8', f) for f in sorted((repo/'engine/v8').glob('*.patch'))]
    state['patches'] = {str(f.relative_to(repo)): digest(f) for _, f in patches}
    save()
    for checkout, patch in patches:
        git(checkout, 'apply', '--check', str(patch))
        git(checkout, 'apply', str(patch))
        applied.append((checkout, patch))
    stem = src/'third_party/blink/renderer/platform/scheduler/common'
    cc, header, test = [stem/n for n in ['process_time_override_coordinator.cc', 'process_time_override_coordinator.h', 'process_time_override_coordinator_unittest.cc']]
    backups = {f: f.read_bytes() for f in [cc, header, test]}
    overlay = repo/'engine/tests/shared-hold-wakeup.patch'
    state['test_overlay_sha256'] = digest(overlay)
    git(src, 'apply', '--check', str(overlay))
    git(src, 'apply', str(overlay))
    positive = {f: f.read_bytes() for f in backups}
    text = cc.read_text()
    start = text.index('  if (now != before)\n', text.index('void ProcessTimeOverrideCoordinator::FrontcapPollSharedClock()'))
    end = text.index('  FrontcapSharedProposalLocked();', start)
    cc.write_text(text[:start] + '''  if (now != before) {
    current_ticks_.store(now, std::memory_order_release);
    for (const auto& [client, requested] : requested_ticks_by_client_) client->ScheduleWork();
  }
''' + text[end:])
    field = '    // Last hold observed by the shared-clock notifier, under the coordinator lock.\n    uint64_t frontcap_notified_holds_ = 0;\n'
    assert field in header.read_text()
    header.write_text(header.read_text().replace(field, ''))
    state['counterfactual_sources'] = {str(f.relative_to(src)): digest(f) for f in positive}
    save()
    build = ['autoninja', '-C', 'out/frontcap', 'blink_platform_unittests', '-j', str(a.jobs)]
    command('before-build', build)
    common = [str(src/'out/frontcap/blink_platform_unittests'), '--test-launcher-jobs=1', '--test-launcher-retry-limit=0', '--single-process-tests']
    command('before-test', common + ['--gtest_filter=ProcessTimeOverrideCoordinatorTest.SharedHoldRequestsOneQueueRecheck', '--gtest_output=xml:'+str(evidence/'before.xml')], 1)
    cases = ET.parse(evidence/'before.xml').findall('.//testcase')
    assert len(cases) == 1 and cases[0].get('name') == 'SharedHoldRequestsOneQueueRecheck'
    failures = cases[0].findall('failure')
    assert failures and all('wake_count' in f.get('message', '') for f in failures)
    for path, data in positive.items():
        path.write_bytes(data)
    state['candidate_sources'] = {str(f.relative_to(src)): digest(f) for f in positive}
    save()
    command('after-build', build)
    command('after-test', common + ['--gtest_filter=ProcessTimeOverrideCoordinatorTest.*', '--gtest_output=xml:'+str(evidence/'after.xml')])
    cases = ET.parse(evidence/'after.xml').findall('.//testcase')
    assert len(cases) > 1 and not any(c.findall('failure') or c.findall('error') for c in cases)
    state.update(state='passed', passed_cases=len(cases))
except BaseException as error:
    state.update(state='failed', error=repr(error))
    raise
finally:
    try:
        for path, data in backups.items():
            path.write_bytes(data)
        for checkout, patch in reversed(applied):
            git(checkout, 'apply', '-R', '--check', str(patch))
            git(checkout, 'apply', '-R', str(patch))
        state['restored'] = all(not git(f, 'diff', '--binary', 'HEAD') for f in [src, src/'v8'])
        assert state['restored']
    except BaseException as error:
        state.update(state='failed', cleanup_error=repr(error))
        raise
    finally:
        state['finished'] = time.time()
        save()
