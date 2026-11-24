"""Stale presentation may be observed again; input, launch and the deadline may not be repeated."""
import json
import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from native_android_capture import COMPONENT, observe, check_presentation


XML = b'<hierarchy><node package="dev.frontcap.s18.probe" content-desc="native-count-0" text="Count: 0" bounds="[0,0][4,4]"/><node package="dev.frontcap.s18.probe" content-desc="native-color" bounds="[0,0][4,4]"/></hierarchy>'


def presentation_evidence(root, name, count):
    captures = root/(name+'-captures'); captures.mkdir(exist_ok=True)
    files = {}
    for suffix, data in [('.xml', XML.replace(b'native-count-0', f'native-count-{count}'.encode()).replace(b'Count: 0', f'Count: {count}'.encode())),
                         ('.png', b'fixture image '+name.encode()),
                         ('-window.txt', (' mCurrentFocus=Window{'+COMPONENT+'}').encode()), ('-crash.txt', b'')]:
        path = captures/('attempt-000'+suffix); path.write_bytes(data)
        (root/(name+suffix)).write_bytes(data)
        files[suffix] = dict(path=path.relative_to(root).as_posix(), sha256=hashlib.sha256(data).hexdigest())
    pixel = [255,0,0] if count == 0 else [0,255,0]
    attempt = dict(index=0, started_s=0, finished_s=.1, files=files, point=[2,2], text=f'Count: {count}',
                   app_nodes=True, size=[1080,1920], pixel=pixel, focus=COMPONENT, matched=True)
    report = dict(status='ready', phase=name, count=count, component=COMPONENT, expected_pixel=pixel,
                  timeout_s=20, started_monotonic_s=100, attempts=[attempt], selected=0, finished_s=.1)
    (root/(name+'-presentation.json')).write_text(json.dumps(report))


class CaptureTests(unittest.TestCase):
    def acquisition(self, directory, *, stale=False, wrong_focus=False, late=False):
        elapsed, attempts, calls = [100.], [0], []
        def command(*args, timeout):
            calls.append((args, timeout)); elapsed[0] += 1
            if args[:3] == ('shell', 'uiautomator', 'dump'):
                attempts[0] += 1
                return b'dumped'
            if args[:2] == ('shell', 'cat'):return XML
            if args[:2] == ('exec-out', 'screencap'):
                return b'stale frame' if stale and attempts[0] == 1 else b'ready frame'
            if args[:3] == ('shell', 'dumpsys', 'window'):
                component = COMPONENT+'Other' if wrong_focus and attempts[0] == 1 else COMPONENT
                return (' mCurrentFocus=Window{'+component+'}').encode()
            if late:elapsed[0] += 20
            return b''
        def image(path, point):
            return [4, 4], [223, 227, 238] if path.read_bytes() == b'stale frame' else [255, 0, 0]
        with patch('native_android_capture.time.monotonic', side_effect=lambda:elapsed[0]), \
             patch('native_android_capture.time.sleep', side_effect=lambda delay:elapsed.__setitem__(0,elapsed[0]+delay)), \
             patch('native_android_capture.read_image', side_effect=image):
            result = observe(command, directory, 'app-before', 0)
        return result, calls

    def test_stale_screen_and_wrong_focus_need_fresh_read_only_attempts(self):
        for option in ('stale', 'wrong_focus'):
            with self.subTest(option=option), tempfile.TemporaryDirectory() as directory:
                (_, observation), calls = self.acquisition(directory, **{option:True})
                root = Path(directory); report=json.loads((root/'app-before-presentation.json').read_text())
                self.assertEqual([row['matched'] for row in report['attempts']], [False, True])
                self.assertEqual((root/'app-before.png').read_bytes(), b'ready frame')
                self.assertEqual(len(report['attempts']), 2)
                self.assertTrue(all(args[:2] in [('shell','uiautomator'), ('shell','cat'), ('exec-out','screencap'),
                                                ('shell','dumpsys'), ('logcat','-b')] for args,_ in calls))
                self.assertTrue(all(a[1] > b[1] > 0 for a,b in zip(calls,calls[1:])))
                checked=[]; check_presentation(root,observation,lambda name,valid,detail:checked.append(valid))
                self.assertEqual(checked,[True])

    def test_matching_screen_after_budget_stays_failed_and_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(TimeoutError,'deadline'):
                self.acquisition(directory,late=True)
            root=Path(directory); report=json.loads((root/'app-before-presentation.json').read_text())
            self.assertEqual(report['status'],'failed')
            self.assertTrue(report['attempts'][0]['matched'])
            self.assertGreater(report['attempts'][0]['finished_s'],20)
            self.assertTrue((root/'app-before-captures/attempt-000.png').exists())
            self.assertFalse((root/'app-before.png').exists())

    def test_acceptance_rejects_replaced_image_or_extended_budget(self):
        for damage in ('image','deadline','selection'):
            with self.subTest(damage=damage), tempfile.TemporaryDirectory() as directory:
                (_, observation), _ = self.acquisition(directory,stale=True)
                root=Path(directory); path=root/'app-before-presentation.json';report=json.loads(path.read_text())
                if damage == 'image':(root/'app-before.png').write_bytes(b'stale frame')
                elif damage == 'deadline':report['timeout_s']=40
                else:report['selected']=0
                path.write_text(json.dumps(report))
                checked=[];check_presentation(root,observation,lambda name,valid,detail:checked.append(valid))
                self.assertEqual(checked,[False])


if __name__ == '__main__':
    unittest.main()
