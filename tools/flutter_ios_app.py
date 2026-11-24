"""Same-install Flutter app lifetime inside a caller's live owned Simulator.

This bridge neither acquires devices nor installs apps. Construct it only under the current worker's
device lease. Saved process/endpoint records are evidence, never authority to reconstruct this object.
"""
import json
from datetime import datetime
from pathlib import Path
import re
import subprocess
import time
import uuid

from flutter_host_commands import app_name, bundle_inventory, installed_bundle_difference, read_plist, host_arguments
from native_host_process import execute, write_json


class IosApp:
    def __init__(self, simulator, product, inventory, output, env, executor=None):
        self.executor = executor or execute
        self.simulator = str(uuid.UUID(simulator)).upper()
        self.product, self.inventory, self.output, self.env = product, inventory, output, env
        if bundle_inventory(product) != inventory:
            raise ValueError('Frozen Flutter product changed')
        info = read_plist(product/'Info.plist')
        self.bundle = app_name(info['CFBundleIdentifier'], 'ios')
        self.executable = info['CFBundleExecutable']
        if (type(self.executable) is not str or self.executable in ('', '.', '..')
                or Path(self.executable).name != self.executable):
            raise ValueError('Invalid Flutter app executable')
        self.owner = self.previous = self.pending_entry = None
        self.uncertain = False
        self.number = 0
        self.initial_installation = None
        output.mkdir()
        write_json(output/'product.json', dict(path=str(product), inventory=inventory))

    @staticmethod
    def remaining(deadline):
        left = deadline-time.monotonic()
        if left <= 0: raise TimeoutError('Flutter installed-app deadline elapsed')
        return left

    def _command(self, name, argv, deadline, absent_ok=False):
        self.number += 1
        directory = self.output/('command-'+str(self.number));directory.mkdir()
        write_json(directory/'request.json', dict(name=name, argv=argv))
        code = self.executor(argv, directory/'stdout.log', self.env, min(20, self.remaining(deadline)), self.output)
        write_json(directory/'exit.json', dict(exit_code=code))
        log = directory/'stdout.log'
        if log.stat().st_size > 4*1024*1024: raise ValueError('Flutter host output exceeds bound')
        raw = log.read_text(errors='strict').strip()
        if code and not (absent_ok and code == 1 and not raw):
            raise RuntimeError(name+' failed: '+str(code))
        return raw

    def _process(self, pid, deadline, record=True):
        argv = ['ps', '-p', str(pid), '-o', 'lstart=,command=']
        if record:
            raw = self._command('process', argv, deadline, absent_ok=True)
        else:
            # Idle health checks must not grow an immutable command tree every polling interval.
            result = subprocess.run(argv, capture_output=True, text=True, env=self.env,
                                    timeout=min(2, self.remaining(deadline)))
            raw = result.stdout.strip()
            if result.stderr or result.returncode not in (0, 1) or (result.returncode and raw):
                raise RuntimeError('Flutter process health query failed')
        if not raw: return None
        values = raw.split(None, 5)
        if len(values) != 6: raise ValueError('Invalid app process evidence')
        return dict(pid=pid, birth=' '.join(values[:5]), command=values[5])

    def _container_paths(self, deadline):
        paths = {}
        for kind in ('app', 'data'):
            path = Path(self._command('container-'+kind,
                ['xcrun', 'simctl', 'get_app_container', self.simulator, self.bundle, kind], deadline))
            if not path.is_absolute() or path.is_symlink() or not path.is_dir():
                raise ValueError('Invalid installed Flutter container')
            paths[kind] = str(path)
        stat = Path(paths['data']).stat()
        paths['data_device'], paths['data_inode'] = stat.st_dev, stat.st_ino
        return paths

    def _containers(self, deadline):
        paths = self._container_paths(deadline)
        actual = bundle_inventory(Path(paths['app']))
        difference, modes = installed_bundle_difference(self.product, self.inventory, actual)
        if any(difference.values()):
            write_json(self.output/('bundle-difference-'+str(self.number)+'.json'), difference)
            raise ValueError('Installed Flutter bundle differs: '+json.dumps(difference))
        paths['installed_library_modes'] = modes
        return paths

    def require_fresh(self, deadline):
        raw = self._command('fresh-apps', ['xcrun','simctl','listapps',self.simulator], deadline)
        # simctl emits an ASCII plist; convert through the same bounded owner.
        path = self.output/'fresh-apps.plist'
        path.write_text(raw)
        self._command('fresh-apps-xml', ['plutil','-convert','xml1',str(path)], deadline)
        apps = read_plist(path)
        if not isinstance(apps, dict):raise ValueError('Invalid installed app inventory')
        if self.bundle in apps:raise ValueError('Flutter initial data requires a fresh installation')

    def prelaunch_identity(self, deadline):
        if self.owner is not None or self.previous is not None or self.pending_entry is not None or self.uncertain:
            raise RuntimeError('Flutter initial data requires an unstarted app')
        containers = self._containers(deadline)
        if self._entry_process(containers, deadline) is not None:
            raise ValueError('Flutter initial-data app already running')
        return dict(platform='ios', simulator=self.simulator.lower(), bundle=self.bundle, containers=containers)

    def ensure_running(self, deadline, record=True):
        if self.uncertain or self.owner is None:
            raise RuntimeError('Flutter app ownership is unavailable')
        if self._process(self.owner['process']['pid'], deadline, record) != self.owner['process']:
            raise RuntimeError('Owned Flutter process changed')

    def _read_endpoint(self, directory, process, started, deadline):
        endpoint = None
        logs = []
        for name in ('stdout.log', 'stderr.log'):
            path = directory/name
            if not path.exists(): continue
            if path.stat().st_size > 8*1024*1024: raise ValueError('Flutter startup log exceeds bound')
            logs.append(path.read_text(errors='replace'))
        marker = 'FRONTCAP_FLUTTER_ENDPOINT '
        if not any(marker in value for value in logs):
            # Flutter's iOS engine writes through unified logging rather than POSIX stdout.
            # Bound discovery to this live process and launch, never a prior saved endpoint.
            logs.append(self._command('endpoint-log', ['xcrun', 'simctl', 'spawn', self.simulator,
                'log', 'show', '--style', 'compact', '--start', started, '--predicate',
                'processID == '+str(process['pid'])+' AND eventMessage CONTAINS "'+marker+'"'], deadline))
        for log in logs:
            for line in log.splitlines():
                if marker not in line: continue
                prefix = line.split(marker, 1)[0]
                if prefix and not prefix.rstrip().endswith('flutter:'): continue
                candidate = json.loads(line.split(marker, 1)[1])
                if (set(candidate) != {'generation', 'token', 'port', 'scenario'} or candidate['scenario'] != 'session'
                        or any(type(candidate[k]) is not str or not re.fullmatch('[a-f0-9]{48}', candidate[k]) for k in ('generation', 'token'))
                        or type(candidate['port']) is not int or not 0 < candidate['port'] < 65536):
                    raise ValueError('Invalid installed Flutter endpoint')
                if endpoint is not None and endpoint != candidate: raise ValueError('Multiple Flutter endpoint identities')
                endpoint = candidate
        return endpoint

    def start(self, directory, deadline):
        if self.uncertain or self.owner is not None or self.pending_entry is not None:
            raise RuntimeError('Flutter app launch cannot be repeated or adopted')
        directory.mkdir()
        containers = self._containers(deadline)
        if self.initial_installation is not None and containers != self.initial_installation:
            raise RuntimeError('Flutter first launch changed its prepared container')
        if self.previous is not None and containers != self.previous['containers']:
            raise RuntimeError('Flutter replacement changed the installed container')
        started = datetime.now().strftime('%Y-%m-%d %H:%M:%S')
        write_json(directory/'intent.json', dict(previous=self.previous, containers=containers, started=started))
        self.uncertain = True  # Every publication/effect failure requires retirement, not replay.
        argv = ['xcrun', 'simctl', 'launch', '--stdout='+str(directory/'stdout.log'),
                '--stderr='+str(directory/'stderr.log'), self.simulator, self.bundle,
                '--enable-dart-profiling', '--disable-vm-service-publication', '--enable-checked-mode', '--verify-entry-points']
        raw = self._command('launch', argv, deadline)
        match = re.fullmatch(re.escape(self.bundle)+r': ([1-9][0-9]*)', raw)
        if not match or int(match[1]) > 2**31-1: raise ValueError('Unconfirmed Flutter launch PID')
        process = self._process(int(match[1]), deadline)
        executable = containers['app']+'/'+self.executable
        if process is None or not (process['command'] == executable or process['command'].startswith(executable+' ')):
            raise ValueError('Flutter launch process differs from installed app')
        if self.previous is not None and process == self.previous['process']:
            raise ValueError('Flutter restart did not replace the process')
        write_json(directory/'process.json', process)
        endpoint = None
        while endpoint is None:
            self.remaining(deadline)
            if self._process(process['pid'], deadline) != process:
                raise RuntimeError('Flutter app exited before its control endpoint')
            endpoint = self._read_endpoint(directory, process, started, deadline)
            if endpoint is None: time.sleep(min(.2, self.remaining(deadline)))
        if self.previous is not None and endpoint['generation'] == self.previous['endpoint']['generation']:
            raise ValueError('Flutter restart retained its old generation')
        if self._containers(deadline) != containers or self._process(process['pid'], deadline) != process:
            raise ValueError('Flutter acquisition owner changed')
        self.owner = dict(simulator=self.simulator, bundle=self.bundle, containers=containers, process=process, endpoint=endpoint)
        write_json(directory/'ready.json', self.owner)
        self.uncertain = False
        return self.owner

    def set_app_state(self, directory, state, deadline, system, call):
        self.ensure_running(deadline)
        before=dict(self.owner)
        expected=dict(bundle=self.bundle,pid=before['process']['pid'],state=state)
        if (state not in ('background','foreground') or call.get('call') != 'execute'
                or call.get('identity') != system.identity or call.get('app') is not None
                or call.get('request',{}).get('operation') != 'set_host_app_state'
                or call['request']['arguments'] != expected):
            raise ValueError('Host visibility request differs from the acquired Flutter app')
        if self._containers(deadline) != before['containers']:raise RuntimeError('Flutter containers changed')
        directory.mkdir();write_json(directory/'before.json',before)
        self.uncertain=True
        reply=system.execute(call,self.remaining(deadline))
        if self._process(before['process']['pid'],deadline) != before['process'] or self._containers(deadline) != before['containers']:
            raise RuntimeError('Flutter iOS host transition changed its process or installation')
        after=dict(self.owner)
        native=dict(platform='ios',completion=reply['completion'])
        write_json(directory/'after.json',after);write_json(directory/'native.json',native)
        self.uncertain=False
        return dict(before=before,after=after,native=native)

    def entry_request(self, url, generation):
        request = dict(target=dict(platform='ios', simulator=self.simulator.lower(), bundle=self.bundle),
                       generation=generation, url=url)
        ready = dict(owner=dict(generation=generation), external_url_target=request['target'])
        host_arguments(dict(operation='open_url', request=request), ready, None)
        return request

    def _entry_process(self, containers, deadline):
        executable = containers['app']+'/'+self.executable
        raw = self._command('process-inventory', ['ps', '-axo', 'pid=,lstart=,command='], deadline)
        matches = []
        for line in raw.splitlines():
            fields = line.split(None, 6)
            if len(fields) != 7 or not (fields[6] == executable or fields[6].startswith(executable+' ')):
                continue
            if not re.fullmatch('[1-9][0-9]*', fields[0]) or int(fields[0]) > 2**31-1:
                raise ValueError('Invalid cold-entry process identity')
            process = dict(pid=int(fields[0]), birth=' '.join(fields[1:6]), command=fields[6])
            if self._process(process['pid'], deadline) != process:
                raise RuntimeError('Cold-entry process changed during discovery')
            matches.append(process)
        if len(matches) > 1: raise RuntimeError('Multiple processes match the installed cold-entry app')
        return matches[0] if matches else None

    def begin_entry(self, directory, deadline, url):
        """After caller-owned scheme admission and termination, dispatch one cold URL.

        The caller retains its Simulator lease and independent system runner across this phase.
        No confirmation is chosen here, and a saved pending record cannot reconstruct authority.
        """
        if self.uncertain or self.owner is not None or self.previous is None or self.pending_entry is not None:
            raise RuntimeError('Cold entry requires the retired original app and no unresolved launch')
        request = self.entry_request(url, self.previous['endpoint']['generation'])
        containers = self._containers(deadline)
        if containers != self.previous['containers'] or self._entry_process(containers, deadline) is not None:
            raise RuntimeError('Cold-entry installation changed or already has a process')
        directory.mkdir()
        started = datetime.now().strftime('%Y-%m-%d %H:%M:%S')
        pending = dict(previous=self.previous, containers=containers, started=started, request=request)
        write_json(directory/'intent.json', pending)
        self.uncertain = True
        ready = dict(owner=dict(generation=request['generation']), external_url_target=request['target'])
        self._command('open-url', host_arguments(dict(operation='open_url', request=request), ready, None), deadline)
        entry = dict(request=request, confirmation='unobserved', destination='unobserved')
        write_json(directory/'entry.json', entry)
        self.pending_entry = dict(pending, directory=directory, entry=entry, process=None)
        self.uncertain = False
        return entry

    def poll_entry(self, directory, deadline):
        """Sample the original URL's new process/endpoint without any launch or URL effect."""
        if self.uncertain or self.owner is not None or self.pending_entry is None:
            raise RuntimeError('No live cold-entry authority')
        pending = self.pending_entry
        directory.mkdir()
        self.uncertain = True
        paths = self._container_paths(deadline)
        containers = pending['containers']
        if any(containers[key] != value for key, value in paths.items()):
            raise RuntimeError('Cold-entry container changed')
        process = self._entry_process(containers, deadline)
        if pending['process'] is not None and process != pending['process']:
            raise RuntimeError('Original cold-entry process exited or changed')
        if process is None:
            result = dict(state='waiting', phase='process')
        else:
            if process == pending['previous']['process']:
                raise RuntimeError('Cold entry retained the retired process')
            if pending['process'] is None:
                pending['process'] = process
                write_json(pending['directory']/'process.json', process)
            endpoint = self._read_endpoint(pending['directory'], process, pending['started'], deadline)
            if endpoint is None:
                result = dict(state='waiting', phase='endpoint', process=process)
            else:
                if endpoint['generation'] == pending['previous']['endpoint']['generation']:
                    raise ValueError('Cold entry retained the original endpoint generation')
                if self._containers(deadline) != containers or self._process(process['pid'], deadline) != process:
                    raise RuntimeError('Cold-entry owner changed before readiness')
                self.owner = dict(simulator=self.simulator, bundle=self.bundle, containers=containers,
                                  process=process, endpoint=endpoint)
                result = dict(state='ready', application=self.owner, entry=pending['entry'])
                write_json(pending['directory']/'ready.json', self.owner)
                self.pending_entry = None
        write_json(directory/'result.json', result)
        self.uncertain = False
        return result

    def terminate(self, directory, deadline, allow_exited=False):
        if self.uncertain or self.owner is None:
            raise RuntimeError('Flutter app ownership is unavailable')
        current = self._process(self.owner['process']['pid'], deadline)
        if current != self.owner['process'] and not (allow_exited and current is None):
            raise RuntimeError('Owned Flutter process changed')
        directory.mkdir()
        previous = self.owner
        write_json(directory/'intent.json', previous)
        self.uncertain = True
        if current is not None:
            self._command('terminate', ['xcrun', 'simctl', 'terminate', self.simulator, self.bundle], deadline)
        while self._process(previous['process']['pid'], deadline) == previous['process']:
            time.sleep(min(.05, self.remaining(deadline)))
        if self._containers(deadline) != previous['containers']:
            raise ValueError('Flutter termination changed its installation')
        write_json(directory/'terminated.json', previous)
        self.previous, self.owner, self.uncertain = previous, None, False
        return previous
