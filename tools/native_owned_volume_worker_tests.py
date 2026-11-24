"""Private-pipe authority checks; actual hdiutil lifecycle belongs to the opt-in host probe."""
import io
import errno
import hashlib
import json
from pathlib import Path
import tempfile
import sys
import unittest
from unittest.mock import patch

from native_owned_volume_worker import run, BusyDetach


class Owner:
    calls = []

    @classmethod
    def create(cls, directory, size_mib, command):
        cls.calls.append('create')
        owner = cls()
        owner.image = directory / 'image'
        owner.mount = directory / 'mount'
        return owner

    def attach(self):
        self.calls.append('attach')
        return self.mount

    def reserve_retirement(self, size):
        self.calls.append('reserve')

    def release_retirement(self, *, writers_retired):
        assert writers_retired is True
        self.calls.append('release')

    def detach(self, *, writers_retired):
        assert writers_retired is True
        self.calls.append('detach')

    def retained_image(self):
        self.calls.append('hash')
        return dict(bytes=256 * 1024 * 1024, sha256='a' * 64)

    def compress_retained_image(self):
        self.calls.append('compress')
        return dict(bytes=256 * 1024 * 1024, sha256='a' * 64, gzip=dict(bytes=1024, sha256='b' * 64))

    def remove(self):
        self.calls.append('remove')


class VolumeWorkerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name) / 'worker'
        Owner.calls = []

    def execute(self, data):
        output = io.BytesIO()
        status = run(self.directory, 256, 8, io.BytesIO(data), output, owner_type=Owner)
        return status, [json.loads(line) for line in output.getvalue().splitlines()]

    def test_control_reservation_failure_prevents_target_creation(self):
        with patch('native_owned_volume_worker.reserve_control', side_effect=OSError('No control space')):
            status, replies = self.execute(b'1')
        self.assertEqual(status, 1)
        self.assertEqual(replies, [])
        self.assertEqual(Owner.calls, [])
        state = json.loads((self.directory/'state.json').read_text())
        self.assertFalse(state['live_owner_acquired'])
        self.assertIn('No control space', state['error'])

    def test_watch_capacity_refusal_keeps_original_cleanup_binding(self):
        from native_owned_volume_worker import save
        for acquisition in (False, True):
            operation = 'watch_uikit_acquisition' if acquisition else 'watch_uikit'
            record = 'watch-acquisition.json' if acquisition else 'watch-uikit.json'
            ready = 'watch-acquisition-ready.json' if acquisition else 'watch-ready.json'
            rejected = 'watch-acquisition-rejected.json' if acquisition else 'watch-rejected.json'
            register = 'register_acquisition' if acquisition else 'register'
            wait = 'wait_for_acquisition_retirement' if acquisition else 'wait_for_retirement'
            for fail_at in (record, ready):
                for proven in (False, True):
                    with self.subTest(acquisition=acquisition, fail_at=fail_at, proven=proven):
                        self.directory = Path(self.temp.name)/f'{operation}-{fail_at}-{proven}'
                        Owner.calls = []
                        request = dict(operation=operation, session='/owned/original', worker={})
                        line = json.dumps(request).encode()+b'\n'
                        binding = dict(original='validated owner')
                        def publish(path, value):
                            if path.name == fail_at:
                                raise OSError(errno.ENOSPC, 'Control allocation refused')
                            save(path, value)
                        with patch('native_owned_volume_worker.save', publish), patch(
                                'native_owned_volume_worker.native_volume_uikit.'+register, return_value=binding), patch(
                                'native_owned_volume_worker.native_volume_uikit.'+wait,
                                side_effect=None if proven else TimeoutError('writer remains live'),
                                return_value={'passed': True}) as waited:
                            status, replies = self.execute(b'1'+line)
                        self.assertEqual(status, 0 if proven else 1)
                        self.assertFalse((self.directory/ready).exists())
                        refusal = json.loads((self.directory/rejected).read_text())
                        self.assertEqual(refusal['request_sha256'], hashlib.sha256(line).hexdigest())
                        self.assertIn('Control allocation refused', refusal['error'])
                        self.assertEqual(waited.call_args.args[0], binding)
                        self.assertEqual('detach' in Owner.calls, proven)
                        self.assertEqual('release' in Owner.calls, proven)
                        self.assertEqual(len(replies), 2 if proven else 1)

    def test_registered_owner_loss_requires_writer_proof_and_preserves_normal_stop(self):
        request = dict(operation='watch_uikit', session='/owned/g-test', worker={}, ready_sha256='a'*64)
        watch = json.dumps(request).encode()+b'\n'
        for mode in ['eof', 'normal', 'unproven']:
            with self.subTest(mode=mode):
                self.directory = Path(self.temp.name)/('worker-'+mode);Owner.calls=[]
                suffix = b'{"operation":"retire","writers_retired":true,"remove":false,"compression":"gzip"}\n' if mode=='normal' else b''
                with patch('native_owned_volume_worker.native_volume_uikit.register', return_value={'registered': True}), patch(
                        'native_owned_volume_worker.native_volume_uikit.wait_for_retirement',
                        side_effect=RuntimeError('unretired target') if mode=='unproven' else None,
                        return_value={'passed': True}) as waited:
                    status, _ = self.execute(b'1'+watch+suffix)
                self.assertEqual(status, 1 if mode=='unproven' else 0)
                self.assertEqual(waited.call_count, 0 if mode=='normal' else 1)
                self.assertEqual('detach' in Owner.calls, mode!='unproven')
                self.assertEqual((self.directory/'owner-loss-writers-retired.json').exists(), mode=='eof')
                self.assertTrue((self.directory/'watch-ready.json').is_file())

    def test_explicit_group_retirement_waits_for_registered_diagnostic_writers(self):
        request=dict(operation='watch_uikit',session='/owned/g-test',worker={},ready_sha256='a'*64,
                     retirement_reader='/original/reader')
        message=b'1'+json.dumps(request).encode()+b'\n'+b'{"operation":"retire","writers_retired":true,"remove":false}\n'
        for ready in [False,True]:
            self.directory=Path(self.temp.name)/str(ready);Owner.calls=[]
            with patch('native_owned_volume_worker.native_volume_uikit.register',return_value=dict(registration=request)), patch(
                    'native_owned_volume_worker.native_volume_uikit.wait_for_retirement',
                    side_effect=None if ready else TimeoutError('group logger remains live'),return_value=dict(passed=True)):
                status,_=self.execute(message)
            self.assertEqual(status,0 if ready else 1)
            self.assertEqual('detach' in Owner.calls,ready)
            self.assertEqual('release' in Owner.calls,ready)

    def test_rejected_watch_preserves_only_explicit_live_retirement(self):
        request = dict(operation='watch_uikit', session='/owned/g-test', worker={}, ready_sha256='a'*64)
        watch = json.dumps(request).encode()+b'\n'
        for mode in ['normal', 'eof', 'unproven']:
            with self.subTest(mode=mode):
                self.directory = Path(self.temp.name)/('rejected-'+mode);Owner.calls=[]
                suffix = b'' if mode=='eof' else json.dumps(dict(operation='retire',
                    writers_retired=mode=='normal', remove=False, compression='gzip')).encode()+b'\n'
                with patch('native_owned_volume_worker.native_volume_uikit.register', side_effect=ValueError('worker exited')), patch(
                        'native_owned_volume_worker.native_volume_uikit.wait_for_retirement') as waited:
                    status, _ = self.execute(b'1'+watch+suffix)
                self.assertEqual(status, 0 if mode=='normal' else 1)
                self.assertEqual('detach' in Owner.calls, mode=='normal')
                self.assertEqual('compress' in Owner.calls, mode=='normal')
                waited.assert_not_called()
                rejected = json.loads((self.directory/'watch-rejected.json').read_text())
                self.assertEqual(rejected['request_sha256'], hashlib.sha256(watch).hexdigest())
                self.assertIn('worker exited', rejected['error'])
                for name in ['watch-ready.json', 'watch-uikit.json', 'owner-loss-writers-retired.json']:
                    self.assertFalse((self.directory/name).exists())

    def test_acquisition_watch_eof_and_upgrade_keep_original_authority(self):
        request=dict(operation='watch_uikit_acquisition',session='/owned/g-test',worker={})
        early=json.dumps(request).encode()+b'\n'
        binding=dict(registration=request,diagnostic='original',diagnostic_process={},diagnostic_config_sha256='a'*64)
        ready=dict(request,operation='watch_uikit',ready_sha256='b'*64)
        for mode in ['eof','upgrade','foreign','unproven']:
            with self.subTest(mode=mode):
                self.directory=Path(self.temp.name)/('acquisition-'+mode);Owner.calls=[]
                normal=dict(binding,registration=ready)
                if mode=='foreign':normal['diagnostic']='foreign'
                suffix=json.dumps(ready).encode()+b'\n' if mode in ['upgrade','foreign'] else b''
                with patch('native_owned_volume_worker.native_volume_uikit.register_acquisition',return_value=binding), patch(
                        'native_owned_volume_worker.native_volume_uikit.register',return_value=normal), patch(
                        'native_owned_volume_worker.native_volume_uikit.wait_for_acquisition_retirement',
                        side_effect=ValueError('unproven') if mode=='unproven' else None,return_value={'original':True}) as early_wait, patch(
                        'native_owned_volume_worker.native_volume_uikit.wait_for_retirement',return_value={'original':True}) as ready_wait:
                    status,_=self.execute(b'1'+early+suffix)
                self.assertEqual(status,1 if mode=='unproven' else 0)
                self.assertEqual(ready_wait.call_count,1 if mode=='upgrade' else 0)
                self.assertEqual(early_wait.call_count,0 if mode=='upgrade' else 1)
                self.assertEqual('detach' in Owner.calls,mode!='unproven')
                self.assertEqual((self.directory/'watch-ready.json').exists(),mode=='upgrade')
                self.assertEqual((self.directory/'watch-rejected.json').exists(),mode=='foreign')

    def test_volume_command_overflow_preserves_failure_without_detach(self):
        class NoisyOwner(Owner):
            @classmethod
            def create(cls, directory, size_mib, command):
                cls.calls.append('create')
                command([sys.executable, '-c',
                         'import os,time; os.write(1,b"partial reply"); '
                         'os.write(2,b"x"*(2*1024*1024)); time.sleep(30)'])
                raise AssertionError('overflow must not become a completed volume command')

        status = run(self.directory, 256, 8, io.BytesIO(b'1'), io.BytesIO(), owner_type=NoisyOwner)
        self.assertEqual(status, 1)
        self.assertEqual(Owner.calls, ['create'])
        self.assertFalse((self.directory / 'ready.json').exists())
        self.assertFalse((self.directory / 'retirement.json').exists())
        command = self.directory / 'command-001'
        report = json.loads((command / 'capture.json').read_text())
        self.assertIn('output limit', report['error'])
        self.assertTrue(report['child_retired'])
        self.assertLessEqual(sum((command / name).stat().st_size for name in ['stdout', 'stderr']), 1024 * 1024)
        self.assertIn('unresolved', json.loads((self.directory / 'state.json').read_text())['error'])

    def test_busy_detach_retries_original_owner_but_uncertain_failure_does_not(self):
        for busy in (True, False):
            self.directory = Path(self.temp.name) / str(busy)
            class DelayedOwner(Owner):
                attempts = 0
                def detach(self, *, writers_retired):
                    super().detach(writers_retired=writers_retired)
                    self.attempts += 1
                    if self.attempts == 1:
                        if busy:
                            raise BusyDetach('original detach returned EBUSY')
                        raise RuntimeError('detach outcome uncertain')
            data = b'1' + json.dumps(dict(operation='retire', writers_retired=True, remove=True)).encode() + b'\n'
            Owner.calls = []
            with patch('native_owned_volume_worker.time.sleep'):
                status = run(self.directory, 256, 8, io.BytesIO(data), io.BytesIO(), owner_type=DelayedOwner)
            self.assertEqual(status, 0 if busy else 1)
            self.assertEqual(Owner.calls.count('release'), 1)
            self.assertEqual(Owner.calls.count('detach'), 2 if busy else 1)
            self.assertEqual('remove' in Owner.calls, busy)

    def test_busy_deadline_preserves_original_failure_without_another_attempt(self):
        class BusyOwner(Owner):
            def detach(self, *, writers_retired):
                super().detach(writers_retired=writers_retired)
                raise BusyDetach('original detach returned EBUSY')
        data = b'1' + json.dumps(dict(operation='retire', writers_retired=True, remove=True)).encode() + b'\n'
        with patch('native_owned_volume_worker.time.monotonic', side_effect=[0, 0, 15]), \
                patch('native_owned_volume_worker.time.sleep'):
            status = run(self.directory, 256, 8, io.BytesIO(data), io.BytesIO(), owner_type=BusyOwner)
        self.assertEqual(status, 1)
        self.assertEqual(Owner.calls.count('detach'), 1)
        self.assertNotIn('remove', Owner.calls)
        self.assertIn('BusyDetach: original detach returned EBUSY',
                      json.loads((self.directory / 'state.json').read_text())['error'])
        self.assertFalse((self.directory / 'retirement.json').exists())

    def test_unauthorized_start_creates_nothing(self):
        with self.assertRaisesRegex(RuntimeError, 'not authorized'):
            self.execute(b'')
        self.assertFalse(self.directory.exists())
        self.assertEqual(Owner.calls, [])

    def test_eof_or_unproven_retirement_preserves_volume(self):
        for index, data in enumerate([b'1', b'1{"operation":"retire","writers_retired":false,"remove":true}\n',
                                     b'1' + b'x' * 4097]):
            with self.subTest(data=data[:80]):
                self.directory = Path(self.temp.name) / str(index)
                Owner.calls = []
                status, replies = self.execute(data)
                self.assertEqual(status, 1)
                self.assertEqual(len(replies), 1)
                self.assertEqual(Owner.calls, ['create', 'attach', 'reserve'])
                self.assertFalse((self.directory / 'retirement.json').exists())

    def test_compressed_retirement_preserves_decoded_identity(self):
        data = b'1' + json.dumps(dict(operation='retire', writers_retired=True,
                                     remove=False, compression='gzip')).encode() + b'\n'
        status, replies = self.execute(data)
        self.assertEqual(status, 0)
        self.assertEqual(Owner.calls, ['create', 'attach', 'reserve', 'release', 'detach', 'compress'])
        self.assertFalse(replies[-1]['removed'])
        self.assertEqual(replies[-1]['retained_image']['gzip']['bytes'], 1024)

    def test_explicit_retirement_detaches_and_honors_disposal_choice(self):
        for remove in [False, True]:
            with self.subTest(remove=remove):
                self.directory = Path(self.temp.name) / str(remove)
                Owner.calls = []
                data = b'1' + json.dumps(dict(operation='retire', writers_retired=True, remove=remove)).encode() + b'\n'
                status, replies = self.execute(data)
                self.assertEqual(status, 0)
                expected = dict(passed=True, detached=True, removed=remove)
                if not remove:
                    expected['retained_image'] = dict(bytes=256 * 1024 * 1024, sha256='a' * 64)
                self.assertEqual(replies[-1], expected)
                self.assertEqual(Owner.calls, ['create', 'attach', 'reserve', 'release', 'detach'] + (['remove'] if remove else ['hash']))
                with self.assertRaises(FileExistsError):
                    self.execute(data)


if __name__ == '__main__':
    unittest.main()
