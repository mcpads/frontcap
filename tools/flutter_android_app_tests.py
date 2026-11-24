"""Android process/installation continuity and uncertain SDK effect fencing."""
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from flutter_android_app import AndroidApp
from flutter_host_commands import digest
from native_command_capture import capture


class AndroidAppTests(unittest.TestCase):
    def test_runtime_diagnostics_do_not_replace_or_restart_original_app(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            app, state = self.fixture(root)
            app.start(root/'first', time.monotonic()+5)
            owner, effects = dict(app.owner), list(state['effects'])
            for failed in (False, True):
                output = root/('failed-read' if failed else 'readable')
                def diagnostic(argv, path, **kwargs):
                    self.assertLessEqual(kwargs['timeout'], 3)
                    self.assertLessEqual(kwargs['output_limit'], 2*1024*1024)
                    if '--pid' in argv:
                        self.assertEqual(argv[argv.index('--pid')+1], str(owner['process']['pid']))
                    if failed:
                        raise OSError('Device unavailable')
                    return capture([sys.executable, '-c', 'print("diagnostic")'], path, **kwargs)
                with patch('flutter_android_app.capture_command', diagnostic):
                    app.record_runtime_failure(output, time.monotonic()+3)
                report = json.loads((output/'endpoint-diagnostics.json').read_text())
                self.assertEqual(report['error'] is not None, failed)
                self.assertEqual(app.owner, owner)
                self.assertEqual(state['effects'], effects)

    def test_initial_data_refuses_existing_or_running_app_and_changed_first_installation(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);app,state=self.fixture(root);deadline=time.monotonic()+5
            original=app._command
            installed=False;running=False
            def command(name,args,deadline,absent_ok=False,record=True):
                if name=='fresh-packages':return 'package:'+app.package if installed else 'package:android'
                if name=='unstarted-processes':return 'NAME\n'+(app.package+':worker' if running else 'system_server')
                return original(name,args,deadline,absent_ok,record)
            app._command=command
            app.require_fresh(deadline)
            installed=True
            with self.assertRaises(ValueError):app.require_fresh(deadline)
            identity=app.prelaunch_identity(deadline)
            running=True
            with self.assertRaises(ValueError):app.prelaunch_identity(deadline)
            running=False
            app.initial_installation=identity['installation'];state['inode']+=1
            with self.assertRaisesRegex(RuntimeError,'prepared installation'):app.start(root/'first',deadline)
            self.assertEqual(state['effects'],[])

    def fixture(self, root, mode='normal'):
        product=root/'client.apk';product.write_bytes(b'frozen APK')
        spec=dict(product=str(product),apk_sha256=digest(product),package='org.example.client',activity='org.example.client.Main')
        app=AndroidApp('emulator-5580',spec,root/'app',{},Path('/sdk/adb'))
        state=dict(pid=None,launches=0,effects=[],forward=None,inode=42,starts=[],preparations=0)
        apk='/data/app/~~package==/base.apk'
        def command(name,args,deadline,absent_ok=False,record=True):
            if name=='startup-barrier':
                self.assertEqual(args,['shell','am','wait-for-broadcast-barrier','--flush-broadcast-loopers','--flush-application-threads'])
                self.assertIsNone(state['pid']);self.assertEqual(state['launches'],0)
                state['preparations']+=1
                if mode=='barrier-timeout':raise TimeoutError('Startup barrier deadline')
                if mode=='barrier-gave-up':return 'Gave up waiting for application barriers!'
                if mode=='barrier-no-proof':return 'All broadcast queues are idle!'
                if mode=='barrier-started-app':state['pid']=999
                if mode=='barrier-changed-data':state['inode']+=1
                return 'Finished application barriers!'
            if args==['shell','pm','path',app.package]:return 'package:'+apk
            if args==['shell','sha256sum',apk]:return spec['apk_sha256']+'  '+apk
            if args==['shell','run-as',app.package,'pwd']:return '/data/user/0/'+app.package
            if args==['shell','run-as',app.package,'stat','-c',"'%d %i'",'.']:return '1 '+str(state['inode'])
            if args==['shell','pidof',app.package]:return '' if state['pid'] is None else str(state['pid'])
            if args==['shell','cat','/proc/sys/kernel/random/boot_id']:return 'abcd0000-0000-0000-0000-000000000001'
            if args[:4]==['shell','run-as',app.package,'cat']:
                if args[-1].endswith('/cmdline'):return app.package+'\0'
                if args[-1].endswith('/stat'):return str(state['pid'])+' (client) S '+'0 '*18+str(100+state['launches'])
            if args[:4]==['shell','am','start','-W']:
                state['starts'].append(dict(args=list(args),previous_pid=state['pid']))
                state['effects'].append('start');state['launches']+=1;state['pid']=1000+state['launches']
                if mode=='lost-launch' and state['launches']>1:raise RuntimeError('Lost start reply')
                if mode=='foreign-route' and state['launches']>1:return 'Status: ok\nActivity: dev.foreign.app/.Main'
                if mode=='warning-timeout' and state['launches']>1:
                    return 'Warning: Activity not started\nStatus: timeout\nActivity: org.example.client/.Main'
                if mode in ('window-timeout','timeout-no-process') or (mode=='cold-timeout-no-process' and state['launches']>1):
                    if mode in ('timeout-no-process','cold-timeout-no-process'):state['pid']=None
                    return 'Status: timeout\nActivity: org.example.client/.Main'
                return 'Status: ok\nActivity: org.example.client/.Main'
            if args==['shell','am','force-stop',app.package]:
                state['effects'].append('stop');state['pid']=None
                if mode=='changed-data':state['inode']+=1
                if mode=='lost-stop':raise RuntimeError('Lost stop reply')
                return ''
            if args[:5]==['shell','run-as',app.package,'test','!']:return ''
            if args[:4]==['logcat','-d','-v','raw']:
                self.assertEqual(args[5],str(state['pid']))
                generation=str(1 if mode=='old-endpoint' else state['launches'])*48
                endpoint=dict(generation=generation,token='a'*48,port=20000+state['launches'],scenario='session')
                line='FRONTCAP_FLUTTER_ENDPOINT '+json.dumps(endpoint)
                if mode=='ambiguous-endpoint':
                    endpoint['generation']='f'*48
                    line+='\nFRONTCAP_FLUTTER_ENDPOINT '+json.dumps(endpoint)
                return line
            if args[:2]==['forward','tcp:0']:
                state['effects'].append('forward');host=30000+state['launches']
                state['forward']=['emulator-5580','tcp:'+str(host),args[2]]
                if mode=='lost-forward':raise RuntimeError('Lost allocated port reply')
                return str(host)
            if args==['forward','--list']:return ' '.join(state['forward']) if state['forward'] else ''
            if args[:2]==['forward','--remove']:
                self.assertIsNotNone(state['forward']);self.assertEqual(args[2],state['forward'][1])
                state['effects'].append('remove');state['forward']=None
                if mode=='lost-remove':raise RuntimeError('Lost remove reply')
                return ''
            raise AssertionError((name,args))
        app._command=command
        return app,state

    def test_initial_start_requires_completed_preparation_and_unchanged_absent_app(self):
        for mode in ('barrier-timeout','barrier-gave-up','barrier-no-proof','barrier-started-app','barrier-changed-data'):
            with self.subTest(mode=mode),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);app,state=self.fixture(root,mode)
                with self.assertRaises((RuntimeError,TimeoutError)):
                    app.start(root/'first',time.monotonic()+5)
                self.assertEqual(state['effects'],[]);self.assertEqual(state['launches'],0)
                self.assertIsNone(app.owner);self.assertIsNone(app.forwarded)
                self.assertFalse((root/'first').exists())

    def test_ambiguous_endpoint_retains_bounded_diagnostics_without_adoption_or_retry(self):
        for mode in ('readable','read-failed','report-failed'):
            with self.subTest(mode=mode),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);app,state=self.fixture(root,'ambiguous-endpoint')
                reads=[]
                def diagnostic(argv, output, **kwargs):
                    self.assertEqual(argv[:3],['/sdk/adb','-s',app.serial])
                    self.assertTrue(argv[3]=='logcat' or argv[3:6]==['shell','dumpsys','activity'])
                    self.assertGreater(kwargs['timeout'],0);self.assertLessEqual(kwargs['timeout'],3)
                    self.assertGreater(kwargs['output_limit'],0)
                    self.assertLessEqual(kwargs['output_limit'],2*1024*1024)
                    reads.append(argv)
                    if mode=='read-failed':raise OSError('diagnostic read failed')
                    return capture([sys.executable,'-c','print("lifecycle evidence")'],output,**kwargs)
                from flutter_android_app import write_json
                def persist(path,value):
                    if mode=='report-failed' and path.name=='endpoint-diagnostics.json':
                        raise OSError('diagnostic report failed')
                    write_json(path,value)
                with patch('flutter_android_app.capture_command',side_effect=diagnostic),patch('flutter_android_app.write_json',side_effect=persist):
                    with self.assertRaisesRegex(ValueError,'Multiple Flutter endpoint identities'):
                        app.start(root/'first',time.monotonic()+5)
                self.assertIsNone(app.owner);self.assertIsNone(app.forwarded);self.assertTrue(app.uncertain)
                with self.assertRaises(RuntimeError):app.start(root/'retry',time.monotonic()+5)
                self.assertEqual(state['effects'],['start'])
                self.assertTrue(reads)
                if mode!='report-failed':
                    report=json.loads((root/'first/endpoint-diagnostics.json').read_text())
                    self.assertEqual(report['context']['process']['pid'],state['pid'])
                    if mode=='read-failed':self.assertIn('diagnostic read failed',report['error'])
                    else:
                        self.assertTrue(all(row['result']['child_retired'] for row in report['reads']))
                        self.assertTrue(any('--pid' in argv and str(state['pid']) in argv for argv in reads))

    def test_host_visibility_preserves_process_endpoint_and_fences_uncertain_effect(self):
        for mode in ('normal','lost-effect','replaced-process'):
            with self.subTest(mode=mode),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);app,state=self.fixture(root);deadline=time.monotonic()+5
                original=app.start(root/'first',deadline);command=app._command
                focus=[app.package+'/'+app.activity];effects=[]
                def host(name,args,budget,absent_ok=False,record=True):
                    if args==['shell','dumpsys','window']:return 'mCurrentFocus=Window{1 u0 '+focus[0]+'}'
                    if name.startswith('host-'):
                        effects.append(name)
                        focus[0]='dev.launcher/dev.launcher.Home' if name=='host-background' else app.package+'/'+app.activity
                        if mode=='lost-effect':raise RuntimeError('effect reply lost')
                        if mode=='replaced-process':state['pid']+=1
                        return ''
                    return command(name,args,budget,absent_ok,record)
                app._command=host
                if mode=='normal':
                    home=app.set_app_state(root/'home','background',deadline)
                    returned=app.set_app_state(root/'return','foreground',deadline)
                    self.assertEqual(home['before'],original);self.assertEqual(returned['after'],original)
                    self.assertEqual(returned['native']['focused_after'],app.package+'/'+app.activity)
                    self.assertEqual(effects,['host-background','host-foreground'])
                else:
                    with self.assertRaises(RuntimeError):app.set_app_state(root/'home','background',deadline)
                    with self.assertRaises(RuntimeError):app.set_app_state(root/'repeat','background',deadline)
                    self.assertEqual(effects,['host-background'])

    def test_restart_keeps_installation_and_replaces_process_endpoint_and_forwarding(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);app,state=self.fixture(root);deadline=time.monotonic()+5
            saved=root/'state.json';saved.write_bytes(b'committed state')
            first=app.start(root/'first',deadline);app.terminate(root/'stop',deadline)
            second=app.start(root/'second',deadline)
            self.assertEqual(state['preparations'],1)
            self.assertEqual(first['installation'],second['installation'])
            for key in ['process','endpoint','forwarding']:self.assertNotEqual(first[key],second[key])
            self.assertEqual(saved.read_bytes(),b'committed state')
            self.assertEqual(state['effects'],['start','forward','remove','stop','start','forward'])
            app.retire_forwarding(deadline);self.assertIsNone(state['forward'])

    def test_foreign_focus_keeps_rejection_and_bounded_diagnostics_without_host_input(self):
        for mode in ('readable','command-failed','reader-unavailable'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root=Path(directory);app,state=self.fixture(root);deadline=time.monotonic()+5
                original=app.start(root/'first',deadline);command=app._command
                window='mCurrentFocus=Window{1 u0 Application Not Responding: com.android.systemui}'
                def host(name,args,budget,absent_ok=False,record=True):
                    if args==['shell','dumpsys','window']:return window
                    return command(name,args,budget,absent_ok,record)
                app._command=host;app.env={'FRONTCAP_DIAGNOSTIC_CONTEXT':'owned-reader'}
                limits=[]
                def diagnostic(argv, output, **kwargs):
                    self.assertEqual(argv[:3],['/sdk/adb','-s','emulator-5580'])
                    self.assertIn(argv[3:], [
                        ['shell','dumpsys','activity','lastanr'],
                        ['logcat','-b','events','-d','-v','epoch','am_anr:I','am_crash:I','*:S'],
                    ])
                    self.assertGreater(kwargs['timeout'],0);self.assertLessEqual(kwargs['timeout'],3)
                    limits.append(kwargs['output_limit'])
                    if mode=='reader-unavailable':raise OSError('reader unavailable')
                    code='import os,sys; print(os.environ["FRONTCAP_DIAGNOSTIC_CONTEXT"]); sys.exit('+('7' if mode=='command-failed' else '0')+')'
                    return capture([sys.executable,'-c',code],output,**kwargs)
                before=list(state['effects'])
                with patch('flutter_android_app.capture_command',side_effect=diagnostic):
                    with self.assertRaisesRegex(RuntimeError,'Flutter host initial focus differs'):
                        app.set_app_state(root/'home','background',deadline)
                self.assertEqual(state['effects'],before);self.assertEqual(app.owner,original)
                self.assertFalse(app.uncertain)
                self.assertEqual((root/'home/window-before.txt').read_text(),window)
                report=json.loads((root/'home/focus-diagnostics.json').read_text())
                self.assertEqual(limits[0],2*1024*1024)
                if mode=='reader-unavailable':
                    self.assertIn('reader unavailable',report['error'])
                else:
                    self.assertLess(limits[1],limits[0])
                    for row in report['reads']:
                        self.assertTrue(row['result']['child_retired'])
                        self.assertEqual(row['result']['exit_code'],7 if mode=='command-failed' else 0)
                        self.assertEqual((root/'home'/row['name']/'stdout').read_text(),'owned-reader\n')

    def test_uncertain_effect_or_changed_identity_never_replays_app_replacement(self):
        for mode in ['lost-launch','lost-stop','changed-data','old-endpoint','lost-forward','lost-remove']:
            with self.subTest(mode=mode),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);app,state=self.fixture(root,mode);deadline=time.monotonic()+5
                with self.assertRaises((RuntimeError,ValueError)):
                    app.start(root/'first',deadline);app.terminate(root/'stop',deadline);app.start(root/'second',deadline)
                effects=list(state['effects'])
                for method in [app.start,app.terminate]:
                    with self.assertRaises(RuntimeError):method(root/'retry',deadline)
                self.assertEqual(state['effects'],effects)
                if mode=='lost-forward':
                    with self.assertRaises(RuntimeError):app.retire_forwarding(deadline)
                elif mode=='lost-remove':
                    app.retire_forwarding(deadline)
                    self.assertEqual(state['effects'],effects) # Lookup resolves absence; no second remove.

    def test_foreign_forwarding_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);app,state=self.fixture(root);deadline=time.monotonic()+5
            app.start(root/'first',deadline);state['forward'][0]='emulator-5582'
            effects=list(state['effects'])
            with self.assertRaises(ValueError):app.retire_forwarding(deadline)
            self.assertEqual(state['effects'],effects);self.assertEqual(state['forward'][0],'emulator-5582')

    def test_cold_url_uses_one_literal_intent_after_confirmed_process_absence(self):
        import shlex
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);app,state=self.fixture(root);deadline=time.monotonic()+5
            first=app.start(root/'first',deadline)
            for url in ('relative','client-test://with space','client-test://x\n',''):
                with self.assertRaises(ValueError):app.entry_request(url,first['endpoint']['generation'])
            self.assertEqual(state['effects'],['start','forward'])
            app.terminate(root/'stop',deadline)
            url="client-test://item/1?text='$(literal)'&x=1#fragment"
            second=app.start(root/'cold',deadline,url=url)
            launch=state['starts'][-1]
            self.assertIsNone(launch['previous_pid'])
            self.assertEqual(launch['args'],['shell','am','start','-W','-a','android.intent.action.VIEW','-d',shlex.quote(url),'-p',app.package])
            accepted=json.loads((root/'cold/entry.json').read_text())
            self.assertEqual(accepted,dict(request=app.entry_request(url,first['endpoint']['generation']),confirmation='unobserved',destination='unobserved'))
            self.assertEqual(first['installation'],second['installation'])
            self.assertNotEqual(first['process'],second['process'])
            self.assertEqual(state['effects'],['start','forward','remove','stop','start','forward'])

    def test_uncertain_or_foreign_cold_delivery_cannot_fall_back_to_normal_launch(self):
        for mode in ('lost-launch','foreign-route','warning-timeout'):
            with self.subTest(mode=mode),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);app,state=self.fixture(root,mode);deadline=time.monotonic()+5
                app.start(root/'first',deadline);app.terminate(root/'stop',deadline)
                with self.assertRaises(RuntimeError):app.start(root/'cold',deadline,url='client-test://item/1')
                before=list(state['effects'])
                with self.assertRaises(RuntimeError):app.start(root/'fallback',deadline)
                with self.assertRaises(RuntimeError):app.start(root/'retry',deadline,url='client-test://item/1')
                self.assertEqual(state['effects'],before)
                self.assertFalse((root/'cold/ready.json').exists())

    def test_window_wait_timeout_preserves_acceptance_and_requires_original_live_readiness(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);app,state=self.fixture(root,'window-timeout');deadline=time.monotonic()+5
            first=app.start(root/'first',deadline)
            self.assertEqual(first['process']['pid'],1001)
            self.assertEqual(state['effects'],['start','forward'])
            self.assertEqual(json.loads((root/'first/launch-status.json').read_text())['status'],'timeout')
            app.terminate(root/'stop',deadline)
            second=app.start(root/'cold',deadline,url='client-test://item/1')
            self.assertEqual(second['installation'],first['installation'])
            self.assertNotEqual(second['process'],first['process'])
            self.assertNotEqual(second['endpoint']['generation'],first['endpoint']['generation'])
            accepted=json.loads((root/'cold/entry.json').read_text())
            self.assertEqual(accepted['request'],app.entry_request('client-test://item/1',first['endpoint']['generation']))
            self.assertEqual(accepted['destination'],'unobserved')
            self.assertEqual(json.loads((root/'cold/launch-status.json').read_text())['status'],'timeout')
            self.assertEqual(state['effects'],['start','forward','remove','stop','start','forward'])
            before=list(state['effects'])
            with self.assertRaises(RuntimeError):app.start(root/'retry',deadline)
            self.assertEqual(state['effects'],before)
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);app,state=self.fixture(root,'timeout-no-process');deadline=time.monotonic()+5
            with self.assertRaises(RuntimeError):app.start(root/'first',deadline)
            with self.assertRaises(RuntimeError):app.start(root/'retry',deadline)
            self.assertEqual(state['effects'],['start'])
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);app,state=self.fixture(root,'cold-timeout-no-process');deadline=time.monotonic()+5
            app.start(root/'first',deadline);app.terminate(root/'stop',deadline)
            with self.assertRaises(RuntimeError):app.start(root/'cold',deadline,url='client-test://item/1')
            self.assertTrue((root/'cold/entry.json').exists())
            self.assertFalse((root/'cold/ready.json').exists())
            before=list(state['effects'])
            with self.assertRaises(RuntimeError):app.start(root/'retry',deadline,url='client-test://item/1')
            self.assertEqual(state['effects'],before)
