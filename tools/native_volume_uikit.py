"""Writer retirement for a UIKit session registered with its original live volume owner.

Records are quiescence evidence only. This module never adopts a volume or signals a PID.
"""
import hashlib
import json
import itertools
import os
from pathlib import Path
import stat
import subprocess
import time


def read(path):
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 1024 * 1024:
        raise ValueError('bounded regular UIKit evidence required')
    with path.open('rb') as stream:
        raw = stream.read(1024 * 1024 + 1)
    if len(raw) > 1024 * 1024:
        raise ValueError('UIKit evidence exceeded read bound')
    return json.loads(raw), hashlib.sha256(raw).hexdigest()


def absent(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    except PermissionError:
        return False
    return False


def identity(value):
    if (not isinstance(value, dict) or set(value) != {'pid', 'started'}
            or type(value['pid']) is not int or value['pid'] <= 0
            or not isinstance(value['started'], str) or not value['started']):
        raise ValueError('original UIKit process identity required')
    return value


def reader_binding(session, request):
    executable = request.get('retirement_reader')
    if executable is None:
        return {}
    path = Path(executable)
    if not path.is_absolute() or path.resolve() != path or not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError('canonical original retirement reader required')
    launch, digest = read(session/'launch.json')
    lifetime = launch['config'].get('resource_lifetime_ms')
    if type(lifetime) is not int or not 1 <= lifetime <= 900000:
        raise ValueError('retirement reader requires independent UIKit resources')
    with path.open('rb') as stream:
        executable_sha = hashlib.file_digest(stream, 'sha256').hexdigest()
    return dict(retirement_reader_sha256=executable_sha, launch_sha256=digest)


def group_writers_retired(binding, *, process_absent=absent):
    if binding['registration'].get('retirement_reader') is None:
        return True
    group=Path(binding['registration']['session'])/'target-resources'
    try:
        owner=identity(read(group/'owner.json')[0])
    except FileNotFoundError:
        return False
    if not process_absent(owner['pid']):
        return False
    # Group exit fixes the resource inventory. Its separate log collectors can outlive it.
    logs=[group/'group.log']
    resources=group/'resources'
    for entry in itertools.islice(resources.iterdir(), 16385):
        if entry.is_symlink() or not entry.is_dir():
            raise ValueError('invalid original group resource directory')
        logs.append(entry/'supervisor.log')
    if len(logs)>16385:
        raise ValueError('group resource inventory exceeds bound')
    for log in logs:
        root=log.with_name(log.name+'.diagnostics')
        if not root.exists():
            if log==logs[0]:
                return False
            continue  # A failed resource can retire before diagnostic initialization.
        if root.is_symlink() or not root.is_dir():
            raise ValueError('invalid group diagnostic root')
        entries=list(itertools.islice(root.iterdir(),2))
        if len(entries)!=1 or entries[0].is_symlink() or not entries[0].is_dir():
            raise ValueError('ambiguous group diagnostic owner')
        directory=entries[0]
        try:
            config,_=read(directory/'config.json');process=identity(read(directory/'ready.json')[0])
            if config != {'log':str(log)}:
                raise ValueError('group diagnostic output differs')
            if not process_absent(process['pid']):
                return False
            result,_=read(directory/'result.json')
        except FileNotFoundError:
            return False
        if result.get('eof') is not True:
            raise ValueError('group diagnostic EOF unproven')
    return True


def target_retirement(binding, result):
    retirement = result.get('retirement') if result is not None else None
    if isinstance(retirement, dict) and retirement.get('passed') is True and retirement.get('errors') == []:
        return retirement, None
    executable = binding['registration'].get('retirement_reader')
    if executable is None:
        if result is None:
            return None
        raise ValueError('original UIKit target retirement is unproven')
    session = Path(binding['registration']['session'])
    if reader_binding(session, binding['registration']) != {
            key: binding[key] for key in ('retirement_reader_sha256', 'launch_sha256')}:
        raise ValueError('original UIKit retirement reader or launch changed')
    # The registered original executable implements only a read-only inspection in this mode.
    # It reuses Rust group/child/Simulator/volume validation, not a second Python interpretation.
    output = subprocess.run([executable, '--inspect-retirement', str(session)],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5, check=True)
    if len(output.stdout) > 65536 or len(output.stderr) > 65536:
        raise ValueError('retirement inspection exceeded reply bound')
    response = json.loads(output.stdout)
    if not isinstance(response, dict) or set(response) != {'retirement'}:
        raise ValueError('invalid retirement inspection reply')
    retirement = response['retirement']
    if retirement is None:
        return None
    if not isinstance(retirement, dict) or retirement.get('passed') is not True or retirement.get('errors') != []:
        raise ValueError('independent UIKit retirement failed')
    return retirement, hashlib.sha256(output.stdout).hexdigest()


def register(mount, request, *, process_absent=absent):
    if (not isinstance(request, dict) or set(request) not in ({'operation', 'session', 'worker', 'ready_sha256'},
            {'operation', 'session', 'worker', 'ready_sha256', 'retirement_reader'})
            or request['operation'] != 'watch_uikit' or not isinstance(request['session'], str)):
        raise ValueError('invalid UIKit watch registration')
    session = Path(request['session'])
    if (not session.is_absolute() or session.parent != mount or session.resolve() != session
            or not session.is_dir() or session.stat().st_dev != mount.stat().st_dev):
        raise ValueError('UIKit session must be a direct directory of this live volume')
    worker = identity(request['worker'])
    if read(session / 'worker.json')[0] != worker or process_absent(worker['pid']):
        raise ValueError('registered UIKit worker is not the original live session')
    ready, ready_sha = read(session / 'ready.json')
    if ready_sha != request['ready_sha256']:
        raise ValueError('UIKit readiness differs from original registration')
    pids = [ready[role]['pid'] for role in ('app', 'runner')]
    if any(type(pid) is not int or pid <= 0 for pid in pids):
        raise ValueError('UIKit app and runner identities required')
    root = session / 'worker.log.diagnostics'
    if root.is_symlink() or not root.is_dir():
        raise ValueError('original UIKit diagnostic directory required')
    entries = list(itertools.islice(root.iterdir(), 2))
    if len(entries) != 1 or entries[0].is_symlink() or not entries[0].is_dir():
        raise ValueError('ambiguous UIKit diagnostic owner')
    diagnostic = entries[0]
    config, config_sha = read(diagnostic / 'config.json')
    if config != {'log': str(session / 'worker.log')}:
        raise ValueError('diagnostic output differs from registered UIKit session')
    process = identity(read(diagnostic / 'ready.json')[0])
    return dict(registration=request, diagnostic=str(diagnostic), diagnostic_process=process,
                diagnostic_config_sha256=config_sha, target_pids=pids, **reader_binding(session, request))


def retired(binding, *, process_absent=absent):
    request = binding['registration'];session = Path(request['session'])
    if read(session / 'worker.json')[0] != request['worker'] or read(session / 'ready.json')[1] != request['ready_sha256']:
        raise ValueError('original UIKit ownership evidence changed')
    diagnostic = Path(binding['diagnostic'])
    if (read(diagnostic / 'config.json')[1] != binding['diagnostic_config_sha256']
            or read(diagnostic / 'ready.json')[0] != binding['diagnostic_process']):
        raise ValueError('original UIKit diagnostic identity changed')
    pids = [request['worker']['pid'], binding['diagnostic_process']['pid'], *binding['target_pids']]
    if not all(process_absent(pid) for pid in pids):
        return None
    try:
        log, log_sha = read(diagnostic / 'result.json')
    except FileNotFoundError:
        return None
    try:
        result, result_sha = read(session / 'result.json')
    except FileNotFoundError:
        result = result_sha = None
    if result is not None and result.get('acquisition_not_started', False) is not False:
        raise ValueError('ready UIKit session contradicts pre-acquisition proof')
    if log.get('eof') is not True:
        raise ValueError('original UIKit diagnostic EOF is unproven')
    resolved = target_retirement(binding, result)
    if resolved is None:
        return None
    retirement, independent_sha = resolved
    if not group_writers_retired(binding, process_absent=process_absent):
        return None
    proof = dict(binding=binding, pids_absent=pids, session_result_sha256=result_sha,
                 diagnostic_result_sha256=log_sha, retirement=retirement)
    if independent_sha is not None:
        proof['independent_retirement_sha256'] = independent_sha
    return proof


def wait_for_retirement(binding, deadline):
    while True:
        proof = retired(binding)
        if proof is not None:
            return proof
        if time.monotonic() >= deadline:
            raise TimeoutError('registered UIKit writers have not retired')
        time.sleep(min(.05, max(0, deadline-time.monotonic())))


def register_acquisition(mount, request, *, process_absent=absent):
    if (not isinstance(request, dict) or set(request) not in ({'operation', 'session', 'worker'},
            {'operation', 'session', 'worker', 'retirement_reader'})
            or request['operation'] != 'watch_uikit_acquisition' or not isinstance(request['session'], str)):
        raise ValueError('invalid UIKit acquisition watch')
    session = Path(request['session'])
    if (not session.is_absolute() or session.parent != mount or session.resolve() != session
            or not session.is_dir() or session.stat().st_dev != mount.stat().st_dev):
        raise ValueError('UIKit acquisition must be a direct directory of this live volume')
    worker = identity(request['worker'])
    if read(session/'worker.json')[0] != worker or process_absent(worker['pid']):
        raise ValueError('original live UIKit acquisition worker required')
    if (session/'ready.json').exists() or (session/'target').exists() or (session/'result.json').exists():
        raise ValueError('UIKit acquisition already started before watch')
    root = session/'worker.log.diagnostics'
    if root.is_symlink() or not root.is_dir():
        raise ValueError('original diagnostic directory required')
    entries = list(itertools.islice(root.iterdir(), 2))
    if len(entries) != 1 or entries[0].is_symlink() or not entries[0].is_dir():
        raise ValueError('ambiguous acquisition diagnostic owner')
    diagnostic = entries[0]
    config, digest = read(diagnostic/'config.json')
    if config != {'log': str(session/'worker.log')}:
        raise ValueError('acquisition diagnostic output differs')
    return dict(registration=request, diagnostic=str(diagnostic),
                diagnostic_process=identity(read(diagnostic/'ready.json')[0]), diagnostic_config_sha256=digest,
                **reader_binding(session, request))


def retired_acquisition(binding, *, process_absent=absent):
    request=binding['registration'];session=Path(request['session']);diagnostic=Path(binding['diagnostic'])
    if (read(session/'worker.json')[0] != request['worker']
            or read(diagnostic/'ready.json')[0] != binding['diagnostic_process']
            or read(diagnostic/'config.json')[1] != binding['diagnostic_config_sha256']):
        raise ValueError('original acquisition ownership changed')
    pids=[request['worker']['pid'],binding['diagnostic_process']['pid']]
    if not all(process_absent(pid) for pid in pids):
        return None
    try:
        log,log_sha=read(diagnostic/'result.json')
    except FileNotFoundError:
        return None
    try:
        result,result_sha=read(session/'result.json')
    except FileNotFoundError:
        result=result_sha=None
    if log.get('eof') is not True:
        raise ValueError('acquisition diagnostic EOF unproven')
    independent_sha = retirement = None
    if result is not None and result.get('acquisition_not_started', False) is True:
        if (result.get('retirement') is not None or not isinstance(result.get('error'),str)
                or not result['error'] or (session/'target').exists() or (session/'ready.json').exists()):
            raise ValueError('contradictory pre-acquisition proof')
    else:
        resolved=target_retirement(binding,result)
        if resolved is None:
            return None
        retirement,independent_sha=resolved
        if not group_writers_retired(binding, process_absent=process_absent):
            return None
        if (session/'ready.json').exists():
            ready,_=read(session/'ready.json');targets=[ready[role]['pid'] for role in ('app','runner')]
            if any(type(pid) is not int or pid <= 0 for pid in targets):
                raise ValueError('invalid acquired target identities')
            pids.extend(targets)
            if not all(process_absent(pid) for pid in targets):
                return None
    proof=dict(binding=binding,pids_absent=pids,session_result_sha256=result_sha,
                diagnostic_result_sha256=log_sha,result=result)
    if independent_sha is not None:
        proof.update(independent_retirement_sha256=independent_sha,retirement=retirement)
    return proof


def wait_for_acquisition_retirement(binding, deadline):
    while True:
        proof=retired_acquisition(binding)
        if proof is not None:
            return proof
        if time.monotonic() >= deadline:
            raise TimeoutError('UIKit acquisition writers have not retired')
        time.sleep(min(.05,max(0,deadline-time.monotonic())))
