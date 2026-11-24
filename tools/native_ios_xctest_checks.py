"""Independent receipt, input-effect and replacement checks for XCTest death."""
import hashlib
import json
import math

from native_clock_control import Request
from native_ios_control import validate_reply as validate_clock
from native_ios_ui import UIRequest, validate_reply


def check_xctest(root, record, expected_hold_us):
    def read(path):
        return json.loads(path.read_text())
    def lines(path):
        return [json.loads(line) for line in path.read_text().splitlines()]
    def confirmed(reply, request, validator=validate_reply):
        try:
            validator(reply, request)
            return True
        except (RuntimeError, ValueError, KeyError, TypeError, AttributeError):
            return False
    report = read(root/'report.json')
    rows = report['cases']
    record('xctest.cases', report.get('progress') == 'finished' and not report.get('error')
           and [r['phase'] for r in rows] == ['before-tap', 'after-tap', 'after-receipt'], report)
    hold = report['hold_us']
    record('xctest.hold_requested', type(hold) is int and hold > 0
           and (expected_hold_us is None or hold == expected_hold_us), hold)
    generations, app_generations = [], []
    for row in rows:
        phase = row['phase']; directory = root/phase; prefix = 'xctest.'+phase
        terminal = phase == 'after-receipt'
        launch, fresh = row['launch'], row['replacement_launch']
        request = UIRequest(launch['generation'], 2, 'tap', dict(target='native-increment', stop_at=phase))
        generations.extend([launch['generation'], fresh['generation']])
        marker, receipt = row['checkpoint'], row['receipt']
        record(prefix+'.checkpoint', row['request'] == marker['request'] == request.__dict__
               and marker == row['checkpoint_after'] == read(directory/'old-ui/ui-checkpoint.json')
               and marker['phase'] == phase and marker['status'] == 'waiting' and marker['wait_s'] == 10
               and marker['pid'] == launch['pid'] and row['signal'] == 9
               and read(directory/'before-kill.json') == dict(request=row['request'], checkpoint=marker, identity=row['identity'])
               and '/'+report['simulator']+'/' in row['identity']
               and '/NativeProbeTests-Runner.app/NativeProbeTests-Runner' in row['identity'], marker)
        death = row['stopped']
        old_session = read(directory/'old-session/session.json')
        record(prefix+'.death', death == dict(pid=launch['pid'], port=launch['port'], process_absent=True, port_closed=True)
               and type(row['controller_exit']) is int and row['controller_exit'] != 0
               and row['controller_pid'] not in (launch['pid'], old_session['pid'])
               and row['xcodebuild_exit'] == old_session['exit_code'] and row['xcodebuild_exit'] != 0
               and not old_session.get('forced_stop')
               and 'EOFError' in (directory/'tap.stderr').read_text()
               and read(directory/'old-ui/ui-control.json') == receipt, dict(death=death, session=old_session))
        before = dict(generation=launch['generation'], next_id=2, pending=row['request'], last_reply=launch)
        resolved = dict(generation=launch['generation'], next_id=3, pending=None, last_reply=receipt) if terminal else before
        attempts = lines(directory/'old-controller/ui-commands.jsonl')
        recoveries = [a for a in attempts if a.get('source') == 'receipt']
        transports = [a for a in attempts if a.get('source') == 'transport']
        recovery = row['recovery']
        valid = (confirmed(receipt, request) and recovery['reply'] == receipt and recovery['exit_code'] == 0
                 and row['outcome']['reply'] == receipt and row['outcome']['exit_code'] == 0) if terminal else (
            receipt['status'] == 'executing' and receipt['request'] == row['request'] and receipt['last_id'] == 1
            and recovery['reply'] is None and recovery['exit_code'] != 0
            and row['blocked_input']['exit_code'] != 0 and 'Pending UI command' in row['blocked_input']['error'])
        record(prefix+'.recovery', valid and row['pending_before'] == before
               and row['pending_after'] == row['pending_final'] == read(directory/'old-controller/ui-client.json') == resolved
               and len(recoveries) == 1 and recoveries[0]['request'] == row['request']
               and recoveries[0].get('reply') == receipt
               and ('error' not in recoveries[0] if terminal else 'Unconfirmed UI outcome' in recoveries[0].get('error', '')),
               dict(recovery=recovery, state=row['pending_final']))
        record(prefix+'.no_retransmission', len(transports) == (2 if terminal else 3)
               and transports[0].get('reply') == launch
               and transports[1]['request'] == row['request'] and 'EOFError' in transports[1].get('error', '')
               and 'reply' not in transports[1]
               and (terminal or ('generation changed' in transports[2].get('error', '') and 'reply' not in transports[2]))
               and row['replacement_receipt_unchanged'] is True
               and row['resume_error'] == ('No pending UI command' if terminal else 'UI generation changed; old input remains unresolved'), transports)
        foreign = row['foreign_request']
        record(prefix+'.generation_fence', fresh['generation'] != launch['generation']
               and foreign['generation'] == fresh['generation'] and foreign['request'] == row['request']
               and foreign['status'] == 'rejected' and foreign['error'] == 'generation_mismatch'
               and foreign['last_id'] == 1, foreign)
        actions = [[line for line in (directory/(side+'-app')/'input-events.txt').read_text().splitlines()
                    if line.startswith('button action ')] for side in ('old', 'new')]
        record(prefix+'.input_effects', actions == [([] if phase == 'before-tap' else ['button action increment']),
                                                  ['button action increment']], actions)
        initial, held, old_clock = row['replacement_before'], row['held'], row['old_clock']
        app_generations.extend([old_clock['generation'], initial['generation']])
        record(prefix+'.frozen', all(confirmed(r, Request(r['generation'], n, 'read'), validate_clock)
                                     and r['now_ns'] == 0 for r,n in ((old_clock,1),(initial,1),(held,2)))
               and old_clock['generation'] == launch['result']['reads']['control']['generation']
               and read(directory/'old-app/clock-control.json') == old_clock
               and initial['generation'] == held['generation'] == fresh['result']['reads']['control']['generation']
               and old_clock['generation'] != initial['generation']
               and math.isfinite(row['hold_s']) and hold/1e6-.001 <= row['hold_s'] <= hold/1e6+2,
               dict(initial=initial, held=held, hold_s=row['hold_s']))
        advances = row['advances']; raw = lines(directory/'new-app/clock-advances.jsonl')
        record(prefix+'.advance_once', len(advances) == len(raw) == 2 and all(
            confirmed(r, Request(initial['generation'], i+3, 'advance', delta), validate_clock)
            and r['applied'] is True and r['status'] == 'ok' and r['now_ns'] == instant
            and log['request'] == r['request'] and log['now_ns'] == instant
            for i,(r,log,delta,instant) in enumerate(zip(advances,raw,(4999000000,1000000),(4999000000,5000000000)))), advances)
        ui = [fresh, row['replacement_input'], *row['captures'], row['finish']]
        record(prefix+'.replacement_operations', len(ui) == 6 and all(
            confirmed(r, UIRequest(fresh['generation'], i, op, args)) for i,(r,op,args) in enumerate(zip(ui,
                ['launch','tap','capture','capture','capture','finish'], [{}, dict(target='native-increment'),
                 *[dict(name=phase+'-'+suffix, clock='Time: '+str(instant), text=label)
                   for suffix,instant,label in [('frozen',0,'Pending'),('before',4999000000,'Pending'),('due',5000000000,'Count: 1')]], {}]),1))
               and confirmed(launch, UIRequest(launch['generation'],1,'launch',{}))
               and fresh['result']['text'] == 'Count: 0' and fresh['result']['reads']['now_ns'] == 0, ui)
        screens = read(directory/'screens.json')
        for i,(capture,instant,label) in enumerate(zip(row['captures'], (0,4999000000,5000000000), ('Pending','Pending','Count: 1'))):
            value = capture['result']; image = directory/'new-ui'/(value['phase']+'.png')
            screen = screens[value['phase']]
            record(prefix+'.capture.'+str(i), value['text'] == label and value['clock'] == 'Time: '+str(instant)
                   and value['reads']['now_ns'] == instant and value['reads']['control']['generation'] == initial['generation']
                   and screen['sha256'] == hashlib.sha256(image.read_bytes()).hexdigest()
                   and screen['sample'] == value['sample'] and screen['screen_points'] == value['screen_points']
                   and screen['pixel'] == ([255,0,0] if i < 2 else [0,255,0])
                   and len(screen['size']) == 2 and min(screen['size']) > 0
                   and label in (directory/'new-ui'/(value['phase']+'.txt')).read_text(), dict(capture=value, screen=screen))
        new_session = read(directory/'new-session/session.json')
        record(prefix+'.retirement', row['replacement_xcodebuild_exit'] == new_session['exit_code'] == 0
               and not new_session.get('forced_stop')
               and all(row[key] == dict(pid=endpoint['pid'], port=endpoint['port'], process_absent=True, port_closed=True)
                       for key,endpoint in [('old_app_stopped',old_clock), ('replacement_app_stopped',initial),
                                            ('replacement_bridge_stopped',fresh)])
               and read(directory/'new-controller/ui-client.json') == dict(generation=fresh['generation'], next_id=7,
                                                                           pending=None, last_reply=row['finish'])
               and read(directory/'new-clock/clock-client.json') == dict(generation=initial['generation'], next_id=5,
                                                                        pending=None, last_reply=advances[-1])
               and read(directory/'new-app/clock-control.json') == advances[-1]
               and read(directory/'new-ui/ui-control.json') == row['finish'], new_session)
    record('xctest.distinct_generations', len(set(generations)) == len(set(app_generations)) == 6,
           dict(ui=generations, app=app_generations))
