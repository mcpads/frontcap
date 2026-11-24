import json
from pathlib import Path
import struct
import tempfile
import time
import unittest
from unittest.mock import patch
import zlib
from native_android_view_capture import capture

GENERATION='abcd0000-0000-0000-0000-000000000001'
def png():
    def chunk(kind,body): return struct.pack('>I',len(body))+kind+body+struct.pack('>I',zlib.crc32(kind+body))
    return b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',1,1,8,2,0,0,0))+chunk(b'IDAT',zlib.compress(b'\0\1\2\3'))+chunk(b'IEND',b'')


class CaptureClock:
    """Advance capture's host deadline without depending on filesystem latency."""

    CLOCK_MONOTONIC = time.CLOCK_MONOTONIC

    def __init__(self):
        self.now = 0

    def monotonic(self):
        return self.now

    def clock_gettime_ns(self, clock):
        assert clock == self.CLOCK_MONOTONIC
        return round(self.now * 1e9)

    def sleep(self, seconds):
        self.now += seconds


class CaptureTests(unittest.TestCase):
    def fixture(self, *, changed=False, lost=False, unfocused=0):
        calls=[];state=dict(count=0,request=None)
        def command(*args,**options):
            calls.append(args)
            if args[:2]==('shell','pidof'):return b'123\n'
            if args[-1]=='files/clock-control.json':return json.dumps(dict(generation=GENERATION,pid=123,status='ready',last_id=0,now_ns=0)).encode()
            if args[:3]==('shell','content','call'):
                state['count']+=1;state['request']=next(arg.split(':',2)[2] for arg in args if arg.startswith('request:s:'))
                if lost:raise TimeoutError('submission reply lost')
                return b'Result: Bundle[{submitted=true}]\n'
            if args[-1]=='files/view-observation.json':
                return json.dumps(dict(state='observed',request=state['request'],generation=GENERATION,pid=123,now_ns=0,
                    catalog=dict(window_focused=state['count']>unfocused,ime_visible=False,targets=[dict(token=GENERATION,class_name='android.widget.Button',role='button',resource_id='',label='',
                        text='changed' if changed and state['count']==2 else 'original',secure=False,enabled=True,visible=True,
                        focused=False,clickable=True,selected=False,transformed=False,layout_bounds_screen_px=[0,0,1,1])]))).encode()
            if args==('shell','dumpsys','window'):return b'mCurrentFocus=Window{abc u0 dev.example/.Main}\n'
            if args==('exec-out','screencap','-p'):return png()
            raise AssertionError('Unexpected command: '+str(args))
        return command,calls
    def run_capture(self,command,root):
        return capture(command,root,package='dev.example',activity='dev.example.Main',generation=GENERATION,pid=123,source_sha256='a'*64)
    def test_capture_keeps_screen_and_both_source_brackets(self):
        with tempfile.TemporaryDirectory() as temporary:
            command,calls=self.fixture();root=Path(temporary)/'capture';result=self.run_capture(command,root)
            self.assertEqual(result['manifest']['catalog']['targets'][0]['text'],'original')
            self.assertEqual(result['manifest']['size'],[1,1])
            self.assertEqual(json.loads((root/'progress.json').read_text())['state'],'complete')
            self.assertEqual(len([c for c in calls if c[:3]==('shell','content','call')]),2)
            self.assertEqual((root/'screen.png').read_bytes(),png())
    def test_changed_source_is_unavailable_but_uncertain_submission_stays_partial(self):
        with tempfile.TemporaryDirectory() as temporary:
            command,calls=self.fixture(changed=True);root=Path(temporary)/'capture'
            result=self.run_capture(command,root)
            self.assertTrue(result['source_changed'])
            self.assertEqual(json.loads((root/'progress.json').read_text())['state'],'unavailable')
            self.assertTrue((root/'screen.png').exists())
            self.assertEqual(len([c for c in calls if c[:3]==('shell','content','call')]),2)
        with tempfile.TemporaryDirectory() as temporary:
            command,calls=self.fixture(lost=True);root=Path(temporary)/'capture'
            with self.assertRaises(TimeoutError):self.run_capture(command,root)
            self.assertFalse((root/'manifest.json').exists())
            self.assertEqual(json.loads((root/'progress.json').read_text())['state'],'partial')
            self.assertEqual(len([c for c in calls if c[:3]==('shell','content','call')]),1)

    def test_startup_focus_wait_keeps_probes_and_never_advances_or_sends_input(self):
        with tempfile.TemporaryDirectory() as temporary:
            command,calls=self.fixture(unfocused=2);root=Path(temporary)/'capture'
            result=self.run_capture(command,root)
            self.assertFalse(result['source_changed'])
            self.assertEqual(result['manifest']['now_ns'],0)
            self.assertTrue(result['manifest']['catalog']['window_focused'])
            probes=[json.loads(path.read_text()) for path in sorted(root.glob('focus-probe-?.json'))]
            self.assertEqual([row['catalog']['window_focused'] for row in probes],[False,False,True])
            submissions=[call for call in calls if call[:3]==('shell','content','call')]
            self.assertEqual(len(submissions),4)
            self.assertTrue(all(call[call.index('--method')+1]=='observe' for call in submissions))
            self.assertFalse(any('input' in call or 'advance' in call for call in calls))

    def test_focus_never_arriving_expires_without_a_screen_or_manifest(self):
        with tempfile.TemporaryDirectory() as temporary:
            command,calls=self.fixture(unfocused=1000);root=Path(temporary)/'capture'
            clock=CaptureClock()
            # The first observation completes; its readiness wait consumes the
            # remaining host budget. Real fsync latency is not this test's clock.
            with patch('native_android_view_capture.time',clock), self.assertRaises(TimeoutError):
                capture(command,root,package='dev.example',activity='dev.example.Main',
                    generation=GENERATION,pid=123,source_sha256='a'*64,timeout=.01)
            self.assertFalse((root/'manifest.json').exists())
            self.assertFalse((root/'screen.png').exists())
            probe=json.loads((root/'focus-probe-0.json').read_text())
            self.assertFalse(probe['catalog']['window_focused'])
            self.assertEqual(probe['now_ns'],0)
            self.assertEqual(json.loads((root/'progress.json').read_text())['state'],'partial')
            submissions=[call for call in calls if call[:3]==('shell','content','call')]
            self.assertEqual(len(submissions),1)
            self.assertEqual(submissions[0][submissions[0].index('--method')+1],'observe')
            self.assertFalse(any('input' in call or 'advance' in call for call in calls))

    def test_owner_query_expiry_prevents_the_first_focus_observation(self):
        with tempfile.TemporaryDirectory() as temporary:
            command,calls=self.fixture();root=Path(temporary)/'capture'
            clock=CaptureClock()
            def late_owner(*args,**options):
                reply=command(*args,**options)
                if args[:2]==('shell','pidof'):
                    clock.sleep(.02)
                return reply
            with patch('native_android_view_capture.time',clock), self.assertRaises(TimeoutError):
                capture(late_owner,root,package='dev.example',activity='dev.example.Main',
                    generation=GENERATION,pid=123,source_sha256='a'*64,timeout=.01)
            self.assertFalse((root/'manifest.json').exists())
            self.assertFalse((root/'screen.png').exists())
            self.assertFalse((root/'focus-probe-0.json').exists())
            progress=json.loads((root/'progress.json').read_text())
            self.assertEqual((progress['state'],progress['phase']),('partial','owner-before'))
            self.assertFalse(any(call[:3]==('shell','content','call') for call in calls))
            self.assertFalse(any('input' in call or 'advance' in call for call in calls))

    def test_truncated_screen_retains_partial_evidence_without_publishing_a_manifest(self):
        with tempfile.TemporaryDirectory() as temporary:
            command,_=self.fixture();root=Path(temporary)/'partial'
            truncated=png()[:len(png())//2]
            def interrupted(*args,**options):
                if args==('exec-out','screencap','-p'):return truncated
                return command(*args,**options)
            with self.assertRaises((ValueError,struct.error)):self.run_capture(interrupted,root)
            self.assertEqual((root/'screen.png').read_bytes(),truncated)
            self.assertTrue((root/'views-before.json').is_file())
            self.assertFalse((root/'manifest.json').exists())
            self.assertFalse((root/'views-after.json').exists())
            progress=json.loads((root/'progress.json').read_text())
            self.assertEqual((progress['state'],progress['phase']),('partial','screen'))
            retained={p.name:p.read_bytes() for p in root.iterdir()}
            fresh=Path(temporary)/'fresh'
            self.run_capture(command,fresh)
            self.assertTrue((fresh/'manifest.json').is_file())
            self.assertEqual({p.name:p.read_bytes() for p in root.iterdir()},retained)

if __name__=='__main__':unittest.main()
