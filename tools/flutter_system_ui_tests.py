"""Frozen runner inputs and owner-bound termination, without acquiring a device."""
import json
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

from flutter_system_ui import SystemRunner, digest, validate_products, validate_runner_process


class SystemRunnerTests(unittest.TestCase):
    def test_runner_identity_uses_actual_simulator_container_after_relocation(self):
        installed = Path('/owned/mount/device-data/Containers/Bundle/Application/app/SystemUITests-Runner.app')
        prefix = '123 Wed Nov 4 20:00:00 2026 '
        validate_runner_process(prefix+str(installed/'SystemUITests-Runner')+' -argument', 123, installed)
        for process in [prefix.replace('123', '124', 1)+str(installed/'SystemUITests-Runner'),
                        prefix+'/foreign/SystemUITests-Runner.app/SystemUITests-Runner',
                        prefix+str(installed/'SystemUITests-Runner')+'-foreign']:
            with self.assertRaises(ValueError):
                validate_runner_process(process, 123, installed)

    def test_changed_products_are_rejected_before_runner_start(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            names = ['SystemUI.xctestrun',
                     'Debug-iphonesimulator/SystemUITests-Runner.app/PlugIns/SystemUITests.xctest/SystemUITests']
            files = {}
            for name in names:
                path = root/name;path.parent.mkdir(parents=True, exist_ok=True);path.write_bytes(b'fixture')
                path.chmod(0o644)
                files[name] = dict(bytes=7, sha256=digest(path), executable=False)
            config = dict(xcodebuild='/usr/bin/xcodebuild', products=str(root), manifest=dict(
                role='external_runner', names=dict(app='SystemUI', tests='SystemUITests'), xctestrun=names[0],
                files=files, directories=[str(p.relative_to(root)) for p in root.rglob('*') if p.is_dir()]))
            validate_products(config)
            path = root/names[1]
            for mutation in ['bytes', 'mode', 'extra', 'symlink']:
                with self.subTest(mutation=mutation):
                    if mutation == 'bytes': path.write_bytes(b'changed')
                    elif mutation == 'mode': path.chmod(0o755)
                    elif mutation == 'extra': (root/'extra').touch()
                    else: path.unlink();path.symlink_to(root/names[0])
                    with self.assertRaises(ValueError): validate_products(config)
                    if mutation == 'extra': (root/'extra').unlink()
                    if path.is_symlink(): path.unlink()
                    path.write_bytes(b'fixture');path.chmod(0o644)

    def test_owner_finish_rejects_uncertain_input_and_replaced_runner_before_sending(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = SystemRunner({}, 'simulator', root/'system', {}, 'a'*64, None)
            runner.process = Mock();runner.process.poll.return_value = None
            runner.identity = dict(generation='abcd0000-0000-0000-0000-000000000001', pid=123)
            runner.descriptor = root/'endpoint.json'
            runner.descriptor.write_text(json.dumps(dict(runner.identity, pid=124, status='ready', last_id=0)))
            ui = runner.output/'client/ui';ui.mkdir(parents=True)
            for pending in [{}, None]:
                (ui/'ui-client.json').write_text(json.dumps(dict(generation=runner.identity['generation'], next_id=1, pending=pending)))
                with patch('flutter_system_ui.exchange') as transport:
                    with self.assertRaises(RuntimeError): runner.finish(time.monotonic()+5)
                    transport.assert_not_called()
