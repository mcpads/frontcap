"""Bind Android input receipts to independent clock, state and lifecycle evidence."""
import json

from native_android_input import InputRequest, validate_reply
from native_controller_recovery import PHASES


def check_inputs(root, record, interruption=None):
    lifecycle = json.loads((root/'clock-lifecycle.json').read_text())
    inputs = []
    for row in lifecycle['cases']:
        base = root/'clock-lifecycle'/row['phase']
        inputs.extend([(row['input'], row['before'], base/'old'),
                       (row['replacement_input'], row['replacement_before'], base/'new')])
    app = json.loads((root/'controller-recovery/app.json').read_text())
    clock = json.loads((root/'controller-recovery/report.json').read_text())
    inputs.append((app['input'], clock['initial'], root/'controller-recovery/app'))
    ordinary = json.loads((root/'virtual.json').read_text())
    endpoint = json.loads((root/'app/clock-control.json').read_text())
    if len(ordinary['inputs']) == 1:
        inputs.append((ordinary['inputs'][0], endpoint, root/'app'))
    receipts = []
    for index, (delivery, clock, archive) in enumerate(inputs):
        reply = delivery['receipt']
        try:
            request = InputRequest(**reply['request'])
            validate_reply(reply, request)
            valid = True
        except (RuntimeError, ValueError, KeyError, TypeError):
            valid = False
        observation = reply['observation']
        record(f'input.{index}.acknowledged', valid and reply['request']['request_id'] == 1
               and reply['generation'] == clock['generation'] and reply['pid'] == clock['pid']
               and all(reply['request'][k] == delivery[k] for k in ('target','x','y'))
               and observation['count'] == 1 and observation['fired'] == 0
               and all(observation[k] == 0 for k in ('uptime_ms','elapsed_ms','nano_ns','wall_ms'))
               and observation['manual'] is True and observation['host_render'] is True
               and json.loads((archive/'input-control.json').read_text()) == reply
               and json.loads((archive/'state.json').read_text())['count'] == 1, reply)
        receipts.append(reply)
    record('input.generations', len({r['generation'] for r in receipts}) == 8, [r['generation'] for r in receipts])
    controls = json.loads((root/'input-registration.json').read_text())
    record('input.registration.cases', [r['name'] for r in controls] == ['replay','conflict','owner','generation','gap'], controls)
    for row, error in zip(controls, (None,'request_conflict','controller_mismatch','generation_mismatch','sequence_gap')):
        request = row['request']
        expected = (f'Broadcast completed: result=1, data="{error}"' if error else
                    f'Broadcast completed: result=-1, data="{request["generation"]}:{request["request_id"]}"')
        record('input.registration.'+row['name'], row['receipt'] == receipts[4]
               and expected in row['broadcast'].splitlines(), row)
    if interruption is not None:
        from native_android_input_interruption_checks import check_interruption
        check_interruption(root, record, interruption)
        return
    report = json.loads((root/'input-recovery/report.json').read_text())
    rows = report['cases']
    record('input.recovery.cases', report.get('progress') == 'finished'
           and [r['phase'] for r in rows] == list(PHASES), report)
    for index, row in enumerate(rows):
        prefix = 'input.recovery.'+row['phase']
        marker, state, reply = row['checkpoint'], row['state_at_death'], row['resolution']
        request = marker['request']
        pending = row['phase'] in ('after-pending','after-reply')
        completed = row['phase'] in ('after-reply','after-commit')
        record(prefix+'.death', marker == row['checkpoint_after'] and marker['status'] == 'waiting'
               and marker['phase'] == row['phase'] and marker['pid'] == row['controller_pid'] and marker['wait_s'] == 10
               and row['exit_code'] == -9 and row['lock_held'] is True and row['lock_released'] is True
               and state['generation'] == request['generation'] == row['before']['generation']
               and state['pid'] == row['before']['pid'] == reply['pid']
               and state['controller_id'] == request['controller_id']
               and state['pending'] == (request if pending else None)
               and state['delivery_phase'] == ('prepared' if row['phase'] == 'after-pending' else
                                               'dispatched' if row['phase'] == 'after-reply' else None)
               and state['next_id'] == (2 if row['phase'] == 'after-commit' else 1)
               and row['before']['status'] == 'ready'
               and row['target_at_death'] == (reply if completed else row['before']), row)
        resolved = row['state_resolved']
        record(prefix+'.once', reply == receipts[index] == row['local_outcome']
               and reply['request'] == request and row['outcome_without_target_change'] is True
               and resolved == json.loads((root/row['client']/'input-client.json').read_text())
               and resolved['pending'] is None and resolved['delivery_phase'] is None
               and resolved['next_id'] == 2 and resolved['last_reply'] == reply
               and (row['resume']['reply'] == reply if pending else
                    row['resume']['exit_code'] != 0 and 'No pending input command' in row['resume']['error']), resolved)
