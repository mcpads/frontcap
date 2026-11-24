"""Bind terminal native evidence to its request/outcome before collection promotion."""
import hashlib
import json
from pathlib import Path


def file_digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def artifact_digests(directory, *, allow_empty=False):
    root = Path(directory)
    artifacts = root / 'artifacts'
    if not artifacts.is_dir() or artifacts.is_symlink():
        raise ValueError('Missing native artifacts directory')
    files = {}
    for path in sorted(artifacts.rglob('*')):
        if path.is_symlink():
            raise ValueError(f'Native artifact is a symbolic link: {path.relative_to(root)}')
        if path.is_file():
            files[path.relative_to(root).as_posix()] = file_digest(path)
        elif not path.is_dir():
            raise ValueError(f'Native artifact is not a regular file: {path.relative_to(root)}')
    if not files and not allow_empty:
        raise ValueError('Native artifacts are empty')
    return files


def verify_completion(directory, request, state):
    """A passed observation must account for every requested case and its cleanup."""
    root = Path(directory)
    platforms, repeat = request.get('platforms'), request.get('repeat')
    if (not isinstance(platforms, list) or not platforms
            or any(p not in ('ios', 'android') for p in platforms) or len(set(platforms)) != len(platforms)
            or type(repeat) is not int or repeat < 1):
        raise ValueError('Invalid requested native cases')
    expected = [(f'{i:02d}-{target}', 200000 if i % 2 else 700000)
                for i in range(1, repeat + 1) for target in platforms]
    results = state.get('results')
    if (not isinstance(results, list) or any(not isinstance(r, dict) for r in results)
            or [r.get('case') for r in results] != [label for label, _ in expected]):
        raise ValueError('Passed status does not contain every requested native case in order')
    environment = json.loads((root / 'artifacts/environment.json').read_text())
    require_passed_checks(environment, 'environment')
    for summary, (label, hold_us) in zip(results, expected):
        folder = root / 'artifacts' / label
        result = json.loads((folder / 'result.json').read_text())
        if not isinstance(result, dict) or {k: v for k, v in result.items() if k != 'checks'} != summary:
            raise ValueError(f'Native case result differs from observed status: {label}')
        require_passed_checks(result, label)
        if label.endswith('-ios') and result.get('ios_app', 'probe') != request.get('ios_app', 'probe'):
            raise ValueError(f'Native iOS app profile differs from the request: {label}')
        if label.endswith('-ios') and result.get('ios_input_interruption') != request.get('ios_input_interruption'):
            raise ValueError(f'Native iOS input interruption profile differs from the request: {label}')
        if label.endswith('-android') and result.get('android_app', 'probe') != request.get('android_app', 'probe'):
            raise ValueError(f'Native Android app profile differs from the request: {label}')
        if label.endswith('-android') and result.get('android_handler_agent', 'none') != request.get('android_handler_agent', 'none'):
            raise ValueError('Android Handler agent differs from frozen request')
        if label.endswith('-android') and result.get('android_input_registration', 'broadcast') != request.get('android_input_registration', 'broadcast'):
            raise ValueError('Android input registration differs from frozen request')
        if label.endswith('-android') and result.get('android_runtime', 'normal') != request.get('android_runtime', 'normal'):
            raise ValueError(f'Native Android runtime differs from the request: {label}')
        if label.endswith('-android') and result.get('android_input_interruption') != request.get('android_input_interruption'):
            raise ValueError(f'Native input interruption profile differs from the request: {label}')
        if label.endswith('-android') and request.get('android_cores') is not None:
            from native_android_resources import check_cpu_count
            observed = check_cpu_count(folder/'measurements', request['android_cores'])
            if not observed['passed']:
                raise ValueError(f'Native CPU resources differ from frozen request: {label}: {observed["detail"]}')
        if (type(result.get('exit_code')) is not int or result['exit_code'] != 0
                or result.get('cleanup_passed') is not True or result.get('hold_us') != hold_us
                or result.get('interrupted') or result.get('error')):
            raise ValueError(f'Native case is incomplete or unsuccessful: {label}')
        require_passed_checks(json.loads((folder / 'cleanup.json').read_text()), label + ' cleanup')
        if not (folder / 'run.log').is_file() or not (folder / 'measurements').is_dir():
            raise ValueError(f'Native case is missing its log or measurements: {label}')
        if not any(path.is_file() for path in (folder / 'measurements').rglob('*')):
            raise ValueError(f'Native measurements are empty: {label}')


