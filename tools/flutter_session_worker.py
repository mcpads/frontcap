"""Owned Flutter test-build lifetime. Stdin EOF revokes authority and starts retirement.

The controller sends start/stop and small host-command notifications. App operations use the
authenticated loopback endpoint. No stored PID is used for signalling or adoption.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import queue
import select
import signal
import subprocess
import sys
import threading
import time
import traceback
import uuid

from flutter_http_bridge import exchange
from flutter_host_commands import installed_target, host_arguments
from flutter_host_commands import bundle_inventory
from flutter_ios_app import IosApp
from flutter_ios_entry import ColdEntry
from flutter_android_app import AndroidApp, validate_product as validate_android_product
from flutter_prelaunch import wait_for_initial_data
from flutter_system_ui import SystemRunner, validate_products
from native_cleanup import inspect_cleanup, require_retired_native_session, retire_simulator
from native_output_log import OutputLog
from native_worker_control import TerminalRecord
from native_write_budget import begin_retirement
from native_control_reserve import reserve_control
from native_emulator import OwnedEmulator
from native_command_capture import capture as capture_command
from flutter_resource_group import Resources, validate as validate_resources
from flutter_simulator_storage import SimulatorStorage, validate as validate_data_volume
from native_host_process import execute, now, signal_owned_group, WorkerInterrupted, write_json


def app_define_arguments(defines):
    import re
    if not isinstance(defines, dict) or len(defines) > 16:
        raise ValueError('Invalid Flutter app definitions')
    for key, value in defines.items():
        if (not isinstance(key, str) or not re.fullmatch(r'[A-Z][A-Z0-9_]{0,63}', key)
                or key.startswith('FRONTCAP_') or not isinstance(value, str)
                or len(value.encode('utf-8')) > 2048 or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in value)):
            raise ValueError('Invalid Flutter app definition')
    return [f'--dart-define={key}={value}' for key, value in sorted(defines.items())]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('config', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('--control-directory', type=Path)
    args = parser.parse_args()
    terminal = TerminalRecord(args.control_directory) if args.control_directory else None
    config = json.loads(args.config.read_text())
    required = {'platform', 'source_sha256', 'acquisition_timeout_s', 'lifetime_s'}
    allowed = required | {'flutter', 'project', 'android_sdk', 'avd', 'runtime', 'app_defines', 'system_ui', 'installed_app', 'initial_data', 'data_volume', 'resource_executable', 'android_resources'}
    if not required <= set(config) <= allowed or config['platform'] not in ('android', 'ios'):
        raise ValueError('Invalid Flutter session configuration')
    for key, maximum in [('acquisition_timeout_s', 900), ('lifetime_s', 3500)]:
        if type(config[key]) is not int or not 1 <= config[key] <= maximum:
            raise ValueError('Invalid real session budget')
    defines = app_define_arguments(config.get('app_defines', {}))
    installed = config.get('installed_app')
    validate_data_volume(config.get('data_volume'), config['platform'], installed)
    initial_data = config.get('initial_data')
    validate_resources(config.get('resource_executable'), config['platform'], installed,
        config.get('android_resources'), config.get('avd'), config.get('android_sdk'))
    if initial_data is not None and (installed is None or not isinstance(initial_data, str)
                                     or not Path(initial_data).is_absolute()):
        raise ValueError('Flutter initial data requires a frozen standalone product and absolute path')
    if installed is not None:
        if defines or not isinstance(installed, dict):
            raise ValueError('Standalone Flutter product requires frozen build definitions')
        if config['platform'] == 'android':
            product = validate_android_product(installed)
        else:
            if set(installed) != {'product', 'inventory'}:raise ValueError('Invalid standalone Flutter iOS product')
            product = Path(installed['product'])
            if product.suffix != '.app' or bundle_inventory(product) != installed['inventory']:
                raise ValueError('Frozen standalone Flutter product changed')
    source = config['source_sha256']
    if config.get('system_ui') is not None:
        if config['platform'] != 'ios': raise ValueError('System UI requires iOS')
        validate_products(config['system_ui'])
    if type(source) is not str or len(source) != 64 or any(c not in '0123456789abcdef' for c in source):
        raise ValueError('Invalid frozen source digest')
    output = args.output.resolve()
    flutter = None
    project = output
    if installed is None:
        if not {'flutter', 'project'} <= set(config):
            raise ValueError('Flutter source execution requires SDK and project paths')
        flutter = Path(config['flutter']).resolve(strict=True)
        project = Path(config['project']).resolve(strict=True)
        if not (project/'integration_test/session_test.dart').is_file():
            raise ValueError('Missing instrumented Flutter session fixture')
    output.mkdir(exist_ok=False, parents=True)
    state = dict(state='starting', started=now(), pid=os.getpid(), platform=config['platform'])
    write_json(output/'config.json', config)
    write_json(output/'status.json', state)
    controls = queue.Queue()
    retiring = threading.Event()

    def interrupted(signum, frame):
        raise WorkerInterrupted('Flutter session authority revoked or worker interrupted')
    for signum in (signal.SIGTERM, signal.SIGINT):
        signal.signal(signum, interrupted)

    def read_control():
        pending = b''
        try:
            while not retiring.is_set():
                if not select.select([sys.stdin.fileno()], [], [], .1)[0]:
                    continue
                data = os.read(sys.stdin.fileno(), 4096)
                if not data:
                    break
                pending += data
                while b'\n' in pending:
                    line, pending = pending.split(b'\n', 1)
                    if len(line) > 4096:
                        raise ValueError('Invalid session control frame')
                    command = json.loads(line)
                    controls.put(command)
                    if command == {'command': 'stop'}:
                        return  # Explicit stop owns retirement even if the pipe closes next.
                if len(pending) > 4096:
                    raise ValueError('Invalid session control frame')
        except Exception:
            pass
        # Signal our own live process to interrupt bounded acquisition as well as idle waits.
        if not retiring.is_set():
            os.kill(os.getpid(), signal.SIGTERM)

    lock_dir = Path.home()/'.cache/frontcap'
    lock_dir.mkdir(parents=True, exist_ok=True)
    sdk_path = str(flutter.parent)+':' if flutter is not None else ''
    env = dict(os.environ, PATH=sdk_path+'/opt/homebrew/bin:'+os.environ['PATH'], CI='true')
    sdk = Path(config.get('android_sdk', str(Path.home()/'Library/Android/sdk')))
    adb = sdk/'platform-tools/adb'
    if config['platform'] == 'android':
        env['ANDROID_HOME'] = str(sdk)
    process = emulator = endpoint = system = app = None
    data_storage = resources = None
    resources_attempted = False
    device = None
    log_stream = log_capture = None
    owns_device = False
    forwarded = None
    forward_uncertain = False
    log = output/'flutter.log'
    deadline = time.monotonic()+config['acquisition_timeout_s']

    def remaining(limit):
        left = deadline-time.monotonic()
        if left <= 0:
            raise TimeoutError('Flutter acquisition deadline reached')
        return min(limit, left)

    def run(name, command, timeout):
        state['active'] = name
        write_json(output/'status.json', state)
        if (resources.execute if resources else execute)(command, output/(name+'.log'), env, remaining(timeout), project):
            raise RuntimeError(name+' failed')

    def recorded_command(directory, command, timeout):
        directory.mkdir(exist_ok=True)
        write_json(directory/'command.json', dict(argv=command, timeout_s=timeout))
        code = (resources.execute if resources else execute)(command, directory/'command.log', env, timeout, project)
        write_json(directory/'exit.json', dict(exit_code=code))
        if code:
            raise RuntimeError('Flutter host command failed')
        log = directory/'command.log'
        if log.stat().st_size > 4 * 1024 * 1024:
            raise ValueError('Flutter host output exceeds bound')
        return log.read_bytes()

    reader = threading.Thread(target=read_control, daemon=True)
    control_reserve = None

    def release_control():
        nonlocal control_reserve
        if control_reserve is not None:
            control_reserve.close()
            control_reserve = None

    with (output/'session.lock').open('a') as session_lock, (lock_dir/'native-tests.lock').open('a') as host_lock:
        fcntl.flock(session_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            reader.start()
            if controls.get(timeout=remaining(30)) != {'command': 'start'}:
                raise ValueError('Expected session start')
            control_reserve = reserve_control(output)
            if config.get('resource_executable') is None:
                fcntl.flock(host_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                require_retired_native_session(lock_dir)
            if installed is None:
                run('sdk', [str(flutter), '--version', '--machine'], 30)
                run('dependencies', [str(flutter), 'pub', 'get', '--enforce-lockfile'], 180)
            if config.get('resource_executable') is None:
                write_json(lock_dir/'native-session.json', dict(directory=str(output), retired=False))
                owns_device = True
            if config['platform'] == 'android':
                if config.get('resource_executable') is not None:
                    resources_attempted = True
                    resources = Resources(config['resource_executable'], output, lock_dir, config, env, deadline)
                    resources.acquire(config, deadline)
                    device = resources.device
                else:
                    emulator = OwnedEmulator(sdk, output, env, avd=config.get('avd', 'Medium_Phone_API_35'))
                    emulator.start()
                    emulator.wait_for_boot(remaining(180))
                    device = emulator.serial
            else:
                if config.get('resource_executable') is not None:
                    resources_attempted = True
                    resources = Resources(config['resource_executable'], output, lock_dir, config, env, deadline)
                    resources.acquire(config, deadline)
                    device = resources.device
                    (output/'simulator-id').write_text(device+'\n')
                    data_storage = resources
                else:
                    run('create', ['xcrun', 'simctl', 'create', 'frontcap-flutter-'+output.name, 'iPhone 16',
                        config.get('runtime', 'com.apple.CoreSimulator.SimRuntime.iOS-26-1')], 60)
                    device = str(uuid.UUID((output/'create.log').read_text().strip())).upper()
                    (output/'simulator-id').write_text(device+'\n')
                    if config.get('data_volume') is not None:
                        command_number = 0
                        def volume_command(arguments):
                            nonlocal command_number
                            command_number += 1
                            directory = output/f'data-command-{command_number}'
                            result = capture_command(arguments, directory, timeout=90, env=env)
                            if result['error'] is not None or result['exit_code'] != 0 or not result.get('child_retired'):
                                raise RuntimeError('Flutter data-volume command outcome unavailable: '+str(result))
                            return (directory/'stdout').read_bytes()
                        data_storage = SimulatorStorage(output/'simulator-data-volume', config['data_volume'], volume_command)
                        data_storage.prepare(device)
                    run('boot', ['xcrun', 'simctl', 'boot', device], 60)
                    run('boot-status', ['xcrun', 'simctl', 'bootstatus', device, '-b'], 180)
                    if data_storage is not None: data_storage.verify()
                if config.get('system_ui') is not None:
                    state['active'] = 'system-ui'; write_json(output/'status.json', state)
                    system = SystemRunner(config['system_ui'], device, output/'system-ui', env, source, recorded_command, **({'resources': resources} if resources else {}))
                    system.start(deadline)
            state['active'] = 'endpoint'
            write_json(output/'status.json', state)
            if installed is not None:
                if config['platform'] == 'android':
                    app = AndroidApp(device, installed, output/'installed-app', env, adb,
                        **(dict(resource_group=resources.group, emulator=resources.emulator,
                            server_port=resources.android['adb_server_port'], confine_adb=True) if resources else {}))
                    if initial_data is not None:app.require_fresh(deadline)
                    run('install', [str(adb), '-s', device, 'install', '-r', str(product)], 60)
                    run('wake', [str(adb), '-s', device, 'shell', 'input', 'keyevent', 'KEYCODE_WAKEUP'], 20)
                    run('unlock', [str(adb), '-s', device, 'shell', 'wm', 'dismiss-keyguard'], 20)
                else:
                    app = IosApp(device, product, installed['inventory'], output/'installed-app', env,
                        **({'executor': resources.execute} if resources else {}))
                    if initial_data is not None:app.require_fresh(deadline)
                    run('install', ['xcrun', 'simctl', 'install', device, str(product)], 60)
                    if data_storage is not None: data_storage.verify(app.bundle)
                if initial_data is not None:
                    state['active'] = 'initial-data';write_json(output/'status.json', state)
                    identity = app.prelaunch_identity(deadline)
                    def initial_command(arguments, directory, timeout):
                        if resources is not None and arguments == ['apply_ios_initial_data']:
                            resources.verify(app.bundle)
                            return recorded_command(directory, [config['resource_executable'],
                                '--apply-flutter-initial-data', str(output.parent)], timeout)
                        if config['platform'] != 'android' or arguments[0] not in ('shell', 'push'):
                            raise ValueError('Initial-data command is outside the owned Android data path')
                        return recorded_command(directory, [str(adb), '-s', device, *arguments], timeout)
                    wait_for_initial_data(controls, output/'prelaunch', identity, deadline,
                                          initial_command, lambda: app.prelaunch_identity(deadline))
                    app.initial_installation = identity['installation' if config['platform'] == 'android' else 'containers']
                application = app.start(output/'first-launch', deadline)
                endpoint = {k:v for k,v in application['endpoint'].items() if k != 'scenario'}
                if config['platform'] == 'android':endpoint['port'] = application['forwarding']['host_port']
            else:
                log_stream = log.open('wb')
                process = subprocess.Popen([str(flutter), 'test', '--no-pub', '--reporter', 'expanded',
                    'integration_test/session_test.dart', '-d', device,
                    '--dart-define=FRONTCAP_CONTROLLED_FRAMES=true', *defines], cwd=project, env=env,
                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
                log_capture = OutputLog(process.stdout, log_stream)
                write_json(output/'flutter-process.json', dict(pid=process.pid))
            while endpoint is None:
                if log_capture is not None: log_capture.check()
                if emulator is not None: emulator.check_output()
                for line in log.read_text(errors='replace').splitlines(keepends=True):
                    if not line.endswith('\n'): continue  # A live collector may have only a line prefix.
                    if line.startswith('FRONTCAP_FLUTTER_ENDPOINT '):
                        endpoint = json.loads(line.split(' ', 1)[1])
                        if endpoint.pop('scenario') != 'session':
                            raise ValueError('Unexpected Flutter endpoint scenario')
                        break
                if emulator is not None and emulator.process.poll() is not None:
                    raise RuntimeError(f'Owned emulator exited before readiness: {emulator.process.returncode}')
                if process.poll() is not None:
                    raise RuntimeError('Flutter exited before readiness')
                if endpoint is None:
                    time.sleep(remaining(.1))
            if config['platform'] == 'android' and installed is None:
                forward_uncertain = True
                write_json(output/'forwarding.json', dict(unresolved=True))
                reply = subprocess.run([str(adb), '-s', device, 'forward', 'tcp:0', 'tcp:'+str(endpoint['port'])],
                    env=env, capture_output=True, text=True, check=True, timeout=remaining(10))
                forwarded = int(reply.stdout.strip())
                endpoint['port'] = forwarded
                forward_uncertain = False
                write_json(output/'forwarding.json', dict(unresolved=False, port=forwarded))
            ready = dict(owner=dict(platform=config['platform'], device=device,
                generation=endpoint['generation'], source_sha256=source),
                endpoint=dict(port=endpoint['port'], token=endpoint['token']))
            if app is not None:
                target = (dict(platform='android',serial=device,package=app.package) if config['platform']=='android'
                          else dict(platform='ios', simulator=device.lower(), bundle=app.bundle))
            else:
                target, application = installed_target(config['platform'], project, device, adb,
                    lambda name, argv: recorded_command(output/name, argv, remaining(15)))
            write_json(output/'application.json', application)
            if target is not None:
                ready['external_url_target'] = target
            if system is not None:
                system.ensure_running()
                ready['system_ui'] = system.identity
            write_json(output/'ready.json', ready)
            state.update(state='ready', active=None)
            write_json(output/'status.json', state)
            stop_at = time.monotonic()+config['lifetime_s']
            host_number = 0
            health_at = 0
            cold_entry = None
            host_state = 'foreground'
            while True:
                if log_capture is not None: log_capture.check()
                if emulator is not None: emulator.check_output()
                if system is not None: system.ensure_running()
                if app is not None and cold_entry is None and time.monotonic() >= health_at:
                    if data_storage is not None: data_storage.verify()
                    app.ensure_running(stop_at, record=False)
                    health_at = time.monotonic()+1
                if process is not None and process.poll() is not None:
                    raise RuntimeError('Flutter session exited without owner stop')
                if emulator is not None and emulator.process.poll() is not None:
                    raise RuntimeError('Owned Flutter emulator exited')
                if time.monotonic() >= stop_at:
                    raise TimeoutError('Flutter session lifetime expired')
                try:
                    command = controls.get(timeout=.1)
                except queue.Empty:
                    continue
                if isinstance(command, dict) and set(command) == {'command', 'number'} and command['command'] == 'host':
                    if type(command['number']) is not int or command['number'] != host_number + 1:
                        raise ValueError('Flutter host command sequence changed')
                    host_number += 1
                    directory = output/'commands'/str(host_number)
                    request_file = directory/'request.json'
                    try:
                        if request_file.stat().st_size > 65536:
                            raise ValueError('Flutter host command exceeds bound')
                        request = json.loads(request_file.read_text())
                        if request.pop('number') != host_number:
                            raise ValueError('Flutter host command identity changed')
                        timeout = request.pop('timeout_ms')
                        if type(timeout) is not int or not 1 <= timeout <= 120000:
                            raise ValueError('Invalid Flutter host command budget')
                        budget = min(timeout/1000, stop_at-time.monotonic())
                        if budget <= 0:
                            raise TimeoutError('Flutter host command lifetime expired')
                        operation = request.get('operation')
                        if cold_entry is not None and operation not in ('poll_entry', 'system_ui'):
                            raise ValueError('Cold iOS entry permits only original readiness or system input')
                        if host_state != 'foreground' and operation not in ('set_app_state','system_ui'):
                            raise ValueError('Flutter app work is unavailable while background')
                        if app is not None and cold_entry is None: app.ensure_running(time.monotonic()+budget)
                        if operation == 'set_app_state':
                            allowed={'operation','generation','state'} | ({'call'} if config['platform']=='ios' else set())
                            if (set(request)!=allowed or app is None or request['generation']!=ready['owner']['generation']
                                    or request['state'] not in ('background','foreground') or request['state']==host_state):
                                raise ValueError('Flutter host transition differs from current standalone owner')
                            if config['platform']=='ios':
                                if system is None:raise ValueError('Host transition needs the independent system runner')
                                changed=app.set_app_state(directory/'app-state',request['state'],time.monotonic()+budget,system,request['call'])
                            else:
                                changed=app.set_app_state(directory/'app-state',request['state'],time.monotonic()+budget)
                            host_state=request['state']
                            write_json(directory/'command.log',dict(changed,ready=ready,state=host_state))
                        elif operation == 'begin_entry':
                            if (set(request) != {'operation', 'generation', 'url'} or app is None
                                    or config['platform'] != 'ios' or system is None
                                    or request['generation'] != ready['owner']['generation']):
                                raise ValueError('Cold iOS entry differs from current standalone owner')
                            cold_entry = ColdEntry(app, system, ready, host_number, directory,
                                                   time.monotonic()+budget, request['url'])
                            state.update(state='entry_pending', entry=cold_entry.started)
                            write_json(output/'status.json', state)
                        elif operation == 'poll_entry':
                            if set(request) != {'operation', 'generation', 'begin_command'} or cold_entry is None:
                                raise ValueError('Cold iOS readiness requires the original live dispatch')
                            result = cold_entry.poll(directory, time.monotonic()+budget,
                                                     request['generation'], request['begin_command'])
                            write_json(directory/'command.log', result)
                            if result['state'] == 'ready':
                                ready = result['replacement']['ready']
                                endpoint = result['replacement']['current']['endpoint']
                                write_json(output/'current-ready.json', ready)
                                cold_entry = None
                                state.pop('entry', None)
                                state['state'] = 'ready'
                                write_json(output/'status.json', state)
                        elif operation == 'restart_app':
                            url = request.pop('url',None)
                            if (set(request) != {'operation', 'generation'} or app is None
                                    or request['generation'] != ready['owner']['generation']):
                                raise ValueError('Flutter restart differs from current standalone owner')
                            entry = None
                            if url is not None:
                                if config['platform'] != 'android':raise ValueError('Cold Flutter URL replacement requires Android')
                                entry = app.entry_request(url,request['generation'])
                            previous = app.owner
                            replacement_deadline = time.monotonic()+budget
                            app.terminate(directory/'termination', replacement_deadline)
                            current = (app.start(directory/'launch', replacement_deadline, url=url) if url is not None
                                       else app.start(directory/'launch', replacement_deadline))
                            endpoint = {k:v for k,v in current['endpoint'].items() if k != 'scenario'}
                            if config['platform'] == 'android':endpoint['port'] = current['forwarding']['host_port']
                            ready = dict(ready, owner=dict(ready['owner'], generation=endpoint['generation']),
                                         endpoint=dict(port=endpoint['port'], token=endpoint['token']))
                            if system is not None: system.ensure_running()
                            result = dict(previous=previous, current=current, ready=ready)
                            if entry is not None:
                                accepted = json.loads((directory/'launch/entry.json').read_text())
                                if accepted['request'] != entry:raise ValueError('Cold Flutter URL receipt changed')
                                result['entry'] = accepted
                            write_json(directory/'command.log', result)
                            write_json(output/'current-ready.json', ready)
                        elif request.get('operation') == 'system_ui':
                            if set(request) != {'operation', 'call'} or system is None:
                                raise ValueError('System runner is unavailable')
                            reply = system.execute(request['call'], budget)
                            write_json(directory/'command.log', reply)
                        else:
                            argv = host_arguments(request, ready, adb)
                            recorded_command(directory, argv, budget)
                        write_json(directory/'reply.json', dict(number=host_number, error=None))
                    except BaseException as error:
                        write_json(directory/'reply.json', dict(number=host_number, error=f'{type(error).__name__}: {error}'))
                        raise
                    continue
                if command != {'command': 'stop'}:
                    raise ValueError('Unsupported Flutter session control')
                release_control()
                begin_retirement()
                retiring.set()
                # A suspended standalone app need not service its cooperative endpoint.
                # Native ownership is sufficient to terminate it and retire the device.
                if cold_entry is None and not (app is not None and host_state == 'background'):
                    reply = exchange(dict(endpoint=ready['endpoint'], request=dict(generation=endpoint['generation'],
                        id='owner-stop', operation='stop'), timeout_ms=10000))
                    write_json(output/'stop-response.json', reply)
                    if reply['http_status'] != 200 or reply['response'].get('status') != 'completed':
                        raise RuntimeError('Flutter stop was not completed')
                finish_deadline = time.monotonic()+45
                if system is not None: system.finish(finish_deadline)
                if app is not None and cold_entry is None:
                    app.terminate(output/'app-termination', finish_deadline, allow_exited=True)
                elif process is not None and process.wait(timeout=max(.001, finish_deadline-time.monotonic())):
                    raise RuntimeError('Flutter fixture did not finish cleanly')
                state['state'] = 'stopped'
                break
        except BaseException as error:
            release_control()
            state['failure_processes'] = dict(
                observed=now(),
                flutter_exit_code=None if process is None else process.poll(),
                system_exit_code=None if system is None or system.process is None else system.process.poll(),
                emulator_exit_code=None if emulator is None or emulator.process is None else emulator.process.poll())
            state.update(state='interrupted' if isinstance(error, WorkerInterrupted) else 'failed',
                         error=f'{type(error).__name__}: {error}', traceback=traceback.format_exc())
            if state['state'] == 'failed' and app is not None and config['platform'] == 'android':
                try:
                    app.record_runtime_failure(output/'failure-diagnostics', time.monotonic()+3)
                except BaseException as diagnostic_error:
                    state['diagnostic_error'] = f'{type(diagnostic_error).__name__}: {diagnostic_error}'
        finally:
            release_control()
            begin_retirement()
            retiring.set()
            reader.join(timeout=1)
            errors = []
            if resources_attempted and resources is None:
                errors.append('Independent resource startup outcome unresolved')
            if system is not None:
                try: system.retire()
                except BaseException as error: errors.append('System runner retirement: '+str(error))
            if reader.is_alive():
                errors.append('Control reader did not retire')
            if process is not None:
                try:
                    signal_owned_group(process.pid, signal.SIGTERM, log)
                    try:
                        process.wait(timeout=30)
                    except subprocess.TimeoutExpired:
                        pass
                    finally:
                        signal_owned_group(process.pid, signal.SIGKILL, log)
                        process.wait(timeout=5)
                except BaseException as error:
                    errors.append('Flutter process retirement: '+str(error))
            if log_capture is not None:
                try:
                    captured = log_capture.finish(log.with_name(log.name + '.output.json'))
                    if not captured['eof']:
                        errors.append('Flutter log output did not close after retirement')
                    if captured['error'] and state['state'] == 'stopped':
                        state.update(state='failed', error=captured['error'])
                except BaseException as error:
                    errors.append('Flutter log finalization: '+str(error))
            if log_stream is not None:
                log_stream.close()
            if app is not None and config['platform'] == 'android' and resources is None:
                try:app.retire_forwarding(time.monotonic()+20)
                except BaseException as error:errors.append('Standalone Android forwarding retirement: '+str(error))
            if forwarded is not None:
                try:
                    subprocess.run([str(adb), '-s', device, 'forward', '--remove', 'tcp:'+str(forwarded)],
                                   env=env, check=True, capture_output=True, timeout=10)
                    inventory = subprocess.run([str(adb), 'forward', '--list'], env=env, check=True,
                                               capture_output=True, text=True, timeout=10)
                    if any('tcp:'+str(forwarded) in line.split() for line in inventory.stdout.splitlines()):
                        raise RuntimeError('Owned forwarding still present')
                    write_json(output/'forwarding.json', dict(unresolved=False, retired=True, port=forwarded))
                except BaseException as error:
                    errors.append('Forwarding retirement: '+str(error))
            if forward_uncertain:
                errors.append('Forwarding creation outcome unknown')
            if resources is not None:
                try: resources.retire()
                except BaseException as error: errors.append('Independent resource retirement: '+str(error))
            if owns_device:
                try:
                    if emulator is not None:
                        cleanup = emulator.retire()
                    elif config['platform'] == 'ios':
                        if device is None:
                            raise RuntimeError('Simulator creation outcome unknown')
                        # Evidence storage failure must not skip the live owner's deletion.
                        # retire_simulator tolerates unavailable action logs independently.
                        try:
                            (output/'retirement').mkdir(exist_ok=True)
                        except OSError as error:
                            errors.append('Retirement log directory: '+str(error))
                        actions = retire_simulator(device, output/'retirement')
                        cleanup = inspect_cleanup('ios', output, env)
                        cleanup['actions'] = actions
                    else:
                        raise RuntimeError('Device ownership outcome unknown')
                    write_json(output/'cleanup.json', cleanup)
                    if not cleanup['passed']:
                        raise RuntimeError('Device retirement unverified')
                except BaseException as error:
                    errors.append('Device retirement: '+str(error))
                if data_storage is not None and not errors:
                    try:
                        data_storage.retire()
                    except BaseException as error:
                        errors.append('Simulator data volume retirement: '+str(error))
                if not errors:
                    try:
                        write_json(lock_dir/'native-session.json', dict(directory=str(output), retired=True))
                    except OSError as error:
                        errors.append('Host retirement record: '+str(error))
            state.update(active=None, finished=now(), retirement=dict(passed=not errors, errors=errors, signal_error=None))
            try:
                write_json(output/'status.json', state)
            except OSError as error:
                errors.append('Worker status publication: '+str(error))
                state['retirement']['passed'] = False
            if terminal is not None:
                terminal.publish(state)
            else:
                write_json(output/'result.json', state)
    return 0 if state['state'] == 'stopped' and state['retirement']['passed'] else 1


if __name__ == '__main__':
    sys.exit(main())
