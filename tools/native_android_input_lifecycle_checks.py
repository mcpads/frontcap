"""Check app-death input outcomes against retained receipts, journals and replacement captures."""
import json
import signal

from native_android_processes import process_absent
from native_android_input_observation import app_clocks_match as clocks, input_receipt_matches as confirmed, capture_matches


def check_lifecycle(root, record, *, mode='app'):
    if mode not in ('app', 'app-controller'):
        raise ValueError('Invalid app input interruption mode')
    joint = mode == 'app-controller'
    report = json.loads((root/'input-lifecycle.json').read_text())
    rows = report['cases']
    phases = ['before-touch-up', 'before-action', 'after-action', 'after-receipt']
    record('input_death.cases', report.get('progress') == 'finished' and 'error' not in report
           and report.get('mode', 'app') == mode
           and [r['phase'] for r in rows] == phases, report)
    generations = []
    def capture(value, count, millis, pixel):
        return capture_matches(root, value, count, millis, pixel)
    for row in rows:
        phase = row['phase']
        prefix = 'input_death.'+phase
        directory = root/'input-lifecycle'/phase
        try:
            terminal = phase == 'after-receipt'
            count = int(phase in ('after-action', 'after-receipt'))
            request = row['request']
            before, marker, receipt = row['before'], row['checkpoint'], row['receipt']
            fresh = row['replacement_before']
            generations.extend([before['generation'], fresh['generation']])
            record(prefix+'.checkpoint', marker == row['checkpoint_after']
                   == json.loads((directory/'old/input-checkpoint.json').read_text())
                   and marker['status'] == 'waiting' and marker['phase'] == phase and marker['wait_ms'] == 5000
                   and marker['request'] == request and marker['generation'] == before['generation']
                   and marker['pid'] == before['pid'] and marker['clicks'] == count and marker['last_id'] == int(terminal)
                   and request['request_id'] == 1 and request['generation'] == before['generation']
                   and before['status'] == 'ready' and before['last_id'] == before['clicks'] == 0, marker)
            absent = True
            for side, endpoint, stopped in (('old', before, row['stopped']), ('new', fresh, row['replacement_stopped'])):
                inventory = (directory/(side+'-processes.txt')).read_text()
                absent &= (stopped == dict(pid=endpoint['pid'], process_absent=True)
                           and process_absent(inventory, endpoint['pid'], 'dev.frontcap.s18.probe'))
            cleanup = row['child_cleanup']
            order = (row['controller_finished_monotonic_ns'] < row['stop_started_monotonic_ns'] if joint else
                     row['stop_finished_monotonic_ns'] <= row['controller_finished_monotonic_ns'])
            record(prefix+'.termination', absent and row['lock_released'] is True and row['cleanup_actions'] == []
                   and row['child_retirement'][-1]['processes'] == [] and cleanup['absent'] is True
                   and cleanup['signals'] == [] and cleanup['observations'][-1]['processes'] == []
                   and row['stop_started_monotonic_ns'] < row['stop_finished_monotonic_ns']
                   and order, dict(old=row['stopped'], new=row['replacement_stopped'], child=cleanup))
            transport = row['transport_after']
            initial_transport = row['transport_at_checkpoint']
            transport_end = (transport['phase'] == 'finished' and type(transport.get('exit_code')) is int
                             and transport['started_monotonic_ns'] < transport['finished_monotonic_ns']
                             <= row['controller_finished_monotonic_ns'])
            if joint:
                # A killed parent cannot publish its child's eventual wait status.
                # Absence proves retirement, not a successful exit or cancelled input.
                transport_end = ((transport_end and transport['exit_code'] == 0)
                                 or (transport['phase'] == 'started' and 'exit_code' not in transport
                                     and 'finished_monotonic_ns' not in transport))
            record(prefix+'.actual_transport', transport == json.loads((directory/'old-controller/input-transport.json').read_text())
                   and transport_end
                   and transport['parent_pid'] == row['controller_pid'] and transport['request'] == request
                   and transport['identity'] == initial_transport['identity'] == cleanup['identity']
                   and transport['command'][-5:] == ['shell', 'input', 'tap', str(request['x']), str(request['y'])], transport)
            expected_status = 'ok' if terminal else 'armed' if phase == 'before-touch-up' else 'executing'
            state = json.loads((directory/'old/state.json').read_text())
            record(prefix+'.durable_app_state', receipt == row['receipt_at_checkpoint']
                   == json.loads((directory/'old/input-control.json').read_text())
                   and receipt['status'] == expected_status and receipt['request'] == request
                   and receipt['pid'] == before['pid'] and receipt['last_id'] == receipt['clicks'] == int(terminal)
                   and state == row['state_at_checkpoint'] and clocks(state, count, 0)
                   and capture(row['initial_screen'], 0, 0, [255, 0, 0]), dict(receipt=receipt, state=state))
            pending = dict(generation=before['generation'], pid=before['pid'], controller_id=request['controller_id'],
                           next_id=1, pending=request, delivery_phase='dispatched',
                           component='dev.frontcap.s18.probe/dev.frontcap.s18.VirtualActivity')
            resolved = {**pending, 'next_id': 2, 'pending': None, 'delivery_phase': None,
                        'last_reply': receipt} if terminal else pending
            recovery = row['recovery']
            if joint:
                pause = row['controller_pause']
                children = row['other_children_cleanup']
                host_marker = row.get('controller_checkpoint')
                checkpoint_valid = (not terminal or (host_marker == row['controller_checkpoint_after']
                    == json.loads((directory/'old-controller/controller-checkpoint.json').read_text())
                    and host_marker == dict(phase='after-reply', request=request, pid=row['controller_pid'], status='waiting', wait_s=10)))
                actual_tap = (phase != 'before-touch-up' or all(len(row[k]) == 1
                    and row[k][0]['pid'] == transport['pid'] and 'Z' not in row[k][0]['state']
                    for k in ('tap_before_kill', 'tap_after_controller_death')))
                record(prefix+'.controller_death', row['controller_exit_at_checkpoint'] is None
                       and row['controller_exit'] == -int(signal.SIGKILL) and row['lock_held'] is True
                       and pause['parent']['pid'] == row['controller_pid'] and 'T' in pause['parent']['state']
                       and pause['signal'] == int(signal.SIGSTOP)
                       and pause['observed_monotonic_ns'] < row['kill_started_monotonic_ns'] < row['controller_finished_monotonic_ns']
                       and row['client_at_checkpoint'] == row['client_before_kill'] == row['pending_before'] == pending
                       and row['recovery_operation'] == 'recover' and not (directory/'tap.stdout').read_bytes()
                       and all(p['absent'] is True and p['signals'] == [] and p['observations'][-1]['processes'] == [] for p in children)
                       and {p['identity']['pid'] for p in children} == {p['pid'] for p in pause['children'] if p['pid'] != transport['pid']}
                       and checkpoint_valid and actual_tap, dict(pause=pause, pending=row['pending_before'], other_children=children))
            record(prefix+'.resolution', row['pending_after'] == row['pending_final'] == resolved
                   == json.loads((directory/'old-controller/input-client.json').read_text())
                   and row['pending_before'] in (pending, resolved)
                   and ((recovery['exit_code'] == 0 and recovery['reply'] == receipt and confirmed(receipt, request)
                         and clocks(receipt['observation'], 1, 0)) if terminal else
                        (row['controller_exit'] != 0 and recovery['exit_code'] != 0 and recovery['reply'] is None
                         and 'Input outcome unavailable' in recovery['error'] and row['recovery_operation'] == 'recover')),
                   dict(recovery=recovery, final=row['pending_final']))
            attempts = [json.loads(line) for line in (directory/'old-controller/input-commands.jsonl').read_text().splitlines()]
            expected_error = 'No pending input command' if terminal else 'Target generation changed; old input remains unresolved'
            outcome = row['outcome_after_replacement']
            dispatch = (attempts[0]['request'] == request and 'registration' in attempts[0]
                        and 'tap_started_monotonic_ns' in attempts[0]
                        and all('registration' not in a and 'tap_started_monotonic_ns' not in a for a in attempts[1:])) if not joint else (
                        len(attempts) == 2 and attempts[0]['request'] == request
                        and attempts[0]['receipt_only'] is True and attempts[0]['delivery_phase'] == 'dispatched'
                        and attempts[0]['started_monotonic_ns'] > row['stop_finished_monotonic_ns']
                        and attempts[0]['reply'] == receipt
                        and all('registration' not in a and 'tap_started_monotonic_ns' not in a for a in attempts))
            record(prefix+'.no_reinput', row['resume']['exit_code'] != 0 and expected_error in row['resume']['error']
                   and row['replacement_receipt_unchanged'] is True
                   and dispatch
                   and ((outcome['exit_code'] == 0 and outcome['reply'] == receipt) if terminal else
                        (outcome['exit_code'] != 0 and 'Pending command' in outcome['error'])), attempts)
            record(prefix+'.replacement_fence', fresh['status'] == 'ready' and fresh['last_id'] == fresh['clicks'] == 0
                   and fresh['generation'] != before['generation'] and fresh['pid'] != before['pid']
                   and row['foreign_receipt_unchanged'] is True
                   and 'Broadcast completed: result=1, data="generation_mismatch"' in row['foreign_registration'].splitlines()
                   and capture(row['replacement_screen_before'], 0, 0, [255, 0, 0]), fresh)
            first, advance, after = (row[k] for k in ('replacement_clock_before', 'replacement_advance', 'replacement_after'))
            new_input = row['replacement_input']
            record(prefix+'.replacement_work', confirmed(new_input, new_input['request'])
                   and new_input['generation'] == fresh['generation'] and new_input['pid'] == fresh['pid']
                   and new_input['last_id'] == 1 and clocks(new_input['observation'], 1, 0)
                   and json.loads((directory/'new/input-control.json').read_text()) == new_input
                   and all(v['status'] == 'ok' and v['generation'] == fresh['generation'] and v['pid'] == fresh['pid'] for v in (first, advance, after))
                   and first['now_ns'] == advance['before_ns'] == 0 and advance['now_ns'] == after['now_ns'] == 1000000
                   and clocks(first['observation'], 0, 0) and clocks(advance['observation'], 1, 1) and clocks(after['observation'], 1, 1)
                   and after['observation'] == json.loads((directory/'new/state.json').read_text())
                   and after == json.loads((directory/'new/clock-control.json').read_text())
                   and capture(row['capture'], 1, 1, [0, 255, 0]), dict(input=new_input, clock=after, capture=row['capture']))
        except (KeyError, ValueError, TypeError, OSError, IndexError) as error:
            record(prefix+'.complete', False, f'{type(error).__name__}: {error}')
    record('input_death.generations', len(generations) == len(set(generations)) == 8, generations)
