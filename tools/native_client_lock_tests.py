"""A competing controller cannot turn bounded calls into unbounded lock waits."""
import fcntl
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class ClientLockTests(unittest.TestCase):
    def test_busy_client_refuses_request_recovery_and_saved_outcome_without_mutation(self):
        script = '''
import sys
from pathlib import Path
from native_ios_control import ClockClient as IOS
from native_android_control import ClockClient as Android
from native_clock_control import recorded_outcome
from native_ios_ui import UIClient
from native_android_input import InputClient
root=Path(sys.argv[1]);mode=sys.argv[2]
def unexpected(*args,**kwargs):raise AssertionError('Busy client contacted target')
try:
 if mode.startswith('ios-'):
  client=IOS(root/'missing-endpoint',root,transport=unexpected)
  client.request('read') if mode.endswith('request') else client.recover()
 elif mode.startswith('android-'):
  client=Android(unexpected,root)
  client.request('read') if mode.endswith('request') else client.recover()
 elif mode.startswith('ui-'):
  client=UIClient(root/'missing-endpoint',root,transport=unexpected)
  client.request('observe') if mode.endswith('request') else client.recover()
 elif mode.startswith('input-'):
  client=InputClient(unexpected,root)
  action=mode.split('-')[1]
  if action=='request':
   client.tap('target',1,1)
  else:getattr(client,action)()
 else:recorded_outcome(root,unexpected,prefix=mode)
except BlockingIOError:sys.exit(75)
raise AssertionError('Busy client admitted operation')
'''
        for mode in ('ios-request', 'ios-recover', 'android-request', 'android-recover', 'clock', 'ui', 'input',
                     'ui-request', 'ui-recover', 'input-initialize', 'input-request',
                     'input-recover'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                prefix = ('input' if mode.startswith('input') else
                          'ui' if mode.startswith('ui') else 'clock')
                cursor = root / (prefix + '-client.json')
                original = b'{"pending":"must remain byte-identical"}\n'
                cursor.write_bytes(original)
                lock_path = root / (prefix + '-client.lock')
                with lock_path.open('a') as owner:
                    fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    child = subprocess.Popen([sys.executable, '-c', script, directory, mode],
                                             cwd=Path(__file__).parent, stdout=subprocess.PIPE,
                                             stderr=subprocess.PIPE, text=True)
                    try:
                        stdout, stderr = child.communicate(timeout=3)
                    except subprocess.TimeoutExpired:
                        child.kill(); child.communicate()
                        self.fail('Competing controller blocked on the owned lock')
                    finally:
                        if child.poll() is None:
                            child.kill(); child.wait()
                    self.assertEqual(child.returncode, 75, stdout + stderr)
                    self.assertEqual(cursor.read_bytes(), original)
                    self.assertEqual(set(root.iterdir()), {cursor, lock_path})


if __name__ == '__main__':
    unittest.main()
