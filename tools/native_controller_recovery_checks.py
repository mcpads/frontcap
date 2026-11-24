"""Acceptance for killed clock controllers, independent live reads and local outcome recovery."""
import json
import xml.etree.ElementTree as ET

from native_clock_control import Request
from native_ios_control import validate_reply as validate_ios
from native_android_control import validate_reply as validate_android


def check_recovery(root, platform, record):
    directory = root/'controller-recovery'
    report = json.loads((directory/'report.json').read_text())
    initial, cases = report['initial'], report['cases']
    generation, pid = initial['generation'], initial['pid']
    validate = validate_ios if platform == 'ios' else validate_android
    def confirmed(reply, number, operation, delta, before):
        try:
            validate(reply, Request(generation, number, operation, delta))
            return (reply['status'] == 'ok' and reply['pid'] == pid and reply['before_ns'] == before
                    and reply['now_ns'] == before+delta)
        except (RuntimeError, ValueError, TypeError, KeyError):
            return False
    record('controller.cases', report['progress'] == 'finished'
           and [c['phase'] for c in cases] == ['before-pending','after-pending','after-reply','after-commit']
           and confirmed(initial,1,'read',0,0), report)
    for i, row in enumerate(cases):
        phase = row['phase']; name = 'controller.'+phase
        number, before = 2*i+2, i*1000000
        request = dict(generation=generation, request_id=number, operation='advance', delta_ns=1000000)
        marker = row['checkpoint']
        record(name+'.death', row['exit_code'] == -9 and marker == row['checkpoint_after']
               and marker['pid'] == row['controller_pid'] and marker['phase'] == phase
               and marker['status'] == 'waiting' and marker['wait_s'] == 10 and marker['request'] == request
               and row['lock_held'] is True and row['lock_released'] is True
               and not (directory/(phase+'.stdout')).read_text(), marker)
        target = row['target_at_death']
        committed = phase == 'after-commit'
        pending = phase in ('after-pending','after-reply')
        expected = dict(generation=generation, next_id=number+int(committed), pending=request if pending else None,
                        last_reply=target if committed else row['before'])
        record(name+'.persistence', row['state_at_death'] == expected
               and confirmed(row['before'],number-1,'read',0,before)
               and (confirmed(target,number,'advance',1000000,before) if phase in ('after-reply','after-commit')
                    else target == row['before']), dict(state=row['state_at_death'], target=target))
        result, resume = row['resolution'], row['resume']
        record(name+'.recovery', confirmed(result,number,'advance',1000000,before)
               and result['replayed'] is (phase == 'after-reply')
               and ((resume['exit_code'] == 0 and resume['reply'] == result) if pending else
                    (resume['exit_code'] != 0 and resume['reply'] is None and 'No pending clock command' in resume['error']))
               and row['local_outcome'] == result and row['outcome_without_target_change'] is True
               and row['state_resolved'] == dict(generation=generation,next_id=number+1,pending=None,last_reply=result), result)
        record(name+'.live_target', confirmed(row['after'],number+1,'read',0,before+1000000), row['after'])
    app = json.loads((directory/'app.json').read_text())
    capture = app['capture']
    final = cases[-1]['after']
    complete = (json.loads((directory/'app/clock-control.json').read_text()) == final
                and json.loads((directory/'client/clock-client.json').read_text()) ==
                dict(generation=generation,next_id=10,pending=None,last_reply=final))
    if platform == 'ios':
        advances = [json.loads(line) for line in (directory/'app/clock-advances.jsonl').read_text().splitlines()]
        inputs = [line for line in (directory/'app/input-events.txt').read_text().splitlines() if line.startswith('button action ')]
        complete &= (inputs == ['button action increment'] and [r['request']['request_id'] for r in advances] == [2,4,6,8]
                     and capture['clock'] == 'Time: 4000000' and capture['text'] == 'Pending' and capture['pixel'] == [255,0,0]
                     and capture['reads']['now_ns'] == 4000000 and capture['reads']['control']['generation'] == generation
                     and app['terminate']['result']['state'] == 'terminated')
    else:
        state = json.loads((directory/'app/state.json').read_text())
        labels = [n.get('text') for n in ET.parse(root/(capture['phase']+'.xml')).iter('node') if n.get('content-desc') == 'native-time']
        complete &= (state == final['observation'] and state['nano_ns'] == 4000000
                     and all(state[k] == 4 for k in ('uptime_ms','elapsed_ms','wall_ms'))
                     and state['count'] == 1 and state['fired'] == 0 and labels == ['Time: 4 Timer: 0']
                     and capture['pixel'] == [0,255,0] and capture['text'] == 'Count: 1'
                     and app['input']['kind'] == 'adb-tap' and app['stopped'] == dict(pid=pid,process_absent=True))
    record('controller.app_evidence', complete, app)
