"""App binding and literal host delivery, without acquiring a device."""
import hashlib
import json
from pathlib import Path
import plistlib
import shlex
import struct
import tempfile
import unittest
from flutter_host_commands import host_arguments, installed_target


class HostCommandsTests(unittest.TestCase):
    def test_url_is_bound_to_owner_and_literal_shell_argument(self):
        for platform in ('android', 'ios'):
            target = (dict(platform=platform, serial='emulator-5580', package='org.example.client')
                      if platform == 'android' else dict(platform=platform, simulator='abcd0000-0000-0000-0000-000000000001', bundle='org.example.client'))
            ready = dict(owner=dict(generation='a'*48), external_url_target=target)
            url = "client-test://item/1?title=a'b&other=$x#fragment"
            request = dict(target=target, generation='a'*48, url=url)
            args = host_arguments(dict(operation='open_url', request=request), ready, Path('/sdk/adb'))
            if platform == 'android':
                self.assertEqual(shlex.split(' '.join(args[3:])), ['shell', 'am', 'start', '-W', '-a',
                    'android.intent.action.VIEW', '-d', url, '-p', 'org.example.client'])
            else:
                self.assertEqual(args[-1], url)
            for change in (dict(generation='c'*48), dict(url='relative'), dict(target={})):
                with self.assertRaises(ValueError):
                    host_arguments(dict(operation='open_url', request=dict(request, **change)), ready, Path('/sdk/adb'))

    def test_android_app_requires_matching_installed_apk(self):
        with tempfile.TemporaryDirectory() as folder:
            project = Path(folder)
            root = project/'build/app/outputs/apk/debug';root.mkdir(parents=True)
            (root/'app-debug.apk').write_bytes(b'fixture APK')
            (root/'output-metadata.json').write_text(json.dumps(dict(applicationId='org.example.client',
                artifactType=dict(type='APK'), elements=[dict(outputFile='app-debug.apk', filters=[])])))
            expected = hashlib.sha256(b'fixture APK').hexdigest()
            def command(name, argv):
                return b'package:/data/app/example/base.apk\n' if name == 'package-path' else (expected+'  /data/app/example/base.apk\n').encode()
            target, record = installed_target('android', project, 'emulator-5580', Path('/sdk/adb'), command)
            self.assertEqual(target['package'], 'org.example.client')
            self.assertEqual(record['sha256'], expected)
            expected = '0'*64
            with self.assertRaises(ValueError):
                installed_target('android', project, 'emulator-5580', Path('/sdk/adb'), command)

    def test_ios_app_requires_matching_dart_code_assets_and_executable(self):
        with tempfile.TemporaryDirectory() as folder:
            project = Path(folder)
            product = project/'build/ios/iphonesimulator/Runner.app';product.mkdir(parents=True)
            installed = project/'installed.app';installed.mkdir()
            info = dict(CFBundleIdentifier='org.example.client', CFBundleExecutable='Runner')
            for directory in (product, installed):
                (directory/'Info.plist').write_bytes(plistlib.dumps(info, fmt=plistlib.FMT_BINARY))
                (directory/'Runner').write_bytes(b'fixture executable')
                (directory/'Runner').chmod(0o755)
                (directory/'Frameworks/App.framework/flutter_assets').mkdir(parents=True)
                (directory/'Frameworks/App.framework/flutter_assets/kernel_blob.bin').write_bytes(b'compiled Dart')
                (directory/'Frameworks/App.framework/App').write_bytes(struct.pack('<IIII', 0xfeedfacf, 0x100000c, 0, 6))
            (product/'Frameworks/App.framework/App').chmod(0o755)
            (installed/'Frameworks/App.framework/App').chmod(0o644)
            command = lambda name, argv: (str(installed)+'\n').encode()
            target, evidence = installed_target('ios', project, 'ABCD0000-0000-0000-0000-000000000001', None, command)
            self.assertEqual(target['bundle'], info['CFBundleIdentifier'])
            self.assertEqual(set(evidence['installed_library_modes']), {'Frameworks/App.framework/App'})
            for name in ['Runner', 'Frameworks/App.framework/flutter_assets/kernel_blob.bin']:
                with self.subTest(name=name):
                    original = (installed/name).read_bytes()
                    (installed/name).write_bytes(b'other')
                    with self.assertRaises(ValueError):
                        installed_target('ios', project, target['simulator'], None, command)
                    (installed/name).write_bytes(original)
            (installed/'Runner').chmod(0o644)
            with self.assertRaises(ValueError): installed_target('ios', project, target['simulator'], None, command)
            (installed/'Runner').chmod(0o755)
            (installed/'unexpected').touch()
            with self.assertRaises(ValueError): installed_target('ios', project, target['simulator'], None, command)


if __name__ == '__main__':
    unittest.main()
