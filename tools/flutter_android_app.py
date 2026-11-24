"""Same-install Flutter APK lifetime inside the caller's live owned emulator.

No build, installation, device acquisition or saved-owner adoption happens here. The existing
controlled session reports its endpoint through ordinary Android logging after each OS launch.
"""
import json
import os
import socket
from pathlib import Path
import re
import subprocess
import time
import uuid

from flutter_host_commands import app_name, digest, host_arguments
from native_exchange_streams import read_exchange
from native_command_capture import capture as capture_command
from native_host_process import execute, write_json


def validate_product(spec):
    if not isinstance(spec, dict) or set(spec) != {'product', 'apk_sha256', 'package', 'activity'}:
        raise ValueError('Invalid standalone Flutter APK configuration')
    product = Path(spec['product'])
    if (not product.is_absolute() or product.is_symlink() or product.suffix != '.apk'
            or not re.fullmatch('[a-f0-9]{64}', spec['apk_sha256']) or digest(product) != spec['apk_sha256']):
        raise ValueError('Frozen standalone Flutter APK changed')
    app_name(spec['package'], 'android');app_name(spec['activity'], 'android')
    return product


class AndroidApp:
    def __init__(self, serial, spec, output, env, adb, *, resource_group=None, emulator=None, server_port=None, confine_adb=False):
        validate_product(spec)
        if not re.fullmatch(r'emulator-[0-9]+', serial):raise ValueError('Invalid owned Android serial')
        self.serial, self.spec, self.output, self.env, self.adb = serial, spec, output, env, adb
        if resource_group is not None and (emulator is None or type(server_port) is not int
                or not 1024 <= server_port <= 65535 or server_port == 5037
                or env.get('ADB_SERVER_SOCKET') != f'tcp:127.0.0.1:{server_port}'):
            raise ValueError('Supervised Android app requires its original emulator and private ADB server')
        if confine_adb and resource_group is None:
            raise ValueError('ADB confinement requires its original resource group')
        self.confine_adb = confine_adb
        self.resource_group, self.emulator, self.server_port = resource_group, emulator, server_port
        self.package, self.activity = spec['package'], spec['activity']
        self.owner = self.previous = self.forwarded = None
        self.uncertain = self.forward_uncertain = self.remove_uncertain = False
        self.number = 0
        self.initial_installation = None
        output.mkdir();write_json(output/'product.json', spec)

    @staticmethod
    def remaining(deadline):
        left = deadline-time.monotonic()
        if left <= 0:raise TimeoutError('Flutter Android app deadline elapsed')
        return left

    def _command(self, name, args, deadline, absent_ok=False, record=True):
        argv = [str(self.adb), '-s', self.serial, *args]
        timeout = min(20 if record else 2, self.remaining(deadline))
        if record or self.resource_group is not None:
            self.number += 1
            directory = self.output/('command-'+str(self.number));directory.mkdir()
            if self.resource_group is None:
                write_json(directory/'request.json', dict(name=name, argv=argv))
                code = execute(argv, directory/'stdout.log', self.env, timeout, self.output)
                write_json(directory/'exit.json', dict(exit_code=code))
                path = directory/'stdout.log'
                if path.stat().st_size > 4*1024*1024:raise ValueError('Flutter Android output exceeds bound')
                raw = path.read_text(errors='strict').strip()
            else:
                evidence = directory/'capture'
                if name == 'forward':
                    if len(args) != 4 or args[:2] != ['forward','--no-rebind']:
                        raise ValueError('Supervised forwarding requires an explicit original bind')
                    self.resource_group.register_forward(f'flutter-forward-{self.number}', adb=self.adb,
                        emulator=self.emulator, serial=self.serial, socket=args[3], port=int(args[2][4:]),
                        evidence=evidence, server_port=self.server_port, timeout=timeout,
                        write_root=self.resource_group.ready['temporary_disks'] if self.confine_adb else None)
                exchange = self._capture(argv, evidence, min(timeout, self.remaining(deadline)), 4*1024*1024)
                code = exchange['exit_code']
                raw = bytes(exchange['stdout']).decode('utf-8', errors='strict').strip()
                if not record and exchange['stderr']:
                    raise RuntimeError('Flutter Android health query failed')
        else:
            reply = subprocess.run(argv, capture_output=True, text=True, env=self.env, timeout=timeout)
            code, raw = reply.returncode, reply.stdout.strip()
            if reply.stderr:raise RuntimeError('Flutter Android health query failed')
        if code and not (absent_ok and code == 1 and not raw):raise RuntimeError(name+' failed: '+str(code))
        return raw

    def _capture(self, argv, evidence, timeout, output_limit):
        overrides = {key: value for key, value in self.env.items() if os.environ.get(key) != value}
        overrides['ADB_SERVER_SOCKET'] = f'tcp:127.0.0.1:{self.server_port}'
        path = self.resource_group.capture(argv, evidence, directory=self.output,
            timeout=timeout, environment=overrides, output_limit=output_limit, confine_adb=self.confine_adb)
        exchange = read_exchange(path, path.read_bytes())
        if exchange['cleanup_failure'] or exchange['evidence_failure']:
            raise RuntimeError('Original Android helper retirement or evidence unresolved')
        # Only a completed nonzero command may reach absent_ok handling. Cancellation,
        # truncation or timeout must remain failed even if the child eventually exits zero.
        if exchange['failure'] and not (exchange['failure'] == 'bridge exited unsuccessfully'
                and type(exchange['exit_code']) is int and exchange['exit_code'] != 0):
            raise RuntimeError('Original Android helper failed: '+exchange['failure'])
        return exchange

    @staticmethod
    def device_path(value):
        if (not re.fullmatch(r'/[A-Za-z0-9/._+=@~-]+', value)
                or any(part in ('.', '..') for part in value.split('/'))):
            raise ValueError('Invalid Android device path')
        return value

    def _installation(self, deadline):
        rows = self._command('package-path', ['shell','pm','path',self.package], deadline).splitlines()
        if len(rows) != 1 or not rows[0].startswith('package:/'):raise ValueError('Flutter requires one installed APK')
        apk = self.device_path(rows[0][len('package:'):])
        hashed = self._command('package-hash', ['shell','sha256sum',apk], deadline).split()
        if hashed != [self.spec['apk_sha256'], apk]:raise ValueError('Installed Flutter APK differs')
        data = self.device_path(self._command('data-path', ['shell','run-as',self.package,'pwd'], deadline))
        stat = self._command('data-identity', ['shell','run-as',self.package,'stat','-c',"'%d %i'",'.'], deadline).split()
        if len(stat) != 2 or any(not re.fullmatch('[0-9]+', x) for x in stat) or int(stat[1]) == 0 or Path(data).name != self.package:
            raise ValueError('Invalid Android app data identity')
        return dict(package=self.package, apk=apk, apk_sha256=self.spec['apk_sha256'], data=data,
                    data_device=int(stat[0]), data_inode=int(stat[1]))

    def require_fresh(self, deadline):
        rows = self._command('fresh-packages', ['shell','pm','list','packages'], deadline).splitlines()
        if any(not re.fullmatch(r'package:[A-Za-z0-9_.]+', row) for row in rows):
            raise ValueError('Invalid installed package inventory')
        if 'package:'+self.package in rows:
            raise ValueError('Flutter initial data requires a fresh installation')

    def prelaunch_identity(self, deadline):
        if self.owner is not None or self.previous is not None or self.uncertain:
            raise RuntimeError('Flutter initial data requires an unstarted app')
        rows = self._command('unstarted-processes', ['shell','ps','-A','-o','NAME'], deadline).splitlines()
        if not rows or rows[0].strip() != 'NAME':raise ValueError('Invalid process inventory')
        if any(row.strip() == self.package or row.strip().startswith(self.package+':') for row in rows[1:]):
            raise ValueError('Flutter initial-data app already running')
        boot = self._command('initial-boot', ['shell','cat','/proc/sys/kernel/random/boot_id'], deadline)
        if str(uuid.UUID(boot)) != boot:raise ValueError('Invalid boot identity')
        return dict(platform='android', serial=self.serial, boot_id=boot, installation=self._installation(deadline))

    @staticmethod
    def start_ticks(raw, pid):
        prefix, rest = raw.rsplit(') ', 1);fields = rest.split()
        if not prefix.startswith(str(pid)+' (') or len(fields) < 20 or fields[0] in ('Z','X','x'):
            raise ValueError('Android process is not live')
        ticks = int(fields[19])
        if ticks <= 0:raise ValueError('Invalid Android process birth')
        return ticks

    def _process(self, deadline, record=True):
        def command(name, args, absent_ok=False):return self._command(name,args,deadline,absent_ok,record)
        raw = command('pid', ['shell','pidof',self.package], True)
        if not raw:return None
        if not re.fullmatch('[1-9][0-9]*', raw) or int(raw) > 2**31-1:raise ValueError('Ambiguous Android app PID')
        pid = int(raw)
        boot = command('boot-id', ['shell','cat','/proc/sys/kernel/random/boot_id'])
        if str(uuid.UUID(boot)) != boot:raise ValueError('Invalid Android boot identity')
        args = ['shell','run-as',self.package,'cat','/proc/'+raw+'/stat']
        birth = self.start_ticks(command('process-birth', args), pid)
        name = command('process-name', ['shell','run-as',self.package,'cat','/proc/'+raw+'/cmdline']).rstrip('\0')
        if name != self.package or self.start_ticks(command('process-birth-after',args),pid) != birth:
            raise ValueError('Flutter Android process changed during inspection')
        return dict(pid=pid,boot_id=boot,start_ticks=birth,command=name)

    def ensure_running(self, deadline, record=True):
        if self.uncertain or self.owner is None or self._process(deadline, record) != self.owner['process']:
            raise RuntimeError('Owned Flutter Android process is unavailable or changed')

    def _forward(self, port, deadline):
        if self.forwarded is not None or self.forward_uncertain:raise RuntimeError('Prior Android forwarding unresolved')
        self.forward_uncertain = True
        write_json(self.output/'forwarding.json', dict(unresolved=True,device_port=port))
        if self.resource_group is None:
            raw = self._command('forward', ['forward','tcp:0','tcp:'+str(port)],deadline)
        else:
            # Reserve a candidate locally, then let ADB --no-rebind reject any race without
            # replacing a foreign mapping. Registration precedes the one original bind.
            with socket.socket() as reservation:
                reservation.bind(('127.0.0.1', 0))
                raw = str(reservation.getsockname()[1])
            self._command('forward', ['forward','--no-rebind','tcp:'+raw,'tcp:'+str(port)],deadline)
        if not re.fullmatch('[1-9][0-9]*',raw) or int(raw)>65535:raise ValueError('Unknown Android forwarding port')
        self.forwarded = dict(serial=self.serial,host_port=int(raw),device_port=port)
        expected = [self.serial,'tcp:'+raw,'tcp:'+str(port)]
        rows = [r.split() for r in self._command('forward-list',['forward','--list'],deadline).splitlines()]
        if [r for r in rows if len(r)>1 and r[1]==expected[1]] != [expected]:raise ValueError('Android forwarding owner differs')
        self.forward_uncertain = False
        write_json(self.output/'forwarding.json',dict(unresolved=False,forwarding=self.forwarded))
        return dict(self.forwarded)

    def retire_forwarding(self, deadline):
        if self.forwarded is None:
            if self.forward_uncertain:raise RuntimeError('Android forwarding allocation outcome unknown')
            return
        mapping = self.forwarded
        host, guest = 'tcp:'+str(mapping['host_port']), 'tcp:'+str(mapping['device_port'])
        def matches():
            rows = [r.split() for r in self._command('forward-list',['forward','--list'],deadline).splitlines()]
            found = [r for r in rows if len(r)>1 and r[1]==host]
            if found and found != [[self.serial,host,guest]]:raise ValueError('Android forwarding changed owner')
            return bool(found)
        if matches():
            if self.remove_uncertain:raise RuntimeError('Android forwarding removal unresolved; no redispatch')
            self.remove_uncertain = True
            self._command('forward-remove',['forward','--remove',host],deadline)
            if matches():raise RuntimeError('Android forwarding remained after removal')
        write_json(self.output/'forwarding.json',dict(unresolved=False,retired=True,forwarding=mapping))
        self.forwarded = None
        self.forward_uncertain = self.remove_uncertain = False

    def entry_request(self, url, generation):
        request = dict(target=dict(platform='android',serial=self.serial,package=self.package),generation=generation,url=url)
        ready = dict(owner=dict(generation=generation),external_url_target=request['target'])
        host_arguments(dict(operation='open_url',request=request),ready,self.adb)
        return request

    def start(self, directory, deadline, url=None):
        if self.uncertain or self.owner is not None:raise RuntimeError('Flutter launch cannot be repeated or adopted')
        entry = None
        args = ['shell','am','start','-W','-n',self.package+'/'+self.activity]
        if url is not None:
            if self.previous is None:raise ValueError('Cold URL entry requires the retired original owner')
            entry = self.entry_request(url,self.previous['endpoint']['generation'])
            ready = dict(owner=dict(generation=entry['generation']),external_url_target=entry['target'])
            args = host_arguments(dict(operation='open_url',request=entry),ready,self.adb)[3:]
        installation = self._installation(deadline)
        if self.initial_installation is not None and installation != self.initial_installation:
            raise RuntimeError('Flutter first launch changed its prepared installation')
        if self.previous is not None and installation != self.previous['installation']:
            raise ValueError('Flutter replacement changed its installation')
        if self._process(deadline) is not None:raise RuntimeError('Flutter process already exists; no adoption')
        if self.previous is None:
            # Initial installation can overlap boot/package broadcasts and Activity relaunches.
            # Cross Android's queued-work boundary before creating the first consumer owner.
            raw = self._command('startup-barrier', ['shell','am','wait-for-broadcast-barrier',
                '--flush-broadcast-loopers','--flush-application-threads'], deadline)
            if (not raw.splitlines() or raw.splitlines()[-1] != 'Finished application barriers!'
                    or 'Gave up waiting' in raw):
                raise RuntimeError('Flutter Android startup barrier unconfirmed')
            if self._process(deadline) is not None or self._installation(deadline) != installation:
                raise RuntimeError('Flutter installation or process changed during startup preparation')
        directory.mkdir();write_json(directory/'intent.json',dict(previous=self.previous,installation=installation,entry=entry))
        self.uncertain = True
        raw = self._command('launch',args,deadline)
        statuses=[line.strip().removeprefix('Status: ') for line in raw.splitlines() if line.strip().startswith('Status: ')]
        activities=[line.strip().removeprefix('Activity: ') for line in raw.splitlines() if line.strip().startswith('Activity: ')]
        # Android prints either status after a successful start; alternate start dispositions
        # also print a Warning. The wait timeout is not a failed Intent or a missing reply.
        # Preserve it, never resend, and establish process/endpoint readiness separately below.
        if (statuses not in (['ok'],['timeout']) or len(activities)!=1 or not activities[0].startswith(self.package+'/')
                or re.search(r'(^|\n)(Error|Warning):',raw)):
            raise RuntimeError('Flutter launch acceptance unconfirmed')
        write_json(directory/'launch-status.json',dict(status=statuses[0],activity=activities[0]))
        if entry is not None:
            write_json(directory/'entry.json',dict(request=entry,confirmation='unobserved',destination='unobserved'))
        process = self._process(deadline)
        if process is None:raise RuntimeError('Flutter launch process missing')
        if self.previous is not None and (process==self.previous['process'] or process['boot_id']!=self.previous['process']['boot_id']):
            raise RuntimeError('Flutter process replacement identity differs')
        endpoint = None
        while endpoint is None:
            if self._process(deadline) != process:raise RuntimeError('Flutter process changed before endpoint readiness')
            raw = self._command('endpoint-log',['logcat','-d','-v','raw','--pid',str(process['pid']),'flutter:I','*:S'],deadline)
            for line in raw.splitlines():
                marker = 'FRONTCAP_FLUTTER_ENDPOINT '
                if not line.startswith(marker):continue
                candidate = json.loads(line[len(marker):])
                if (set(candidate) != {'generation','token','port','scenario'} or candidate['scenario']!='session'
                        or any(type(candidate[k]) is not str or not re.fullmatch('[a-f0-9]{48}',candidate[k]) for k in ('generation','token'))
                        or type(candidate['port']) is not int or not 0<candidate['port']<65536):
                    raise ValueError('Invalid Flutter Android endpoint')
                if endpoint is not None and endpoint!=candidate:
                    # Keep admission failed even when diagnostic reads or persistence fail.
                    try:
                        self._record_endpoint_failure(directory, deadline, process)
                    except Exception:
                        pass
                    raise ValueError('Multiple Flutter endpoint identities')
                endpoint = candidate
            if endpoint is None:time.sleep(min(.2,self.remaining(deadline)))
        if self.previous is not None and endpoint['generation']==self.previous['endpoint']['generation']:
            raise ValueError('Flutter replacement retained its generation')
        forwarding = self._forward(endpoint['port'],deadline)
        if self._installation(deadline)!=installation or self._process(deadline)!=process:
            raise ValueError('Flutter app changed during acquisition')
        self.owner = dict(serial=self.serial,package=self.package,activity=self.activity,installation=installation,
                          process=process,endpoint=endpoint,forwarding=forwarding)
        write_json(directory/'ready.json',self.owner);self.uncertain=False
        return self.owner

    def record_runtime_failure(self, directory, deadline):
        """Bounded reads from the original app before its owning device retires."""
        if self.owner is None:
            return
        directory.mkdir()
        self._record_endpoint_failure(directory, deadline, self.owner['process'])

    def _record_endpoint_failure(self, directory, deadline, process):
        self._record_diagnostics(directory, deadline, 'endpoint-diagnostics.json', [
            ('lifecycle-events', ['logcat','-b','events','-d','-v','epoch','-t','512']),
            ('process-log', ['logcat','-d','-v','epoch','--pid',str(process['pid']),'-t','512']),
            ('activities', ['shell','dumpsys','activity','activities']),
        ], context=dict(process=process, package=self.package))

    def _record_focus_failure(self, directory, deadline):
        self._record_diagnostics(directory, deadline, 'focus-diagnostics.json', [
            ('last-anr', ['shell','dumpsys','activity','lastanr']),
            ('failure-events', ['logcat','-b','events','-d','-v','epoch','am_anr:I','am_crash:I','*:S']),
        ])

    def _record_diagnostics(self, directory, deadline, filename, commands, context=None):
        end = min(deadline, time.monotonic()+3)
        remaining = 2*1024*1024
        report = dict(reads=[], error=None)
        if context is not None:report['context'] = context
        try:
            for name, args in commands:
                left = end-time.monotonic()
                if left <= 0 or remaining <= 0:
                    report['error'] = 'Diagnostic budget exhausted'
                    break
                argv = [str(self.adb),'-s',self.serial,*args]
                if self.resource_group is None:
                    result = capture_command(argv, directory/name, timeout=left, output_limit=remaining, env=self.env)
                else:
                    exchange = self._capture(argv, directory/name, left, remaining)
                    result = dict(exchange=str(directory/name/'exchange.json'), exit_code=exchange['exit_code'],
                        error=exchange['failure'], child_retired=True,
                        outputs={key: dict(bytes=len(exchange[key])) for key in ('stdout','stderr')})
                report['reads'].append(dict(name=name, result=result))
                remaining -= sum(row['bytes'] for row in result['outputs'].values())
        except Exception as error:
            report['error'] = f'{type(error).__name__}: {error}'
        write_json(directory/filename, report)

    def set_app_state(self, directory, state, deadline):
        from native_android_input import focused_component
        if state not in ('background','foreground'):raise ValueError('Invalid Flutter host state')
        self.ensure_running(deadline)
        before=dict(self.owner)
        if self._installation(deadline) != before['installation']:raise RuntimeError('Flutter installation changed')
        directory.mkdir();write_json(directory/'before.json',before)
        window=self._command('focus-before',['shell','dumpsys','window'],deadline)
        (directory/'window-before.txt').write_text(window)
        focused=focused_component(window.encode())
        component=self.package+'/'+self.activity
        if focused is None or (focused != component if state == 'background' else focused.split('/')[0] == self.package):
            self._record_focus_failure(directory, deadline)
            raise RuntimeError('Flutter host initial focus differs')
        self.uncertain=True
        command=(['shell','input','keyevent','KEYCODE_HOME'] if state == 'background' else
                 ['shell','am','start','-W','--activity-reorder-to-front','-n',component])
        self._command('host-'+state,command,deadline)
        visibility_deadline=min(deadline,time.monotonic()+5)
        while True:
            self.remaining(visibility_deadline)
            if self._process(deadline) != before['process']:raise RuntimeError('Flutter process changed during host transition')
            after_window=self._command('focus-after',['shell','dumpsys','window'],deadline)
            after_focus=focused_component(after_window.encode())
            if after_focus is not None and ((after_focus.split('/')[0] != self.package) if state == 'background' else after_focus == component):break
            time.sleep(min(.05,self.remaining(visibility_deadline)))
        if self._process(deadline) != before['process'] or self._installation(deadline) != before['installation']:
            raise RuntimeError('Flutter host transition changed process or installation')
        after=dict(self.owner)
        native=dict(platform='android',focused_before=focused,focused_after=after_focus)
        (directory/'window-after.txt').write_text(after_window)
        write_json(directory/'after.json',after);write_json(directory/'native.json',native)
        self.uncertain=False
        return dict(before=before,after=after,native=native)

    def terminate(self, directory, deadline, allow_exited=False):
        if self.uncertain or self.owner is None:raise RuntimeError('Flutter app ownership unavailable')
        current = self._process(deadline)
        if current!=self.owner['process'] and not (allow_exited and current is None):raise RuntimeError('Flutter app changed')
        previous = self.owner
        directory.mkdir();write_json(directory/'intent.json',previous);self.uncertain=True
        self.retire_forwarding(deadline)
        if current is not None:self._command('terminate',['shell','am','force-stop',self.package],deadline)
        while True:
            current=self._process(deadline)
            if current is None:break
            if current!=previous['process']:raise RuntimeError('Flutter process changed during termination')
            time.sleep(min(.05,self.remaining(deadline)))
        self._command('process-absent',['shell','run-as',self.package,'test','!','-d','/proc/'+str(previous['process']['pid'])],deadline)
        if self._installation(deadline)!=previous['installation']:raise ValueError('Flutter termination changed installation')
        write_json(directory/'terminated.json',previous)
        self.previous,self.owner,self.uncertain=previous,None,False
        return previous
