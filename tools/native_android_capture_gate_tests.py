import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest


class CaptureGateTests(unittest.TestCase):
    def fixture(self, root, wait):
        adb = root/'adb'
        adb.write_text('#!'+sys.executable+'\nfrom pathlib import Path\n'
                       +'p=Path('+repr(str(root/'calls'))+')\n'
                       +'with p.open("a") as f:f.write("dispatch\\n")\n'
                       +'print("screenshot")\n')
        adb.chmod(0o700)
        code = 'import sys; sys.path.insert(0,sys.argv[1]); from native_android_capture_gate import run; run(sys.argv[2],sys.argv[3],sys.argv[5:],wait_seconds=float(sys.argv[4]))'
        return [sys.executable, '-c', code, str(Path(__file__).resolve().parent), str(adb),
                str(root/'gate'), str(wait), '-s', 'emulator-5580', 'exec-out', 'screencap', '-p']

    def test_release_dispatches_once_and_later_screenshots_do_not_wait(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder);command=self.fixture(root, 5)
            child=subprocess.Popen(command,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            try:
                deadline=time.monotonic()+3
                while not (root/'gate/state.json').exists():
                    self.assertIsNone(child.poll())
                    self.assertLess(time.monotonic(),deadline)
                    time.sleep(.005)
                self.assertEqual(json.loads((root/'gate/state.json').read_text())['status'],'waiting')
                self.assertFalse((root/'calls').exists())
                before=(root/'gate/state.json').read_bytes()
                duplicate=subprocess.run(command,capture_output=True,timeout=3)
                self.assertNotEqual(duplicate.returncode,0)
                self.assertFalse((root/'calls').exists())
                self.assertEqual((root/'gate/state.json').read_bytes(),before)
                (root/'gate/release').touch()
                out,err=child.communicate(timeout=3)
                self.assertEqual(child.returncode,0,err)
                self.assertEqual(out,b'screenshot\n')
                self.assertEqual((root/'calls').read_text(),'dispatch\n')
                marker=json.loads((root/'gate/state.json').read_text())
                self.assertEqual(marker['status'],'released')
                self.assertEqual(marker['pid'],child.pid)
                subprocess.run(command,check=True,capture_output=True,timeout=3)
                self.assertEqual((root/'calls').read_text(),'dispatch\ndispatch\n')
                self.assertEqual(json.loads((root/'gate/state.json').read_text()),marker)
            finally:
                if child.poll() is None:child.kill()
                child.communicate()

    def test_expiry_preserves_failure_without_screenshot_dispatch(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder)
            result=subprocess.run(self.fixture(root,.05),capture_output=True,timeout=3)
            self.assertNotEqual(result.returncode,0)
            self.assertIn(b'Capture gate expired',result.stderr)
            self.assertFalse((root/'calls').exists())
            self.assertEqual(json.loads((root/'gate/state.json').read_text())['status'],'expired')
            before=(root/'gate/state.json').read_bytes()
            (root/'gate/release').touch()  # A late release must not resurrect the failed attempt.
            retry=subprocess.run(self.fixture(root,.05),capture_output=True,timeout=3)
            self.assertNotEqual(retry.returncode,0)
            self.assertFalse((root/'calls').exists())
            self.assertEqual((root/'gate/state.json').read_bytes(),before)



if __name__ == '__main__':
    unittest.main()
