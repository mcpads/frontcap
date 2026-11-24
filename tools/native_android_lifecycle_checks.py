"""Require recorded recovery or retained uncertainty, with independent replacement app evidence."""
import json
import xml.etree.ElementTree as ET

from native_android_processes import process_absent
from native_android_control import Request, validate_reply


def check_lifecycle(root, record):
    report = json.loads((root/'clock-lifecycle.json').read_text())
    rows = report['cases']
    record('lifecycle.cases', report.get('progress') == 'finished'
           and [r['phase'] for r in rows] == ['before-advance','after-advance','after-receipt'], report)
    generations = []
    def confirmed(reply, request):
        try:
            validate_reply(reply, request)
            return reply['status'] == 'ok'
        except (RuntimeError, ValueError, KeyError, TypeError):
            return False
    def clocks(observation, millis, count):
        return (all(observation[k] == millis for k in ('uptime_ms','elapsed_ms','wall_ms'))
                and observation['nano_ns'] == millis*1000000 and observation['count'] == count
                and observation['fired'] == 0 and observation['manual'] is True and observation['host_render'] is True)
    for row in rows:
        phase = row['phase']
        prefix = 'lifecycle.'+phase
        directory = root/'clock-lifecycle'/phase
        terminal = phase == 'after-receipt'
        request = Request(**row['request'])
        marker, receipt = row['checkpoint'], row['receipt']
        fresh, advance, after = (row[k] for k in ('replacement_before','replacement_advance','replacement_after'))
        generations.extend([request.generation, fresh['generation']])
        record(prefix+'.checkpoint', marker == row['checkpoint_after']
               and marker['status'] == 'waiting' and marker['phase'] == phase and marker['wait_ms'] == 5000
               and marker['request'] == row['request'] and marker['generation'] == request.generation
               and marker['pid'] == row['before']['pid'] and marker['now_ns'] == (0 if phase == 'before-advance' else 1000000)
               and request.request_id == 2 and request.operation == 'advance' and request.delta_ns == 1000000
               and json.loads((directory/'old/clock-checkpoint.json').read_text()) == marker, marker)
        absent = True
        for side, endpoint, observation in (('old', row['before'], row['stopped']), ('new', after, row['replacement_stopped'])):
            inventory = (directory/(side+'-processes.txt')).read_text()
            absent &= (observation['process_absent'] is True and observation['pid'] == endpoint['pid']
                       and process_absent(inventory, endpoint['pid'], 'dev.frontcap.s18.probe'))
        record(prefix+'.termination', absent and type(row['controller_exit']) is int and row['controller_exit'] != 0
               and 'Receiver completion unavailable' in (directory/'advance.stderr').read_text()
               and not (directory/'advance.stdout').read_text(), dict(old=row['stopped'], new=row['replacement_stopped']))
        pending = dict(generation=request.generation, next_id=2, pending=row['request'], last_reply=row['before'])
        resolved = dict(generation=request.generation, next_id=3, pending=None, last_reply=receipt) if terminal else pending
        attempts = [json.loads(line) for line in (directory/'old-controller/clock-commands.jsonl').read_text().splitlines()]
        recovered = [a for a in attempts if a.get('source') == 'receipt']
        recovery = row['recovery']
        valid_receipt = (confirmed(receipt, request) and receipt['applied'] is True and receipt['now_ns'] == 1000000
                         and recovery == dict(exit_code=0, reply=receipt)) if terminal else (
            receipt['status'] == 'executing' and receipt['request'] == row['request'] and receipt['last_id'] == 1
            and recovery['exit_code'] != 0 and recovery['reply'] is None)
        record(prefix+'.recovery', valid_receipt
               and json.loads((directory/'old/clock-control.json').read_text()) == receipt
               and row['pending_before'] == pending and row['pending_after'] == row['pending_final'] == resolved
               and json.loads((directory/'old-controller/clock-client.json').read_text()) == resolved
               and len(recovered) == 1 and recovered[0]['request'] == row['request'] and recovered[0].get('reply') == receipt
               and (('error' not in recovered[0]) if terminal else 'Clock outcome unavailable' in recovered[0].get('error','')),
               dict(recovery=recovery, pending=row['pending_final']))
        transport = [a for a in attempts if a.get('source') == 'broadcast']
        record(prefix+'.no_replay', row['replacement_receipt_unchanged'] is True
               and row['resume_error'] == ('No pending clock command' if terminal else 'Target generation changed; old request remains unresolved')
               and len(transport) == 2 and transport[1]['request'] == row['request']
               and 'Receiver completion unavailable' in transport[1].get('error','') and 'reply' not in transport[1]
               and transport[1].get('broadcast_output','').strip().endswith('Broadcast completed: result=0'), transport)
        foreign = row['foreign_request']
        record(prefix+'.generation_fence', fresh['generation'] != request.generation and fresh['now_ns'] == 0
               and foreign.get('generation') == fresh['generation'] and foreign.get('request') == row['request']
               and foreign.get('status') == 'rejected' and foreign.get('error') == 'generation_mismatch'
               and foreign.get('applied') is False and foreign.get('before_ns') == foreign.get('now_ns') == 0
               and foreign.get('last_id') == 1, foreign)
        old_state = json.loads((directory/'old/state.json').read_text())
        record(prefix+'.old_state', confirmed(row['before'], Request(request.generation, 1, 'read'))
               and clocks(row['before']['observation'], 0, 1) and clocks(old_state, int(terminal), 1)
               and row['initial_screen']['text'] == 'Count: 0' and row['initial_screen']['pixel'] == [255,0,0], old_state)
        new_state = json.loads((directory/'new/state.json').read_text())
        record(prefix+'.replacement_clock', confirmed(fresh, Request(fresh['generation'],1,'read'))
               and clocks(fresh['observation'],0,0) and confirmed(advance, Request(fresh['generation'],2,'advance',1000000))
               and advance['before_ns'] == 0 and confirmed(after, Request(fresh['generation'],3,'read'))
               and clocks(advance['observation'],1,1) and clocks(after['observation'],1,1)
               and new_state == after['observation'] and after['observation']['sequence'] > advance['observation']['sequence']
               and json.loads((directory/'new/clock-control.json').read_text()) == after,
               dict(before=fresh, advance=advance, after=after))
        capture = row['capture']
        labels = [n.get('text') for n in ET.parse(root/(capture['phase']+'.xml')).iter('node')
                  if n.get('content-desc') == 'native-time']
        record(prefix+'.replacement_input', all(row[k]['kind'] == 'adb-tap' and row[k]['target'] == 'native-increment'
               and row[k]['x'] > 0 and row[k]['y'] > 0 for k in ('input','replacement_input'))
               and row['replacement_screen_before']['text'] == 'Count: 0'
               and row['replacement_screen_before']['pixel'] == [255,0,0]
               and capture['text'] == 'Count: 1' and capture['pixel'] == [0,255,0]
               and labels == ['Time: 1 Timer: 0'], capture)
    record('lifecycle.distinct_generations', len(set(generations)) == 6, generations)
