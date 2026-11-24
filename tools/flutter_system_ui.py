"""System-only XCTest child of the live Flutter SDK worker; never acquires a second device."""
import hashlib
import json
from pathlib import Path
import signal
import subprocess
import time

from native_ios_ui_bridge import Bridge
from native_ios_clock_bridge import Bridge as ClockBridge
from native_ios_generic_ui import UIKitUIRequest
from native_ios_ui import UIClient
from native_ios_transport import exchange
from native_host_process import signal_owned_group, write_json


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def validate_products(config):
    if type(config) is not dict or set(config) != {'xcodebuild', 'products', 'manifest'}:
        raise ValueError('Invalid system runner configuration')
    root = Path(config['products']); manifest = config['manifest']
    if (not root.is_absolute() or root.is_symlink() or not Path(config['xcodebuild']).is_absolute()
            or manifest.get('role') != 'external_runner'
            or manifest.get('names') != dict(app='SystemUI', tests='SystemUITests')
            or set(manifest) != {'role', 'names', 'files', 'directories', 'xctestrun'}):
        raise ValueError('Invalid frozen system runner products')
    files = {}; directories = set()
    for path in root.rglob('*'):
        if path.is_symlink(): raise ValueError('Linked system runner product')
        name = str(path.relative_to(root))
        if path.is_dir(): directories.add(name)
        elif path.is_file(): files[name] = path
        else: raise ValueError('Special system runner product')
    if set(files) != set(manifest['files']) or directories != set(manifest['directories']):
        raise ValueError('System runner product inventory changed')
    if manifest['xctestrun'] not in files or not manifest['xctestrun'].endswith('.xctestrun'):
        raise ValueError('Missing frozen test configuration')
    plugin = 'Debug-iphonesimulator/SystemUITests-Runner.app/PlugIns/SystemUITests.xctest/SystemUITests'
    if plugin not in files: raise ValueError('Missing system test executable')
    for name, path in files.items():
        expected = manifest['files'][name]
        if (path.stat().st_size != expected['bytes'] or digest(path) != expected['sha256']
                or bool(path.stat().st_mode & 0o111) != expected['executable']):
            raise ValueError('System runner product bytes or mode changed')


def validate_runner_process(process, pid, installed):
    fields = process.split(None, 6)
    executable = str(installed/'SystemUITests-Runner')
    if (not installed.is_absolute() or len(fields) != 7 or fields[0] != str(pid)
            or not (fields[6] == executable or fields[6].startswith(executable+' '))):
        raise ValueError('System endpoint is not the owned Simulator runner')