def require_passed_checks(report, name):
    if not isinstance(report, dict):
        raise ValueError(f'Native evidence report is not an object: {name}')
    checks = report.get('checks')
    if (report.get('passed') is not True or not isinstance(checks, list) or not checks
            or any(not isinstance(check, dict) or check.get('passed') is not True for check in checks)):
        raise ValueError(f'Native evidence has missing or failed checks: {name}')


def seal_evidence(directory, request, state):
    root = Path(directory)
    if state['state'] == 'passed':
        verify_completion(root, request, state)
    elif state['state'] not in ('failed', 'interrupted'):
        raise ValueError('Only terminal native outcomes can seal evidence')
    manifest = dict(request_sha256=file_digest(root / 'request.json'),
                    outcome={k: v for k, v in state.items() if k != 'evidence_sha256'},
                    files=artifact_digests(root, allow_empty=state['state'] != 'passed'))
    path = root / 'evidence.json'
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(manifest, indent=2) + '\n')
    temporary.replace(path)
    return file_digest(path)


def verify_collected_evidence(extracted, request_path, state):
    if state['state'] not in ('passed', 'failed', 'interrupted'):
        return 'running'
    root = Path(extracted)
    request = json.loads(Path(request_path).read_text())
    expected = state.get('evidence_sha256')
    if not expected and state['state'] != 'passed':
        if request.get('seal_failed_evidence'):
            # A dead worker or failed seal can leave useful bytes, but cannot
            # authorize replacing the previously verified evidence collection.
            if state.get('worker_missing') is True or state.get('evidence_error'):
                return 'unverified'
            raise ValueError('Terminal native run has no evidence seal or recorded sealing failure')
        return 'legacy'
    archived = json.loads((root / 'status.json').read_text())
    if archived != state:
        raise ValueError('Archived native status differs from the terminal observation; retry the same run handle')
    if state['state'] == 'passed':
        verify_completion(root, request, state)
    if not expected:
        if request.get('require_evidence_manifest'):
            raise ValueError('Passed native run has no sealed evidence manifest; retain this run handle')
        return 'legacy'  # Only completion records can be checked for older successful workers.
    manifest_path = root / 'evidence.json'
    if file_digest(manifest_path) != expected:
        raise ValueError('Native evidence manifest differs from the terminal observation')
    manifest = json.loads(manifest_path.read_text())
    if not isinstance(manifest, dict):
        raise ValueError('Native evidence manifest is not an object')
    if manifest.get('request_sha256') != file_digest(request_path):
        raise ValueError('Native evidence belongs to a different request')
    if ('outcome' in manifest or request.get('seal_failed_evidence') or state['state'] != 'passed'):
        if manifest.get('outcome') != {k: v for k, v in state.items() if k != 'evidence_sha256'}:
            raise ValueError('Native evidence belongs to a different terminal outcome')
    actual = artifact_digests(root, allow_empty=state['state'] != 'passed')
    recorded = manifest.get('files')
    if not isinstance(recorded, dict) or actual != recorded:
        missing = sorted(set(recorded or {}) - actual.keys()) if isinstance(recorded, dict) else []
        extra = sorted(actual.keys() - set(recorded or {})) if isinstance(recorded, dict) else []
        changed = sorted(name for name in actual.keys() & recorded.keys() if actual[name] != recorded[name]) if isinstance(recorded, dict) else []
        raise ValueError(f'Native evidence file mismatch: missing={missing[:8]}, extra={extra[:8]}, changed={changed[:8]}')
    return 'sealed'
