"""Same-install owner continuity and failure fencing at the OS effect boundary."""
import json
from pathlib import Path
import plistlib
import tempfile
import time
import unittest

from flutter_host_commands import bundle_inventory
from flutter_ios_app import IosApp


class IosAppTests(unittest.TestCase):
    def test_initial_data_refuses_existing_running_and_changed_container(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);app,state,_=self.fixture(root,'normal');deadline=time.monotonic()+5
            original=app._command;installed=False
            def command(name,argv,deadline,absent_ok=False):
                if name=='fresh-apps':return plistlib.dumps({app.bundle:{} } if installed else {}).decode()
                if name=='fresh-apps-xml':return ''
                return original(name,argv,deadline,absent_ok)
            app._command=command
            app.require_fresh(deadline);installed=True
            with self.assertRaises(ValueError):app.require_fresh(deadline)
            identity=app.prelaunch_identity(deadline)
            state['pid']=123
            with self.assertRaises(ValueError):app.prelaunch_identity(deadline)
            state['pid']=None
            app.initial_installation=identity['containers']
            state['data']=root/'replacement';state['data'].mkdir()
            with self.assertRaisesRegex(RuntimeError,'prepared container'):app.start(root/'first',deadline)
            self.assertEqual(state['effects'],[])

    def fixture(self, root, mode):
        product = root/'Runner.app';product.mkdir()
        (product/'Info.plist').write_bytes(plistlib.dumps(dict(CFBundleIdentifier='org.example.client', CFBundleExecutable='Runner')))
        (product/'Runner').write_bytes(b'app')
        data = root/'data';data.mkdir();(data/'note.json').write_bytes(b'committed work')
        app = IosApp('ABCD0000-0000-0000-0000-000000000001', product, bundle_inventory(product), root/'app', {})
        state = dict(pid=None, launches=0, data=data, effects=[])
        def command(name, argv, deadline, absent_ok=False):
            if argv[0] == 'ps':
                if argv[1] == '-axo':
                    if state['pid'] is None:return ''
                    row=f"{state['pid']} Fri Oct 23 19:00:00 2026 {product}/Runner"
                    return row+(f'\n999 Fri Oct 23 19:00:00 2026 {product}/Runner' if state.get('duplicate') else '')
                if state.get('duplicate') and int(argv[2])==999:return f'Fri Oct 23 19:00:00 2026 {product}/Runner'
                return '' if int(argv[2]) != state['pid'] else f'Fri Oct 23 19:00:00 2026 {product}/Runner'
            if argv[2] == 'get_app_container': return str(product if argv[-1] == 'app' else state['data'])
            if argv[2] == 'spawn':
                if state.get('endpoint') is None:return 'No endpoint yet'
                return ('Filtering processID and eventMessage CONTAINS "FRONTCAP_FLUTTER_ENDPOINT "\n'
                        '2026-10-23 19:00:00 Runner[101] (Flutter) flutter: FRONTCAP_FLUTTER_ENDPOINT '+state['endpoint'])
            state['effects'].append(argv[2])
            if argv[2] == 'openurl':
                state['url']=argv[-1];state['endpoint']=None
                if mode=='lost-url':raise RuntimeError('Lost URL acceptance')
                return ''
            if argv[2] == 'launch':
                state['launches'] += 1;state['pid'] = 100+state['launches']
                if state['launches'] > 1 and mode == 'lost-launch': raise RuntimeError('Lost launch response')
                stdout = Path(next(v.split('=', 1)[1] for v in argv if v.startswith('--stdout=')))
                state['endpoint'] = json.dumps(dict(generation=str(state['launches'])*48, token='b'*48, port=12345, scenario='session'))
                stdout.write_text('' if mode == 'unified-log' else 'flutter: FRONTCAP_FLUTTER_ENDPOINT '+state['endpoint']+'\n')
                return 'org.example.client: '+str(state['pid'])
            if argv[2] == 'terminate':
                state['pid'] = None
                if mode == 'changed-container':
                    state['data'] = root/'replacement-data';state['data'].mkdir()
                if mode == 'lost-terminate': raise RuntimeError('Lost terminate response')
                return ''
            raise AssertionError(argv)
        app._command = command
        return app, state, data

    def test_independent_host_transition_preserves_owner_and_fences_process_change(self):
        from unittest.mock import Mock
        for replace in (False,True):
            with self.subTest(replace=replace),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);app,state,_=self.fixture(root,'normal');deadline=time.monotonic()+5
                original=app.start(root/'first',deadline)
                runner=Mock();runner.identity=dict(simulator=app.simulator,generation='runner',pid=999)
                call=dict(call='execute',identity=runner.identity,app=None,request=dict(operation='set_host_app_state',
                    arguments=dict(bundle=app.bundle,pid=original['process']['pid'],state='background')))
                def execute(request,budget):
                    if replace:state['pid']+=1
                    return dict(completion=dict(native='source receipt'))
                runner.execute.side_effect=execute
                if replace:
                    with self.assertRaises(RuntimeError):app.set_app_state(root/'home','background',deadline,runner,call)
                    with self.assertRaises(RuntimeError):app.set_app_state(root/'repeat','background',deadline,runner,call)
                else:
                    result=app.set_app_state(root/'home','background',deadline,runner,call)
                    self.assertEqual(result['before'],original);self.assertEqual(result['after'],original)
                self.assertEqual(runner.execute.call_count,1)
                self.assertEqual(state['effects'],['launch'])

    def test_restart_replaces_process_and_generation_without_install_or_data_reconstruction(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory);app, state, data = self.fixture(root, 'unified-log')
            deadline = time.monotonic()+5
            before = app.start(root/'first', deadline)
            app.terminate(root/'termination', deadline)
            after = app.start(root/'second', deadline)
            self.assertNotEqual(before['process'], after['process'])
            self.assertNotEqual(before['endpoint']['generation'], after['endpoint']['generation'])
            self.assertEqual(before['containers'], after['containers'])
            self.assertEqual((data/'note.json').read_bytes(), b'committed work')
            self.assertEqual(state['effects'], ['launch', 'terminate', 'launch'])
            with self.assertRaises(RuntimeError): app.start(root/'repeat', deadline)
            self.assertEqual(state['effects'], ['launch', 'terminate', 'launch'])

    def test_uncertain_effect_or_changed_container_never_allows_another_launch(self):
        for mode in ['lost-terminate', 'lost-launch', 'changed-container']:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory);app, state, _ = self.fixture(root, mode)
                deadline = time.monotonic()+5
                app.start(root/'first', deadline)
                with self.assertRaises((RuntimeError, ValueError)):
                    app.terminate(root/'termination', deadline)
                    app.start(root/'second', deadline)
                effects = list(state['effects'])
                for action in [app.start, app.terminate]:
                    with self.assertRaises(RuntimeError): action(root/'retry', deadline)
                self.assertEqual(state['effects'], effects)
                self.assertTrue(app.uncertain)

    def test_cold_url_waits_for_confirmation_then_discovers_one_new_process_without_relaunch(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);app,state,data=self.fixture(root,'unified-log');deadline=time.monotonic()+5
            before=app.start(root/'first',deadline);app.terminate(root/'termination',deadline)
            accepted=app.begin_entry(root/'cold',deadline,'client-test://item/1')
            self.assertEqual(accepted['request']['generation'],before['endpoint']['generation'])
            self.assertEqual(accepted['confirmation'],accepted['destination'])
            self.assertEqual(accepted['destination'],'unobserved')
            for i in range(2):
                self.assertEqual(app.poll_entry(root/('waiting-'+str(i)),deadline),dict(state='waiting',phase='process'))
            with self.assertRaises(RuntimeError):app.ensure_running(deadline)
            with self.assertRaises(RuntimeError):app.start(root/'fallback',deadline)
            with self.assertRaises(RuntimeError):app.begin_entry(root/'replay',deadline,'client-test://item/2')
            state['pid']=202
            self.assertEqual(app.poll_entry(root/'endpoint-pending',deadline)['phase'],'endpoint')
            state['endpoint']=json.dumps(dict(generation='c'*48,token='d'*48,port=12346,scenario='session'))
            ready=app.poll_entry(root/'ready',deadline);after=ready['application']
            self.assertEqual(ready['state'],'ready');self.assertEqual(ready['entry'],accepted)
            self.assertNotEqual(after['process'],before['process'])
            self.assertEqual(after['containers'],before['containers'])
            self.assertEqual((data/'note.json').read_bytes(),b'committed work')
            self.assertEqual(state['effects'],['launch','terminate','openurl'])
            app.ensure_running(deadline)
            with self.assertRaises(RuntimeError):app.poll_entry(root/'adopt',deadline)

    def test_cold_url_uncertainty_and_changed_process_never_authorize_fallback_or_adoption(self):
        for failure in ('lost-url','duplicate','replaced','old-generation'):
            with self.subTest(failure=failure),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);app,state,_=self.fixture(root,failure);deadline=time.monotonic()+5
                before=app.start(root/'first',deadline);app.terminate(root/'termination',deadline)
                with self.assertRaises((ValueError,RuntimeError)):
                    app.begin_entry(root/'cold',deadline,'client-test://item/1')
                    state['pid']=202
                    if failure=='replaced':
                        app.poll_entry(root/'pending',deadline);state['pid']=203
                    if failure=='duplicate':state['duplicate']=True
                    if failure=='old-generation':state['endpoint']=json.dumps(before['endpoint'])
                    app.poll_entry(root/'failed',deadline)
                effects=list(state['effects'])
                self.assertTrue(app.uncertain)
                with self.assertRaises(RuntimeError):app.start(root/'fallback',deadline)
                with self.assertRaises(RuntimeError):app.begin_entry(root/'repeat',deadline,'client-test://item/1')
                with self.assertRaises(RuntimeError):app.poll_entry(root/'adopt',deadline)
                self.assertEqual(state['effects'],effects)
