"""Interrupt an input controller at observed touch, action and receipt boundaries."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

from native_android_input import read_receipt
from native_android_tap import host_processes, matching_processes, wait_for_exit, retire_process
from native_clock_control import atomic_json
from native_controller_recovery import controller_environment, invoke_cli, lock_available

PHASES = ('before-touch-up','before-action','after-action','after-receipt')


def interrupt_tap(root, client, adb, phase, target, x, y, *, mode):
    if mode not in ('controller', 'tap'): raise ValueError('Invalid input interruption mode')
    kill_tap = mode == 'tap' and phase == 'before-touch-up'
    directory=root/'input-interruption';directory.mkdir(exist_ok=True)
    path=directory/'report.json'
    report=json.loads(path.read_text()) if path.exists() else dict(mode=mode,progress='starting',cases=[])
    if report['mode'] != mode: raise RuntimeError('Input interruption mode changed')
    if [r['phase'] for r in report['cases']]!=list(PHASES[:PHASES.index(phase)]):
        raise RuntimeError('Input phases must run once in order')
    row=dict(phase=phase,before=read_receipt(client.command,client.package),client=str(client.directory.relative_to(root)))
    report['cases'].append(row)
    def save(progress):
        report['progress']=progress;atomic_json(path,report)
    def app_file(name):
        return client.command('shell','run-as',client.package,'cat','files/'+name)
    command=[sys.executable,str(Path(__file__).with_name('native_android_input.py')),
             '--adb',adb[0],'--serial',adb[2],'--directory',str(client.directory),'--record-tap-process']
    def invoke(name,*args,success=True):
        return invoke_cli(directory,command,phase+'-'+name,*args,success=success)
    expected=' '.join([*adb,'shell','input','tap',str(x),str(y)])
    controller_marker=client.directory/'controller-checkpoint.json'
    controller_marker.unlink(missing_ok=True)
    environment=controller_environment()
    if phase=='after-receipt': environment['FC_CONTROLLER_STOP_AT']='after-reply'
    process=None;child=None;marker=None;released=False;owned=[]
    try:
        save(phase+'-starting')
        with (directory/(phase+'.stdout')).open('wb') as stdout, (directory/(phase+'.stderr')).open('wb') as stderr:
            process=subprocess.Popen([*command,'tap','--target',target,'--x',str(x),'--y',str(y)],
                                     stdout=stdout,stderr=stderr,env=environment)
            row['controller_pid']=process.pid;deadline=time.monotonic()+5
            while True:
                marker=json.loads(app_file('input-checkpoint.json'))
                if marker.get('status')=='waiting' and marker.get('phase')==phase and marker.get('generation')==row['before']['generation']:
                    if phase!='after-receipt': break
                    if controller_marker.exists():
                        host_marker=json.loads(controller_marker.read_text())
                        if (host_marker.get('pid')==process.pid and host_marker.get('phase')=='after-reply'
                                and host_marker.get('status')=='waiting' and host_marker.get('request')==marker['request']):
                            row['controller_checkpoint']=host_marker
                            break
                if process.poll() is not None or time.monotonic()>=deadline:
                    raise RuntimeError(f'Input checkpoint unavailable: {phase}, child={process.poll()}')
                time.sleep(.02)
            row['checkpoint']=marker
            row['processes_before']=host_processes()
            owned=[p for p in row['processes_before'] if p['ppid']==process.pid]
            transport=json.loads((client.directory/'input-transport.json').read_text())
            row['transport_at_checkpoint']=transport
            child=transport['identity'];row['tap_child']=child
            children=matching_processes(row['processes_before'],child);row['tap_children']=children
            if (transport['command']!=[*adb,'shell','input','tap',str(x),str(y)] or transport['parent_pid']!=process.pid
                    or transport['request']!=marker['request']):
                raise RuntimeError('Transport record does not belong to this input controller')
            if children and (len(children)!=1 or 'Z' in children[0]['state'] or children[0]['ppid']!=process.pid or children[0]['command']!=expected):
                raise RuntimeError('Actual input child is not live under this controller')
            if children and transport['phase']!='started':
                raise RuntimeError('Live tap child has a terminal transport record')
            row['transport_state']='live' if children else 'retired'
            if not children and (transport['phase']!='finished' or transport['exit_code']!=0):
                raise RuntimeError('Missing terminal evidence for the original tap child')
            row['controller_exit_before']=process.poll()
            row['controller_live_before']=row['controller_exit_before'] is None
            row['lock_held']=not lock_available(client.directory,'input')
            row['target_at_checkpoint']=read_receipt(client.command,client.package)
            row['app_at_checkpoint']=json.loads(app_file('state.json'))
            save(phase+'-checkpoint')
            if (phase=='before-touch-up' and len(children)!=1) or not row['controller_live_before'] or not row['lock_held']:
                raise RuntimeError('No live actual adb tap child at the application checkpoint')
            row['kill_started_ns']=time.monotonic_ns()
            row['interruption']='tap-child' if kill_tap else 'controller'
            if kill_tap:
                row['tap_before_signal']=matching_processes(host_processes(),child)
                current=row['tap_before_signal']
                if len(current)!=1 or 'Z' in current[0]['state'] or current[0]['ppid']!=process.pid:
                    raise RuntimeError('Owned live tap child unavailable immediately before signal')
                os.kill(child['pid'],signal.SIGKILL)
                row['tap_signal']=int(signal.SIGKILL)
            else:
                process.kill()
            row['exit_code']=process.wait(timeout=5)
            if kill_tap:
                row['transport_after_death']=json.loads((client.directory/'input-transport.json').read_text())
                if row['exit_code']!=1 or row['transport_after_death'].get('exit_code')!=-9:
                    raise RuntimeError('Input controller did not retain the actual tap-child failure')
            if phase=='after-receipt': row['controller_checkpoint_after']=json.loads(controller_marker.read_text())
            row['lock_released']=lock_available(client.directory,'input')
            row['state_at_death']=json.loads(client.path.read_text())
            row['child_after_death']=matching_processes(host_processes(),child)
            if (kill_tap or not children) and row['child_after_death']:
                raise RuntimeError('Terminated or previously retired adb child remains present')
            if phase=='before-touch-up' and not kill_tap and (len(row['child_after_death'])!=1 or 'Z' in row['child_after_death'][0]['state']):
                raise RuntimeError('Actual adb child did not survive controller death')
            save(phase+'-killed')
            row['resume']=invoke('resume','resume',success=False)
            row['state_after_resume']=json.loads(client.path.read_text())
            row['target_after_resume']=read_receipt(client.command,client.package)
            row['checkpoint_after_resume']=json.loads(app_file('input-checkpoint.json'))
            row['child_after_resume']=matching_processes(host_processes(),child)
            if phase=='after-receipt':
                if row['resume']['exit_code'] or row['resume']['reply']!=row['target_at_checkpoint']:
                    raise RuntimeError('Terminal receipt was not recovered while the app callback remained held')
            elif (row['resume']['exit_code']==0 or 'Input outcome unavailable' not in row['resume']['error']
                  or row['state_after_resume']!=row['state_at_death']):
                raise RuntimeError('Executing input lost its retained uncertainty')
            if (row['checkpoint_after_resume']!=marker or row['target_after_resume']!=row['target_at_checkpoint']
                    or (kill_tap and row['child_after_resume'])
                    or (phase=='before-touch-up' and not kill_tap and (len(row['child_after_resume'])!=1 or 'Z' in row['child_after_resume'][0]['state']))
                    or (not children and row['child_after_resume'])):
                raise RuntimeError('Target/transport changed before explicit checkpoint release')
            save(phase+'-unresolved' if phase!='after-receipt' else phase+'-terminal')
            row['release_started_ns']=time.monotonic_ns()
            client.command('shell','run-as',client.package,'touch','files/'+marker['release_file'])
            released=True
            row['resolution']=(row['resume']['reply'] if phase=='after-receipt' else invoke('recover','recover')['reply'])
            row['retirement_observations']=[]
            row['child_absent']=wait_for_exit(child,row['retirement_observations'])
            if not row['child_absent']: raise RuntimeError('Orphaned adb input child did not retire after callback release')
            row['checkpoint_released']=json.loads(app_file('input-checkpoint.json'))
            row['local_outcome']=invoke('outcome','outcome')['reply']
            row['state_resolved']=json.loads(client.path.read_text())
            row['app_after']=json.loads(app_file('state.json'))
            row['target_after']=read_receipt(client.command,client.package)
            save('finished' if phase==PHASES[-1] else phase+'-finished')
            return row['resolution']
    except BaseException as error:
        report['error']=f'{type(error).__name__}: {error}';save('failed');raise
    finally:
        cleanup=[]
        try:
            if process is not None and process.poll() is None:
                process.kill();process.wait(timeout=5)
            if marker and marker.get('status')=='waiting' and not released:
                client.command('shell','run-as',client.package,'touch','files/'+marker['release_file'])
                cleanup.append('released diagnostic callback after failure')
            row['other_children_cleanup']=[retire_process(p) for p in owned if child is None or p['pid']!=child['pid']]
            if any(not r['absent'] for r in row['other_children_cleanup']):
                raise RuntimeError('Other owned child cleanup incomplete')
            if child:
                row['child_cleanup']=retire_process(child)
                if not row['child_cleanup']['absent']:
                    raise RuntimeError('Owned adb child cleanup incomplete')
        except BaseException as error:
            row['cleanup_error']=f'{type(error).__name__}: {error}'
            report['progress']='failed'
            raise
        finally:
            row['cleanup_actions']=cleanup;atomic_json(path,report)
