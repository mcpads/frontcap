"""Compare transport loss at owned host-start or guest-before-input boundaries."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

from native_android_control import ClockClient
from native_android_input import InputClient, read_receipt
from native_android_lifecycle import PACKAGE, archive, stop
from native_android_tap import host_processes, matching_processes, retire_process
from native_clock_control import atomic_json
from native_controller_recovery import controller_environment, invoke_cli, lock_available


def exercise(root, command, launch, observe, node, center, adb, *, mode='transport-start'):
    if mode not in ('transport-start','guest-before-input'):raise ValueError('Invalid transport boundary')
    guest_mode=mode=='guest-before-input'
    stem='input-'+mode
    gate=None
    if guest_mode:
        from native_android_guest_input import GuestInputGate
        gate=GuestInputGate(root,adb)
    report = dict(progress='starting', cases=[])
    def save(progress):
        report['progress'] = progress
        atomic_json(root/(stem+'.json'), report)
    def receipt():
        return read_receipt(command, PACKAGE)
    def cli(directory):
        return [sys.executable, str(Path(__file__).with_name('native_android_input.py')),
                '--adb', adb[0], '--serial', adb[2], '--directory', str(directory), '--record-tap-process']
    try:
        if gate:
            report['guest_setup']=gate.setup
            gate.prepare()
        for action in ('continue', 'kill'):
            directory = root/stem/action
            directory.mkdir(parents=True)
            row = dict(action=action)
            if gate:row['guest']=gate.create(action)
            report['cases'].append(row)
            save(action+'-launching')
            row['launch'] = launch().decode()
            tree, row['initial_screen'] = observe('transport-'+action+'-initial',0)
            row['before'] = receipt()
            client = InputClient(command,directory/'controller')
            x,y = center(node(tree,'native-increment'))
            command_line = cli(client.directory)
            process,child = None,None
            try:
                with (directory/'tap.stdout').open('wb') as stdout, (directory/'tap.stderr').open('wb') as stderr:
                    started = time.monotonic()
                    options=(['--tap-guest-gate',row['guest']['executable'],row['guest']['directory']] if gate else ['--tap-start-suspended'])
                    process = subprocess.Popen([*command_line,*options,'tap','--x',str(x),'--y',str(y)],
                        env=controller_environment(),stdout=stdout,stderr=stderr)
                    row['controller_pid'] = process.pid
                    deadline = started+5
                    while True:
                        path = client.directory/'input-transport.json'
                        if path.exists():
                            transport = json.loads(path.read_text())
                            if transport['phase'] == ('started' if gate else 'suspended'):
                                break
                        if process.poll() is not None or time.monotonic() >= deadline:
                            raise RuntimeError('Actual suspended adb child unavailable')
                        time.sleep(.01)
                    row['transport_before'] = transport
                    if gate:gate.ready(row['guest'],transport['request'],deadline)
                    child = transport['identity']
                    row['child_before'] = matching_processes(host_processes(),child)
                    row['client_before'] = json.loads(client.path.read_text())
                    row['receipt_before'] = receipt()
                    row['lock_held'] = not lock_available(client.directory,'input')
                    archive(command,directory/'before')
                    _,row['suspended_screen'] = observe('transport-'+action+'-suspended',0)
                    row['receipt_before_signal'] = receipt()
                    if gate:
                        row['guest']['before_signal']=gate.snapshot(row['guest']['identity']['pid'])
                    else:
                        row['child_output_before'] = (client.directory/'input-transport.stdout').read_text()
                    row['child_before_signal'] = matching_processes(host_processes(),child)
                    if (len(row['child_before_signal']) != 1 or child['ppid'] != process.pid
                            or not row['lock_held'] or row['receipt_before_signal'] != row['receipt_before']):
                        raise RuntimeError('Input changed before transport intervention')
                    if not gate and ('T' not in row['child_before_signal'][0]['state']
                            or transport['command'] != [*adb,'shell','input','tap',str(x),str(y)]):
                        raise RuntimeError('Suspended tap identity changed')
                    if gate:save(action+'-guest-waiting')
                    row['signal_monotonic_ns'] = time.monotonic_ns()
                    if gate:
                        row['intervention']='continue-guest' if action=='continue' else 'kill-host'
                        if action=='continue':gate.release(row['guest'])
                        else:os.kill(child['pid'],signal.SIGKILL)
                    else:
                        row['signal'] = int(signal.SIGCONT if action == 'continue' else signal.SIGKILL)
                        os.kill(child['pid'],row['signal'])
                    row['controller_exit'] = process.wait(timeout=max(0,15-(time.monotonic()-started)))
                    row['controller_finished_ns'] = time.monotonic_ns()
                    if gate:
                        if action=='kill':
                            gate.disconnected(row['guest'])
                            gate.release(row['guest'])
                        gate.finished(row['guest'])
                row['lock_released'] = lock_available(client.directory,'input')
                row['transport_after'] = json.loads((client.directory/'input-transport.json').read_text())
                row['client_after'] = json.loads(client.path.read_text())
                row['receipt_after'] = receipt()
                row['original_reply'] = json.loads((directory/'tap.stdout').read_text()) if action == 'continue' else None
                _,row['signal_screen'] = observe('transport-'+action+'-signal',1 if action == 'continue' else 0)
                archive(command,directory/'after-signal')
                if row['controller_exit'] != (0 if action == 'continue' else 1):
                    raise RuntimeError('Unexpected original controller outcome')
                save(action+'-observed')
            finally:
                row['cleanup_actions'] = []
                if process is not None and process.poll() is None:
                    process.kill();process.wait(timeout=5)
                    row['cleanup_actions'].append('killed controller after failure')
                if child:
                    row['child_cleanup'] = retire_process(child)
                    if not row['child_cleanup']['absent']:
                        raise RuntimeError('Original adb child cleanup incomplete')
                if gate:gate.cleanup(row['guest'])
                save(action+'-retired')
            def invoke(name,*args,success=True):
                return invoke_cli(directory,command_line,name,*args,success=success)
            row['recover'] = invoke('recover','recover',success=False)
            row['client_after_recover'] = json.loads(client.path.read_text())
            row['receipt_after_recover'] = receipt()
            if action == 'kill':
                row['resume'] = invoke('resume','resume',success=False)
                row['client_after_resume'] = json.loads(client.path.read_text())
                row['receipt_after_resume'] = receipt()
                row['old_stopped'] = stop(command,row['before']['pid'],directory/'old-processes.txt')
                archive(command,directory/'old')
                row['replacement_launch'] = launch().decode()
                tree,row['replacement_screen'] = observe('transport-replacement',0)
                row['replacement_before'] = receipt()
                row['old_resume'] = invoke('old-resume','resume',success=False)
                row['replacement_unchanged'] = receipt() == row['replacement_before']
                completed = directory/'replacement-controller'
                x,y = center(node(tree,'native-increment'))
                row['resolution'] = invoke_cli(directory,cli(completed),'replacement-tap','tap','--x',str(x),'--y',str(y))['reply']
                row['replacement_transport'] = json.loads((completed/'input-transport.json').read_text())
            else:
                completed = client.directory
                row['resolution'] = row['original_reply']
            endpoint = receipt()
            row['outcome'] = invoke_cli(directory,cli(completed),'outcome','outcome')['reply']
            row['outcome_unchanged'] = receipt() == endpoint
            row['client_final'] = json.loads(client.path.read_text())
            row['completed_client'] = str(completed.relative_to(root))
            row['completed_state'] = json.loads((completed/'input-client.json').read_text())
            clock = ClockClient(command,directory/'clock')
            row['clock_before'] = clock.request('read')
            row['advance'] = clock.request('advance',1000000)
            row['clock_after'] = clock.request('read')
            _,row['capture'] = observe('transport-'+action+'-after',1)
            row['stopped'] = stop(command,row['resolution']['pid'],directory/'processes.txt')
            archive(command,directory/'final')
            save(action+'-finished')
        save('finished')
    except BaseException as error:
        report['error'] = f'{type(error).__name__}: {error}'
        save('failed')
        raise