class SystemRunner:
    def __init__(self, config, device, output, env, source, recorded_command, resources=None):
        self.resources = resources
        self.config, self.device, self.output, self.env = config, device, output, env
        self.source, self.recorded_command = source, recorded_command
        self.process = None
        self.identity = self.descriptor = None
        output.mkdir()
        write_json(output/'launch.json', config)

    def start(self, deadline):
        products = Path(self.config['products'])
        args = [self.config['xcodebuild'], 'test-without-building', '-xctestrun',
                str(products/self.config['manifest']['xctestrun']), '-destination',
                'platform=iOS Simulator,id='+self.device, '-parallel-testing-enabled', 'NO',
                '-collect-test-diagnostics', 'never', '-only-testing:SystemUITests/SystemUITests/testSystemBridge',
                '-resultBundlePath', str(self.output/'test.xcresult')]
        write_json(self.output/'command.json', args)
        if self.resources is not None:
            self.process = self.resources.spawn(args, self.env, self.output)
        else:
            with (self.output/'xctest.log').open('wb') as log:
                self.process = subprocess.Popen(args, env=self.env, stdout=log, stderr=subprocess.STDOUT,
                                                stdin=subprocess.DEVNULL, start_new_session=True)
        write_json(self.output/'process.json', dict(pid=self.process.pid))
        def remaining(limit):
            left = deadline-time.monotonic()
            if left <= 0: raise TimeoutError('System runner acquisition expired')
            return min(limit, left)
        def command(name, args):
            return self.recorded_command(self.output/name, args, remaining(10)).decode().strip()
        attempt = 0
        while True:
            remaining(1); self.ensure_running(); attempt += 1
            try:
                container = Path(command('container-'+str(attempt), ['xcrun', 'simctl', 'get_app_container',
                    self.device, 'dev.frontcap.system-ui.tests.xctrunner', 'data']))
                candidate = container/'Documents/system-ui/ui-control.json'
                if candidate.is_file(): self.descriptor = candidate; break
            except RuntimeError:
                pass  # Runner installation may not have completed yet; bounded discovery only.
            time.sleep(min(.2, remaining(.2)))
        endpoint = json.loads(self.descriptor.read_text())
        if (endpoint.get('system_only') is not True or endpoint.get('system_alerts') is not True
                or endpoint.get('status') != 'ready' or endpoint.get('last_id') != 0):
            raise ValueError('System runner is not a fresh system-only endpoint')
        identity = dict(simulator=self.device, generation=endpoint['generation'], pid=endpoint['pid'])
        process = command('identity', ['ps', '-p', str(identity['pid']), '-o', 'pid=,lstart=,command='])
        installed = Path(command('installed', ['xcrun', 'simctl', 'get_app_container', self.device,
                                              'dev.frontcap.system-ui.tests.xctrunner', 'app']))
        validate_runner_process(process, identity['pid'], installed)
        plugin = 'PlugIns/SystemUITests.xctest/SystemUITests'
        expected = self.config['manifest']['files']['Debug-iphonesimulator/SystemUITests-Runner.app/'+plugin]['sha256']
        if digest(installed/plugin) != expected: raise ValueError('Installed system runner executable changed')
        self.identity = identity
        write_json(self.output/'host-app-state.json', endpoint.get('host_app_state') is True)
        bridge = Bridge(self.descriptor, self.output/'client', timeout=remaining(60), system_only=True)
        reply = bridge.handle(dict(call='initialize', identity=identity, source_sha256=self.source))
        write_json(self.output/'initialized.json', reply)
        write_json(self.output/'ready.json', dict(identity=identity, descriptor=str(self.descriptor)))
        self.ensure_running()
        return identity

    def ensure_running(self):
        if self.process is None or self.process.poll() is not None:
            raise RuntimeError('Owned system runner exited')

    def execute(self, call, timeout):
        self.ensure_running()
        if (type(call) is not dict or set(call) != {'call', 'identity', 'app', 'request'}
                or call['call'] not in ('execute', 'recover') or call['identity'] != self.identity
                or call['app'] is not None):
            raise ValueError('System request differs from the owned runner')
        if call['request']['operation'] == 'set_host_app_state':
            if json.loads((self.output/'host-app-state.json').read_text()) is not True:
                raise ValueError('System runner does not support host visibility')
        reply = Bridge(self.descriptor, self.output/'client', timeout=min(timeout, 60), system_only=True).handle(call)
        if reply['reply'] != 'completed': raise RuntimeError('System UI outcome unresolved')
        folder = self.output/'results'; folder.mkdir(exist_ok=True)
        path = folder/(str(call['request']['request_id'])+'.json')
        if path.exists() and json.loads(path.read_text()) != reply:
            raise RuntimeError('System UI saved result changed')
        write_json(path, reply)  # Durable result before worker reply publication.
        return reply

    def finish(self, deadline):
        self.ensure_running()
        def remaining():
            budget = deadline-time.monotonic()
            if budget <= 0: raise TimeoutError("System runner finish deadline elapsed")
            return budget
        cursor = json.loads((self.output/'client/ui/ui-client.json').read_text())
        if cursor['pending'] is not None: raise RuntimeError('Uncertain system input cannot be finished')
        request = UIKitUIRequest(self.identity['generation'], cursor['next_id'], 'finish', {})
        client = UIClient(self.descriptor, self.output/'client/ui',
                          lambda endpoint, request, **_: exchange(endpoint, request, timeout=min(20, remaining())),
                          request_type=UIKitUIRequest,
                          validate_owner=lambda receipt: ClockBridge.same_app(receipt, self.identity))
        write_json(self.output/'finish.json', client.request('finish', expected_request=request))
        if self.process.wait(timeout=remaining()): raise RuntimeError('System runner did not finish cleanly')

    def retire(self):
        if self.resources is not None and self.process is not None:
            self.process.retire()
            return
        if self.process is not None and self.process.poll() is None:
            signal_owned_group(self.process.pid, signal.SIGTERM, self.output/'xctest.log')
            try: self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                signal_owned_group(self.process.pid, signal.SIGKILL, self.output/'xctest.log')
                self.process.wait(timeout=5)
