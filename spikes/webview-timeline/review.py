"""Bounded, file-driven native exploration for an already-owned probe.

Commands are consumed once in order. Unknown delivery aborts; this is not a
durable controller or authorization to replay an unfinished command.
"""
import json
import subprocess
import time
from pathlib import Path


def review(args, command, ui, process):
    out = args.output
    checks = {}
    hid_index = 0
    contact_revision = 0
    held_contacts = []
    (out/'review-ready.json').write_text(json.dumps({'next_action':0})+'\n')
    for index in range(24):
        path = out/f'action-{index}.json'
        deadline = time.monotonic()+100
        while not path.exists():
            if process.poll() is not None:
                raise RuntimeError('Native input runner exited during review')
            if time.monotonic()>deadline:
                raise TimeoutError('Action review deadline')
            time.sleep(.1)
        action = json.loads(path.read_text())
        operation = action.get('operation')
        if operation == 'finish':
            if held_contacts:
                raise RuntimeError('Release held contacts before finishing review')
            (out/f'action-{index}.reply.json').write_text('{"finished":true}\n')
            return checks
        if operation not in ('sample','tap','touches','hid_touches','hid_contacts','hid_cancel','capture','background_return'):
            raise ValueError('Unknown review operation')
        steps = action.get('steps',0)
        delta = action.get('delta_ms',50)
        if type(steps) is not int or not 0 <= steps <= 120 or type(delta) is not int or not 0 <= delta <= 1000:
            raise ValueError('Invalid bounded progress')
        if operation in ('capture','background_return') and steps:
            raise ValueError('Capture or lifecycle action cannot advance time')
        _, before = command()
        receipt = None
        lifecycle = None
        if operation == 'background_return':
            hold_ms = action.get('hold_ms',700)
            if type(hold_ms) is not int or not 0 <= hold_ms <= 5000:
                raise ValueError('Invalid background host hold')
            background = ui('background')
            time.sleep(hold_ms/1000)
            foreground = ui('foreground')
            lifecycle = dict(background=background,foreground=foreground,host_hold_ms=hold_ms)
        if operation == 'tap':
            x,y = action['x'],action['y']
            if any(type(v) not in (int,float) or not 0 <= v <= 1 for v in (x,y)):
                raise ValueError('Invalid normalized tap')
            receipt = ui('tap',dict(x=x,y=y))
        if operation == 'touches':
            receipt = ui('touches',dict(paths=action['paths']))
        if operation in ('hid_touches','hid_contacts','hid_cancel'):
            container = Path(subprocess.check_output(['xcrun','simctl','get_app_container',
                args.device,args.bundle_id,'data'],text=True).strip())
            directory = container/'Documents/frontcap-hid'
            endpoint = json.loads((directory/'endpoint.json').read_text())
            if not endpoint['available'] or endpoint['pid'] != before['pid']:
                raise RuntimeError('HID probe unavailable or wrong consumer process')
            hid_index += 1
            request = dict(generation=endpoint['generation'],request_id=hid_index)
            if operation == 'hid_cancel':
                request.update(cancel=True,expected_revision=contact_revision)
            elif operation == 'hid_contacts':
                request.update(contacts=action['contacts'],expected_revision=contact_revision)
            else:
                request['paths'] = action['paths']
            target = directory/f'request-{hid_index}.json'
            if target.exists():
                raise RuntimeError('HID request already exists; never replay unknown input')
            temporary = target.with_suffix('.tmp')
            temporary.write_text(json.dumps(request))
            temporary.rename(target)
            deadline = time.monotonic()+10
            result_path = directory/f'reply-{hid_index}.json'
            while True:
                if result_path.exists():
                    receipt = json.loads(result_path.read_text())
                    if receipt['status'] != 'executing':
                        break
                if time.monotonic()>deadline or process.poll() is not None:
                    raise RuntimeError('HID completion unknown; retire without replay')
                time.sleep(.05)
            if (receipt['status'] != 'enqueued' or receipt['generation'] != endpoint['generation']
                    or receipt['request_id'] != hid_index):
                raise RuntimeError('HID request did not finish enqueueing')
            if operation in ('hid_contacts','hid_cancel'):
                contact_revision = receipt['contact_revision']
                held_contacts = receipt['contacts']
        samples = []
        for step in range(max(steps,1)):
            if steps:
                _, result = command('advance',delta*1_000_000)
                if result['status'] != 'ok' or result['state']['failure']:
                    raise RuntimeError('Review advance failed')
            if operation == 'sample' or step == max(steps,1)-1:
                name = f'action-{index}-{step}.png'
                _, boundary = command()
                if boundary['status'] != 'ok':
                    raise RuntimeError('Review observation unavailable')
                subprocess.run(['xcrun','simctl','io',args.device,'screenshot',str(out/name)],
                               check=True,capture_output=True,timeout=15)
                samples.append(dict(image=name,clock=boundary))
        if lifecycle:
            after = samples[-1]['clock']
            checks.update({
                f'lifecycle_{index}_same_process':before['pid']==after['pid'],
                f'lifecycle_{index}_same_document':before['document']==after['document'],
                f'lifecycle_{index}_same_generation':before['generation']==after['generation'],
                f'lifecycle_{index}_clock_held':before['state']['elapsedMs']==after['state']['elapsedMs'],
                f'lifecycle_{index}_background':lifecycle['background']['result']['app_state'] in (2,3),
                f'lifecycle_{index}_foreground':lifecycle['foreground']['result']['app_state']==4,
            })
        # Keep the XCTest endpoint active and retain its independent UI observation.
        ui('capture')
        reply = dict(before=before,input=receipt,lifecycle=lifecycle,samples=samples,checks=dict(checks))
        target = out/f'action-{index}.reply.json'
        temporary = target.with_suffix('.tmp')
        temporary.write_text(json.dumps(reply,indent=2)+'\n')
        temporary.replace(target)
    raise RuntimeError('Review action budget exhausted')
