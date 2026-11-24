"""Require tap recovery to agree with live UI captures and actual app input logs."""
import json

from native_controller_recovery import PHASES
from native_ios_ui import UIRequest, validate_reply


def check_ui_recovery(root, record):
    directory = root/'ui-recovery'
    report = json.loads((directory/'report.json').read_text())
    cases = report['cases']
    generation, pid = cases[0]['before']['generation'], cases[0]['before']['pid']
    lifecycle = json.loads((root/'clock-lifecycle.json').read_text())['cases']
    def confirmed(reply, request):
        try:
            validate_reply(reply, request)
            return reply['generation'] == generation and reply['pid'] == pid
        except (RuntimeError, ValueError, KeyError, TypeError):
            return False
    record('ui_recovery.cases', report['progress'] == 'finished'
           and [r['phase'] for r in cases] == list(PHASES), report)
    apps = []
    for i, row in enumerate(cases):
        phase = row['phase']; name = 'ui_recovery.'+phase
        before, result, marker = row['before'], row['resolution'], row['checkpoint']
        number = before['last_id']+1
        request = UIRequest(generation, number, 'tap', dict(target='native-increment'))
        record(name+'.death', row['exit_code'] == -9 and marker == row['checkpoint_after']
               and marker['pid'] == row['controller_pid'] and marker['pid'] != pid and marker['phase'] == phase
               and marker['status'] == 'waiting' and marker['wait_s'] == 10
               and confirmed(result, request) and marker['request'] == result['request']
               and row['lock_held'] is True and row['lock_released'] is True
               and not (directory/(phase+'.stdout')).read_text(), marker)
        committed = phase == 'after-commit'; pending = phase in ('after-pending','after-reply')
        target = row['target_at_death']
        record(name+'.persistence', before == row['launch']
               and confirmed(before, UIRequest(**before['request'])) and before['request']['operation'] == 'launch'
               and row['state_at_death'] == dict(generation=generation, next_id=number+int(committed),
                    pending=marker['request'] if pending else None, last_reply=target if committed else before)
               and (confirmed(target, request) if phase in ('after-reply','after-commit') else target == before), row['state_at_death'])
        resume = row['resume']
        record(name+'.recovery', confirmed(result, request) and result['replayed'] is (phase == 'after-reply')
               and ((resume['exit_code'] == 0 and resume['reply'] == result) if pending else
                    (resume['exit_code'] != 0 and resume['reply'] is None and 'No pending UI command' in resume['error']))
               and row['local_outcome'] == result and row['outcome_without_target_change'] is True
               and json.loads((directory/(phase+'-outcome.stdout')).read_text()) == result
               and row['state_resolved'] == dict(generation=generation,next_id=number+1,pending=None,last_reply=result), result)
        capture = row['capture']; observation = capture['result']
        launch_reads = row['launch']['result']['reads']; app = launch_reads['control']; apps.append(app['generation'])
        episode = lifecycle[i//2]; side = 'new' if i%2 else 'old'
        archive = f'clock-lifecycle/{episode["phase"]}/{side}'
        receipt = json.loads((root/archive/'clock-control.json').read_text())
        actions = [s for s in (root/archive/'input-events.txt').read_text().splitlines() if s.startswith('button action ')]
        record(name+'.live_input', row['app_archive'] == archive
               and row['launch'] == episode['replacement_launch' if i%2 else 'launch']
               and result == episode['replacement_input' if i%2 else 'input']
               and launch_reads['now_ns'] == 0 and row['launch']['result']['text'] == 'Count: 0'
               and confirmed(capture, UIRequest(generation, number+1, 'capture',
                    dict(name='ui-death-'+phase,clock='Time: 0',text='Pending')))
               and observation['reads']['control']['generation'] == receipt['generation'] == app['generation']
               and observation['reads']['control']['pid'] == receipt['pid'] == app['pid']
               and observation['reads']['now_ns'] == 0 and observation['pixel'] == [255,0,0]
               and all(abs(observation['reads'][k]) <= .000001 for k in ('uptime_s','wall_s','mach_s'))
               and actions == ['button action increment'], dict(capture=capture, actions=actions, app=app))
    state = json.loads((root/'ui-controller/ui-client.json').read_text())
    final = json.loads((root/'ui-control.json').read_text())
    record('ui_recovery.finished_bridge', len(set(apps)) == 4
           and confirmed(final, UIRequest(**final['request'])) and final['request']['operation'] == 'finish'
           and final['last_id'] > cases[-1]['capture']['last_id']
           and state == dict(generation=generation,next_id=final['last_id']+1,pending=None,last_reply=final), state)
