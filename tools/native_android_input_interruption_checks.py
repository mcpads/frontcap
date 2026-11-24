"""Require a live actual tap child, retained uncertainty and exactly one eventual action."""
import json
from native_android_input import InputRequest, validate_reply
from native_android_input_interruption import PHASES
from native_android_tap import matching_processes


def check_interruption(root, record, mode):
    if mode not in ('controller','tap'): raise ValueError('Invalid input interruption mode')
    report=json.loads((root/'input-interruption/report.json').read_text());rows=report['cases']
    record('input.interruption.cases',report.get('progress')=='finished' and report.get('mode')==mode and [r['phase'] for r in rows]==list(PHASES),report)
    lifecycle=json.loads((root/'clock-lifecycle.json').read_text())
    deliveries=[lifecycle['cases'][0]['input'],lifecycle['cases'][0]['replacement_input'],lifecycle['cases'][1]['input'],lifecycle['cases'][1]['replacement_input']]
    for row,delivery in zip(rows,deliveries):
        phase=row['phase'];prefix='input.interruption.'+phase;marker=row['checkpoint'];child=row['tap_child']
        request=InputRequest(**marker['request']);reply=row['resolution'];terminal=phase=='after-receipt'
        kill_tap = mode=='tap' and phase=='before-touch-up'
        validate_reply(reply,request)
        expected_tail=f' shell input tap {request.x} {request.y}'
        transport=row['transport_at_checkpoint'];live=row['transport_state']=='live'
        observed=row['tap_children']
        original=(transport['phase']=='started' and len(observed)==1 and 'Z' not in observed[0]['state']
                  and observed[0]['ppid']==row['controller_pid'] and matching_processes(observed,child)==observed
                  and matching_processes(row['processes_before'],child)==observed) if live else (
                  transport['phase']=='finished' and transport['exit_code']==0 and 'error' not in transport
                  and transport['finished_monotonic_ns']<row['kill_started_ns'] and not row['tap_children']
                  and not matching_processes(row['processes_before'],child))
        record(prefix+'.actual_transport',original and (live or phase!='before-touch-up')
               and row['controller_live_before'] is True and row['lock_held'] is True
               and child['ppid']==row['controller_pid']==transport['parent_pid'] and child['pid']==transport['pid']
               and transport['request']==marker['request'] and transport['identity']==child
               and child['command'].endswith(expected_tail) and '/platform-tools/adb -s emulator-' in child['command'],transport)
        state=row['state_at_death'];later=row['child_after_death'];still=row['child_after_resume']
        child_state=(not later and not still) if kill_tap else (
                     len(later)==len(still)==1 and all(matching_processes(group,child)==group and 'Z' not in group[0]['state']
                     and group[0]['ppid']!=row['controller_pid'] for group in (later,still))) if phase=='before-touch-up' else (
                     all(len(group)<=1 and matching_processes(group,child)==group and all(p['ppid']!=row['controller_pid'] for p in group)
                         for group in (later,still)) if live else not later and not still)
        record(prefix+'.death',row['exit_code']==(1 if kill_tap else -9)
               and row['interruption']==('tap-child' if kill_tap else 'controller')
               and row['lock_released'] is True and child_state
               and state['pending']==marker['request'] and state['delivery_phase']=='dispatched'
               and state['generation']==marker['generation']==row['before']['generation']
               and state['pid']==marker['pid']==row['before']['pid']==reply['pid']
               and state['next_id']==1,dict(state=state,child_after_death=later,child_after_resume=still))
        if kill_tap:
            before_signal=row['tap_before_signal'];failure=row['transport_after_death']
            record(prefix+'.tap_termination',len(before_signal)==1 and matching_processes(before_signal,child)==before_signal
                   and before_signal[0]['ppid']==row['controller_pid'] and 'Z' not in before_signal[0]['state']
                   and row['tap_signal']==9 and failure['phase']=='finished' and failure['exit_code']==-9
                   and failure['pid']==child['pid'] and failure['parent_pid']==row['controller_pid']
                   and failure['request']==marker['request'] and failure['command']==transport['command']
                   and failure['identity']==child and 'CalledProcessError' in failure['error']
                   and failure['finished_monotonic_ns']>=row['kill_started_ns'],failure)
        if terminal:
            host_marker=row['controller_checkpoint']
            record(prefix+'.controller_checkpoint',host_marker==row['controller_checkpoint_after']
                   and host_marker['phase']=='after-reply' and host_marker['status']=='waiting'
                   and host_marker['pid']==row['controller_pid'] and host_marker['request']==marker['request']
                   and host_marker['wait_s']==10 and row['controller_exit_before'] is None,host_marker)
        current=row['target_at_checkpoint'];app=row['app_at_checkpoint']
        record(prefix+'.checkpoint',marker['status']=='waiting' and marker['phase']==phase and marker['wait_ms']==5000
               and marker['request']==reply['request'] and current['request']==reply['request']
               and marker['generation']==reply['generation'] and marker['pid']==reply['pid']
               and marker['release_file']==f'input-release-{request.generation}-{request.request_id}'
               and marker['clicks']==(0 if phase in ('before-touch-up','before-action') else 1)
               and current['status']==('ok' if terminal else 'armed' if phase=='before-touch-up' else 'executing')
               and current['last_id']==current['clicks']==int(terminal)
               and app['count']==(0 if phase in ('before-touch-up','before-action') else 1)
               and app['fired']==0 and all(app[k]==0 for k in ('uptime_ms','elapsed_ms','nano_ns','wall_ms')),
               dict(marker=marker,receipt=current,app=app))
        resume=row['resume']
        unresolved=(resume['exit_code']!=0 and 'Input outcome unavailable' in resume['error']
                    and row['state_after_resume']==state)
        committed=(resume['exit_code']==0 and resume['reply']==reply and row['state_after_resume']['pending'] is None
                   and row['state_after_resume']['last_reply']==reply)
        record(prefix+'.recovery_boundary',(committed if terminal else unresolved)
               and row['target_after_resume']==current and row['checkpoint_after_resume']==marker,
               dict(resume=resume,state=row['state_after_resume']))
        released=row['checkpoint_released'];done=row['state_resolved']
        record(prefix+'.released_once',released=={**marker,'status':'released','released_real_ns':released['released_real_ns']}
               and 0<=released['released_real_ns']-marker['started_real_ns']<5000000000
               and reply==row['local_outcome']==row['target_after']==delivery['receipt']
               and reply['observation']==row['app_after'] and reply['clicks']==row['app_after']['count']==1
               and row['app_after']['fired']==0 and row['app_after']['nano_ns']==0
               and done['pending'] is None and done['delivery_phase'] is None and done['next_id']==2
               and done['last_reply']==reply and done==json.loads((root/row['client']/'input-client.json').read_text()),
               dict(released=released,reply=reply))
        observations=row['retirement_observations'];cleanup=row['child_cleanup']
        record(prefix+'.retired',row['child_absent'] is True and observations and not observations[-1]['processes']
               and cleanup['absent'] is True and not cleanup['signals'] and not row['cleanup_actions']
               and all(c['absent'] is True and not c['signals'] for c in row['other_children_cleanup'])
               and 'cleanup_error' not in row,dict(observations=observations,cleanup=cleanup))
        journal=[json.loads(line) for line in (root/row['client']/'input-commands.jsonl').read_text().splitlines()]
        requests=[r for r in journal if 'request' in r]
        if kill_tap:
            attempt=requests[0]
            record(prefix+'.failed_attempt',attempt['request']==marker['request'] and attempt['delivery_phase']=='prepared'
                   and 'registration' in attempt and 'tap_started_monotonic_ns' in attempt
                   and 'tap_finished_monotonic_ns' not in attempt and 'CalledProcessError' in attempt['error'],attempt)
            requests=requests[1:]
        record(prefix+'.no_second_tap',len(requests)==(1 if terminal else 2)
               and all(r['request']==marker['request'] and r['delivery_phase']=='dispatched'
                       and 'tap_started_monotonic_ns' not in r and 'registration' not in r for r in requests)
               and (terminal or requests[0]['reply']==current and 'Input outcome unavailable' in requests[0]['error'])
               and requests[-1]['reply']==reply,journal)
