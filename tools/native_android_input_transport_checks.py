"""Compare owned transport interruption against independent app effects and recovery."""
import json
import signal
from native_android_processes import process_absent
from native_android_input_observation import app_clocks_match, input_receipt_matches, capture_matches
from native_android_tap import matching_processes


def check_transport(root, record, *, mode='transport-start'):
    if mode not in ('transport-start','guest-before-input'):raise ValueError('Invalid transport boundary')
    guest_mode=mode=='guest-before-input';stem='input-'+mode;category=mode.replace('-','_')
    report=json.loads((root/(stem+'.json')).read_text())
    if guest_mode:
        from native_android_guest_input_checks import check_setup,check_guest
        check_setup(root,report['guest_setup'],record)
    record(category+'.cases',report.get('progress')=='finished' and 'error' not in report
           and [r['action'] for r in report['cases']]==['continue','kill'],report)
    generations=[]
    for row in report['cases']:
        action=row['action'];killed=action=='kill';prefix=category+'.'+action
        directory=root/stem/action
        try:
            before=row['before'];transport=row['transport_before'];child=transport['identity'];request=transport['request']
            generations.append(before['generation'])
            pending=dict(generation=before['generation'],pid=before['pid'],controller_id=request['controller_id'],
                         next_id=1,pending=request,delivery_phase='dispatched',
                         component='dev.frontcap.s18.probe/dev.frontcap.s18.VirtualActivity')
            record(prefix+('.live_transport' if guest_mode else '.suspended_child'),transport['phase']==('started' if guest_mode else 'suspended')
                   and (guest_mode or transport['start_suspended'] is True)
                   and child['pid']==transport['pid'] and child['ppid']==transport['parent_pid']==row['controller_pid']
                   and child['command']==' '.join(transport['command']) and '/platform-tools/adb' in transport['command'][0]
                   and (guest_mode or transport['command'][-5:]==['shell','input','tap',str(request['x']),str(request['y'])])
                   and all(len(row[key])==1 and matching_processes(row[key],child)==row[key]
                           and (all(flag not in row[key][0]['state'] for flag in ('T','Z')) if guest_mode else 'T' in row[key][0]['state'])
                           and row[key][0]['ppid']==row['controller_pid']
                           for key in ('child_before','child_before_signal'))
                   and row['lock_held'] is True and row['client_before']==pending and (guest_mode or row['child_output_before']==''),transport)
            armed=row['receipt_before'];state=json.loads((directory/'before/state.json').read_text())
            record(prefix+'.before_delivery',before['status']=='ready' and before['last_id']==before['clicks']==0
                   and request['generation']==before['generation'] and request['request_id']==1
                   and armed==row['receipt_before_signal']==json.loads((directory/'before/input-control.json').read_text())
                   and armed['status']=='armed' and armed['request']==request and armed['pid']==before['pid']
                   and armed['generation']==before['generation'] and armed['last_id']==armed['clicks']==0
                   and app_clocks_match(state,0,0)
                   and capture_matches(root,row['initial_screen'],0,0,[255,0,0])
                   and capture_matches(root,row['suspended_screen'],0,0,[255,0,0]),dict(receipt=armed,app=state))
            terminal=row['transport_after'];cleanup=row['child_cleanup']
            record(prefix+'.termination',terminal==json.loads((directory/'controller/input-transport.json').read_text())
                   and all(terminal[key]==transport[key] for key in ('pid','parent_pid','command','request','identity','identity_observed','started_monotonic_ns')+(() if guest_mode else ('start_suspended',)))
                   and terminal['phase']=='finished' and terminal['exit_code']==(-9 if killed else 0)
                   and (guest_mode or row['signal']==int(signal.SIGKILL if killed else signal.SIGCONT))
                   and transport['started_monotonic_ns']<row['signal_monotonic_ns']<terminal['finished_monotonic_ns']<=row['controller_finished_ns']
                   and ('CalledProcessError' in terminal.get('error','') if killed else 'error' not in terminal)
                   and 'cleanup_error' not in terminal and row['controller_exit']==(1 if killed else 0)
                   and row['lock_released'] is True and row['cleanup_actions']==[]
                   and cleanup['identity']==child and cleanup['absent'] is True and cleanup['signals']==[]
                   and cleanup['observations'][-1]['processes']==[],dict(transport=terminal,cleanup=cleanup))
            if guest_mode:check_guest(root,directory,row,report['guest_setup'],record,prefix)
            after=row['receipt_after'];actual=json.loads((directory/'after-signal/state.json').read_text())
            record(prefix+'.signal_effect',after==json.loads((directory/'after-signal/input-control.json').read_text())
                   and (after==armed and row['original_reply'] is None and not (directory/'tap.stdout').read_bytes() if killed else
                        input_receipt_matches(after,request) and row['original_reply']==after
                        and json.loads((directory/'tap.stdout').read_text())==after and app_clocks_match(after['observation'],1,0))
                   and app_clocks_match(actual,0 if killed else 1,0)
                   and capture_matches(root,row['signal_screen'],0 if killed else 1,0,[255,0,0] if killed else [0,255,0]),dict(receipt=after,app=actual))
            recovered=row['recover']
            resolved={**pending,'next_id':2,'pending':None,'delivery_phase':None,'last_reply':after}
            expected=pending if killed else resolved
            record(prefix+'.receipt_only',row['client_after']==row['client_after_recover']==expected
                   and row['receipt_after_recover']==after and recovered['exit_code']!=0 and recovered['reply'] is None
                   and ('Input outcome unavailable' if killed else 'No pending input command') in recovered['error'],recovered)
            journal=[json.loads(line) for line in (directory/'controller/input-commands.jsonl').read_text().splitlines()]
            original=journal[0]
            record(prefix+'.original_attempt',original['request']==request and original['delivery_phase']=='prepared'
                   and 'registration' in original and 'tap_started_monotonic_ns' in original
                   and ('tap_finished_monotonic_ns' not in original and 'CalledProcessError' in original.get('error','') if killed else
                        original['reply']==after and 'tap_finished_monotonic_ns' in original and 'error' not in original),original)
            record(prefix+'.no_reinput',len(journal)==(4 if killed else 2)
                   and journal[1]['receipt_only'] is True
                   and all('registration' not in attempt and 'tap_started_monotonic_ns' not in attempt for attempt in journal[1:]),journal)
            if killed:
                fresh=row['replacement_before'];generations.append(fresh['generation'])
                record(prefix+'.unknown',row['client_after_resume']==row['client_final']==pending
                       ==json.loads((directory/'controller/input-client.json').read_text())
                       and row['resume']['exit_code']!=0 and 'Input outcome unavailable' in row['resume']['error']
                       and row['receipt_after_resume']==armed==json.loads((directory/'old/input-control.json').read_text())
                       and app_clocks_match(json.loads((directory/'old/state.json').read_text()),0,0),row['resume'])
                record(prefix+'.replacement',fresh['generation']!=before['generation'] and fresh['pid']!=before['pid']
                       and fresh['status']=='ready' and fresh['last_id']==fresh['clicks']==0
                       and row['old_resume']['exit_code']!=0 and 'Target generation changed' in row['old_resume']['error']
                       and row['replacement_unchanged'] is True and capture_matches(root,row['replacement_screen'],0,0,[255,0,0]),fresh)
                endpoint=fresh
            else:
                record(prefix+'.committed',row['client_final']==resolved==json.loads((directory/'controller/input-client.json').read_text()),resolved)
                endpoint=before
            reply=row['resolution'];completed=root/row['completed_client'];saved=row['completed_state']
            record(prefix+'.usable_input',completed==directory/('replacement-controller' if killed else 'controller')
                   and input_receipt_matches(reply,reply['request']) and reply['clicks']==1
                   and reply['generation']==endpoint['generation'] and reply['pid']==endpoint['pid']
                   and app_clocks_match(reply['observation'],1,0) and row['outcome']==reply and row['outcome_unchanged'] is True
                   and saved==json.loads((completed/'input-client.json').read_text()) and saved['pending'] is None
                   and saved['generation']==reply['generation'] and saved['pid']==reply['pid']
                   and saved['next_id']==2 and saved['last_reply']==reply and saved['delivery_phase'] is None,reply)
            if killed:
                new=row['replacement_transport'];attempts=[json.loads(line) for line in (completed/'input-commands.jsonl').read_text().splitlines()]
                record(prefix+'.replacement_delivery',new==json.loads((completed/'input-transport.json').read_text())
                       and new['request']==reply['request'] and new['phase']=='finished' and new['exit_code']==0 and 'error' not in new
                       and len(attempts)==1 and attempts[0]['request']==reply['request'] and attempts[0]['reply']==reply
                       and 'tap_started_monotonic_ns' in attempts[0] and 'tap_finished_monotonic_ns' in attempts[0],dict(transport=new,journal=attempts))
            clock,advance,later=(row[k] for k in ('clock_before','advance','clock_after'))
            record(prefix+'.clock_and_screen',clock['pid']==advance['pid']==later['pid']==reply['pid']
                   and clock['generation']==advance['generation']==later['generation']==reply['generation']
                   and clock['now_ns']==0 and advance['now_ns']==later['now_ns']==1000000
                   and app_clocks_match(clock['observation'],1,0) and app_clocks_match(advance['observation'],1,1)
                   and app_clocks_match(later['observation'],1,1)
                   and app_clocks_match(json.loads((directory/'final/state.json').read_text()),1,1)
                   and json.loads((directory/'final/input-control.json').read_text())==reply
                   and capture_matches(root,row['capture'],1,1,[0,255,0]),later)
            for label,filename,stopped,pid in [('final','processes.txt',row['stopped'],reply['pid'])]+(
                    [('old','old-processes.txt',row['old_stopped'],before['pid'])] if killed else []):
                inventory=(directory/filename).read_text()
                record(prefix+'.'+label+'_retired', stopped==dict(pid=pid,process_absent=True)
                       and process_absent(inventory,pid,'dev.frontcap.s18.probe'),stopped)
        except Exception as error:record(prefix+'.complete',False,f'{type(error).__name__}: {error}')
    record(category+'.generations',len(generations)==3 and len(set(generations))==3,generations)
