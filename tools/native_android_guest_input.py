"""Own and observe a stopped remote shell child before exec of Android input."""
import hashlib
import json
from pathlib import PurePosixPath
import shlex
import time
import uuid

from native_android_diagnostics import run_command
from native_android_input import InputRequest


def require_shell_protocol(output):
    if 'shell_v2' not in output.splitlines():
        raise RuntimeError('Guest input gate requires advertised shell_v2 support')


def gate_command(adb, executable, directory, request):
    InputRequest(**request)
    for value in (executable,directory):
        if not PurePosixPath(value).is_absolute() or '\0' in value:
            raise ValueError('Guest gate paths must be absolute and NUL-free')
    args=[executable,directory,request['generation'],request['controller_id'],
          str(request['request_id']),str(request['x']),str(request['y'])]
    return [*adb,'shell','-T','exec '+shlex.join(args)]


def parse_process(raw):
    stat,tail=raw.split('\n',1);fields=stat[stat.rfind(')')+2:].split()
    status,tail=tail.split('\nCOMMAND\n',1);command,exe=tail.split('\nEXE\n',1)
    values=dict(line.split(':',1) for line in status.splitlines() if ':' in line)
    return dict(pid=int(stat.split()[0]),state=fields[0],ppid=int(fields[1]),group=int(fields[2]),start_ticks=int(fields[19]),
        command=command.replace('\0',' ').rstrip(),exe=exe.strip(),uid=int(values['Uid'].split()[0]),
        sigpnd=int(values['SigPnd'].strip(),16),shdpnd=int(values['ShdPnd'].strip(),16),
        sigign=int(values['SigIgn'].strip(),16),sigblk=int(values['SigBlk'].strip(),16),raw=raw)


def same_process(current,expected):
    return current is not None and all(current[k]==expected[k] for k in ('pid','start_ticks','command','exe','uid'))


class GuestInputGate:
    def __init__(self,root,adb):
        self.root,self.adb=root,adb
        self.remote='/data/local/tmp/fc-input-gate-'+uuid.uuid4().hex
        self.executable=self.remote+'/input-gate'
        self.setup={}

    def call(self,*args):
        return run_command([*self.adb,*args],self.root,timeout=5)

    def shell(self,code):
        return self.call('shell',code)

    def prepare(self):
        features=self.call('features').decode().strip();require_shell_protocol(features)
        binary=self.root.parent/'input-gate'
        self.setup.update(features=features,adbd_pid=int(self.shell('pidof adbd')),remote=self.remote,
                          binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
        self.shell('mkdir '+shlex.quote(self.remote))
        self.call('push',str(binary),self.executable)
        self.shell('chmod 700 '+shlex.quote(self.executable))
        self.setup['guest_binary_sha256']=self.shell('sha256sum '+shlex.quote(self.executable)).decode().split()[0]
        if self.setup['guest_binary_sha256']!=self.setup['binary_sha256']:
            raise RuntimeError('Guest input gate binary differs from built input')

    def create(self,action):
        directory=self.remote+'/'+action
        self.shell('mkdir '+shlex.quote(directory))
        return dict(directory=directory,executable=self.executable,cleanup_signals=[])

    def read(self,row,name):
        path=shlex.quote(row['directory']+'/'+name)
        raw=self.shell('if [ -e '+path+' ]; then cat '+path+'; else printf null; fi')
        return json.loads(raw)

    def snapshot(self,pid):
        if type(pid) is not int or pid<=0:raise ValueError('Invalid owned guest PID')
        code=f'if [ -d /proc/{pid} ]; then cat /proc/{pid}/stat; cat /proc/{pid}/status; printf "\\nCOMMAND\\n"; cat /proc/{pid}/cmdline; printf "\\nEXE\\n"; readlink /proc/{pid}/exe; else printf absent; fi'
        raw=self.shell(code).decode()
        return None if raw=='absent' else parse_process(raw)

    def wait(self,row,*,absent=False,pending=False):
        deadline=time.monotonic()+2;observations=[]
        while True:
            current=self.snapshot(row['identity']['pid'])
            observations.append(dict(host_ns=time.monotonic_ns(),process=current))
            if absent and current is None:return observations
            if (not absent and same_process(current,row['identity']) and current['state']=='T'
                    and (not pending or (current['sigpnd']|current['shdpnd'])&1)):
                return observations
            if time.monotonic()>=deadline:raise RuntimeError('Guest input boundary unavailable: '+repr(observations))
            time.sleep(.02)

    def ready(self,row,request,deadline):
        while True:
            value=self.read(row,'ready.json')
            if value is not None:break
            if time.monotonic()>=deadline:raise RuntimeError('Guest input readiness unavailable')
            time.sleep(.02)
        row['ready']=value;row['identity']=self.snapshot(value['pid']);identity=row['identity']
        expected=gate_command(self.adb,row['executable'],row['directory'],request)[-1][5:]
        if (value['request']!=request or identity is None or identity['command']!=expected
                or identity['exe']!=row['executable']
                or value['uid']!=2000 or identity['uid']!=2000
                or value['ppid']!=identity['ppid'] or value['ppid']!=self.setup['adbd_pid']
                or value['group']!=value['pid'] or identity['group']!=value['pid']
                or not value['hup_default'] or value['hup_blocked']):
            raise RuntimeError('Guest input identity does not match the request')
        row['stopped']=self.wait(row)
        row['released_before']=self.read(row,'released.json')
        if row['released_before'] is not None:raise RuntimeError('Guest input already released')

    def signal(self,row,name):
        if name not in ('CONT','KILL'):raise ValueError('Unsupported guest intervention')
        current=self.snapshot(row['identity']['pid'])
        if not same_process(current,row['identity']) or current['state']!='T':
            raise RuntimeError('Owned stopped guest identity changed; refusing signal')
        self.shell('kill -'+name+' '+str(current['pid']))

    def release(self,row):
        row['before_release']=self.snapshot(row['identity']['pid'])
        row['release_ns']=time.monotonic_ns()
        self.signal(row,'CONT')

    def disconnected(self,row):
        row['after_host_death']=self.wait(row,pending=True)
        row['released_after_host_death']=self.read(row,'released.json')
        if row['released_after_host_death'] is not None:raise RuntimeError('Input released during host death')

    def finished(self,row):
        row['retired']=self.wait(row,absent=True)
        row['released_after']=self.read(row,'released.json')

    def cleanup(self,row):
        if 'identity' not in row or row['identity'] is None:return
        current=self.snapshot(row['identity']['pid'])
        if current is not None:
            self.signal(row,'KILL');row['cleanup_signals'].append('KILL')
        row['cleanup_retired']=self.wait(row,absent=True)
