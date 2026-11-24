"""System-only mode must preserve input uncertainty and cannot acquire a consumer app."""
from dataclasses import asdict
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from native_ios_ui_bridge import Bridge


class SystemBridgeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(); self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.descriptor = self.root/'ui-control.json'
        self.runner = dict(simulator='abcd0000-0000-0000-0000-000000000001',
                           generation='abcd0000-0000-0000-0000-000000000002', pid=123)
        self.descriptor.write_text(json.dumps(dict(generation=self.runner['generation'], pid=123,
            port=1, status='ready', last_id=0, system_alerts=True, system_only=True)))
        self.catalog = json.loads((Path(__file__).resolve().parents[1]/
            'crates/core/tests/fixtures/ios-system-ui.json').read_text())
        self.artifacts = []
        for name in ('screen.png', 'hierarchy.txt'):
            raw = name.encode(); path = 'observations/'+self.catalog['observation']+'/'+name
            file = self.root/path; file.parent.mkdir(parents=True, exist_ok=True); file.write_bytes(raw)
            self.artifacts.append(dict(path=path, bytes=len(raw), sha256=hashlib.sha256(raw).hexdigest()))
        self.calls = []; self.lose = False; self.publish = True
        self.bridge = Bridge(self.descriptor, self.root/'client', self.transport, system_only=True)
        self.initialize = dict(call='initialize', identity=self.runner, source_sha256='a'*64)
        self.bridge.handle(self.initialize)

    def transport(self, endpoint, request, **options):
        self.calls.append(asdict(request))
        if request.operation == 'observe_system_alerts':
            result = dict(state='system_observed', catalog=self.catalog, artifacts=self.artifacts)
        elif request.operation == 'set_host_app_state':
            args=request.arguments
            result=dict(state='host_app_state_changed',change=dict(bundle=args['bundle'],pid=args['pid'],
                requested=args['state'],before_state=4,app_state=3))
        else:
            result = dict(input='xctest_system_tap', **request.arguments)
        reply = dict(generation=request.generation, pid=123, port=1, last_id=request.request_id,
                     request=asdict(request), status='ok', result=result)
        if self.publish: self.descriptor.write_text(json.dumps(reply))
        if self.lose: raise EOFError('reply unavailable')
        return reply

    def call(self, number, operation, **arguments):
        return dict(call='execute', identity=self.runner, app=None,
                    request=dict(generation=self.runner['generation'], request_id=number,
                                 operation=operation, arguments=arguments))

    def test_system_input_recovers_without_app_identity_or_redelivery(self):
        observed = self.bridge.handle(self.call(1, 'observe_system_alerts'))
        self.assertEqual(observed['completion']['outcome']['catalog'], self.catalog)
        arguments = dict(observation=self.catalog['observation'],
                         reference=self.catalog['alerts'][0]['buttons'][1]['reference'])
        request = self.call(2, 'tap_system_button', **arguments)
        self.lose = True
        accepted = self.bridge.handle(request)  # Saved receipt resolves a lost transport response.
        self.assertEqual(accepted['completion']['outcome']['outcome'], 'system_button_tapped')
        self.descriptor.unlink()
        reopened = Bridge(self.descriptor, self.root/'client', self.transport, system_only=True)
        self.assertEqual(reopened.handle(dict(request, call='recover')), accepted)
        with self.assertRaises(RuntimeError): reopened.handle(self.call(3, 'tap_system_button', **arguments))
        self.assertEqual(len(self.calls), 2)
        self.assertFalse((self.root/'client/app.json').exists())

    def test_external_host_state_shares_cursor_and_recovers_without_clock_or_another_effect(self):
        self.bridge.handle(self.call(1,'observe_system_alerts'))
        request=self.call(2,'set_host_app_state',bundle='dev.example.flutter',pid=456,state='background')
        self.lose=True
        result=self.bridge.handle(request)
        self.assertEqual(result['completion']['outcome']['change']['app_state'],3)
        self.descriptor.unlink()
        recovered=Bridge(self.descriptor,self.root/'client',self.transport,system_only=True).handle(dict(request,call='recover'))
        self.assertEqual(recovered,result)
        self.assertFalse((self.root/'client/app.json').exists())
        self.assertEqual(len(self.calls),2)
        with self.assertRaises(RuntimeError):
            self.bridge.handle(self.call(3,'tap_system_button',observation=self.catalog['observation'],
                reference=self.catalog['alerts'][0]['buttons'][1]['reference']))
        self.assertEqual(len(self.calls),2)

    def test_missing_receipt_fences_new_work_and_app_operations_never_dispatch(self):
        for operation in ('launch', 'terminate', 'finish', 'observe_targets'):
            with self.subTest(operation=operation), self.assertRaises(ValueError):
                self.bridge.handle(self.call(1, operation))
        with self.assertRaises(ValueError):
            self.bridge.handle(dict(self.call(1, 'observe_system_alerts'), app=self.runner))
        with self.assertRaises(RuntimeError):
            Bridge(self.descriptor, self.root/'client', self.transport, generic=True).handle(self.initialize)
        self.assertEqual(self.calls, [])
        self.lose = True; self.publish = False
        request = self.call(1, 'observe_system_alerts')
        with self.assertRaises(EOFError): self.bridge.handle(request)
        for call in (dict(request, call='recover'), self.call(2, 'observe_system_alerts')):
            with self.assertRaises(RuntimeError): self.bridge.handle(call)
        self.assertEqual(len(self.calls), 1)


if __name__ == '__main__': unittest.main()
