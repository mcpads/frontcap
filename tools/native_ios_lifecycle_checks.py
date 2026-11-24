"""Cross-check app termination evidence against persisted requests and app-side receipts."""
import json

from native_clock_control import Request
from native_ios_control import validate_reply
from native_ios_ui import UIRequest, validate_reply as validate_ui


def check_lifecycle(root, record):
    report = json.loads((root/'clock-lifecycle.json').read_text())
    rows = report['cases']
    record('lifecycle.cases', report.get('progress') == 'finished'
           and [r['phase'] for r in rows] == ['before-advance', 'after-advance', 'after-receipt'], report)
    generations = []
    def confirmed(reply, request, validator=validate_reply):
        try:
            validator(reply, request)
            return True
        except (RuntimeError, ValueError, KeyError, TypeError):
            return False
    for row in rows:
        phase = row['phase']
        directory = root/'clock-lifecycle'/phase
        prefix = 'lifecycle.'+phase
        request = Request(**row['request'])
        marker, receipt = row['checkpoint'], row['receipt']
        terminal = phase == 'after-receipt'
        generations.extend([request.generation, row['replacement_before']['generation']])
        record(prefix+'.checkpoint', marker == row['checkpoint_after']
               and marker['status'] == 'waiting' and marker['phase'] == phase and marker['wait_ms'] == 10000
               and marker['request'] == row['request'] and marker['generation'] == request.generation
               and marker['pid'] == row['before']['pid'] and marker['now_ns'] == (0 if phase == 'before-advance' else 1000000)
               and request.request_id == 2 and request.operation == 'advance' and request.delta_ns == 1000000,
               marker)
        stopped = row['stopped']
        record(prefix+'.termination', type(row['controller_exit']) is int and row['controller_exit'] != 0
               and stopped['process_absent'] is True and stopped['port_closed'] is True
               and stopped['pid'] == marker['pid'] and stopped['port'] == marker['port']
               and row['controller_pid'] != marker['pid']
               and json.loads((directory/'old/clock-checkpoint.json').read_text()) == marker
               and json.loads((directory/'old/clock-control.json').read_text()) == receipt, stopped)
        expected_pending = dict(generation=request.generation, next_id=2, pending=row['request'], last_reply=row['before'])
        expected_resolved = dict(generation=request.generation, next_id=3, pending=None, last_reply=receipt) if terminal else expected_pending
        attempts = [json.loads(line) for line in (directory/'old-controller/clock-commands.jsonl').read_text().splitlines()]
        recovered = [r for r in attempts if r.get('source') == 'receipt']
        transport = [r for r in attempts if r.get('source') == 'transport']
        recovery = row['recovery']
        valid_receipt = (confirmed(receipt, request) and receipt['applied'] is True and receipt['now_ns'] == 1000000
                         and recovery['reply'] == receipt and recovery['exit_code'] == 0) if terminal else (
            receipt.get('status') == 'executing' and receipt.get('request') == row['request']
            and receipt.get('last_id') == 1 and recovery['reply'] is None and recovery['exit_code'] != 0)
        record(prefix+'.recovery', valid_receipt and row['pending_before'] == expected_pending
               and row['pending_after'] == row['pending_final'] == expected_resolved
               and json.loads((directory/'old-controller/clock-client.json').read_text()) == expected_resolved
               and len(recovered) == 1 and recovered[0]['request'] == row['request']
               and recovered[0].get('reply') == receipt
               and (('error' not in recovered[0]) if terminal else 'Unconfirmed clock outcome' in recovered[0].get('error', '')),
               dict(recovery=recovery, pending=row['pending_final'], attempts=recovered))
        record(prefix+'.no_replay', row['replacement_receipt_unchanged'] is True
               and row['resume_error'] == ('No pending clock command' if terminal else
                                          'Target generation changed; old outcome remains unresolved')
               and len(transport) == (2 if terminal else 3)
               and transport[1]['request'] == row['request'] and 'EOFError' in transport[1].get('error', '')
               and 'reply' not in transport[1]
               and (terminal or ('generation changed' in transport[2].get('error', '') and 'reply' not in transport[2])),
               dict(error=row['resume_error'], attempts=transport))
        fresh = row['replacement_before']
        foreign = row['foreign_request']
        record(prefix+'.generation_fence', fresh['generation'] != request.generation and fresh['now_ns'] == 0
               and fresh['last_id'] == 1 and foreign.get('generation') == fresh['generation']
               and foreign.get('request') == row['request'] and foreign.get('status') == 'rejected'
               and foreign.get('error') == 'generation_mismatch' and foreign.get('applied') is False
               and foreign.get('now_ns') == 0 and foreign.get('last_id') == 1, dict(fresh=fresh, foreign=foreign))
        advance, after = row['replacement_advance'], row['replacement_after']
        logs = [json.loads(line) for line in (directory/'new/clock-advances.jsonl').read_text().splitlines()]
        old_log = directory/'old/clock-advances.jsonl'
        old_advances = [json.loads(line) for line in old_log.read_text().splitlines()] if old_log.exists() else []
        record(prefix+'.replacement_clock', confirmed(fresh, Request(fresh['generation'], 1, 'read'))
               and confirmed(advance, Request(fresh['generation'], 2, 'advance', 1000000))
               and advance['status'] == 'ok' and advance['applied'] is True and advance['before_ns'] == 0
               and confirmed(after, Request(fresh['generation'], 3, 'read')) and after['now_ns'] == 1000000
               and len(logs) == 1 and logs[0]['request'] == advance['request'] and logs[0]['now_ns'] == 1000000
               and len(old_advances) == int(terminal)
               and (not terminal or old_advances[0]['request'] == row['request']), dict(advance=advance, after=after))
        ui = [row[k] for k in ('launch', 'input', 'terminate', 'replacement_launch', 'replacement_input', 'capture', 'replacement_terminate')]
        record(prefix+'.ui_lifecycle', all(confirmed(r, UIRequest(**r['request']), validate_ui) for r in ui)
               and [r['request']['operation'] for r in ui] == ['launch','tap','terminate','launch','tap','capture','terminate']
               and len({r['generation'] for r in ui}) == 1
               and row['launch']['request']['arguments'] == dict(clock_stop_at=phase)
               and row['replacement_launch']['request']['arguments'] == {}
               and row['replacement_launch']['result']['reads']['now_ns'] == 0
               and row['replacement_launch']['result']['text'] == 'Count: 0'
               and row['replacement_stopped']['process_absent'] is True and row['replacement_stopped']['port_closed'] is True,
               ui)
        capture = row['capture']['result']
        actions = [[line for line in (directory/side/'input-events.txt').read_text().splitlines()
                    if line.startswith('button action ')] for side in ('old', 'new')]
        record(prefix+'.replacement_state', actions == [['button action increment']]*2
               and capture['reads']['now_ns'] == 1000000 and capture['reads']['control']['generation'] == fresh['generation']
               and capture['text'] == 'Pending' and capture['clock'] == 'Time: 1000000'
               and capture['pixel'][0] > 200 and max(capture['pixel'][1:]) < 30, dict(capture=capture, actions=actions))
    record('lifecycle.distinct_generations', len(set(generations)) == 6, generations)
