"""Native predicate diagnostics preserve evaluation and failure exit semantics."""
import errno
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest


class ProbeDiagnosticsTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == 'darwin' and shutil.which('cc'), 'Mach clock and C compiler required')
    def test_timing_uses_host_clock_and_preserves_failure_when_storage_is_unavailable(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root/'timing.c'
            source.write_text('''#include "probe_assert.h"
#include "probe_timing.h"
uint64_t fc_real_abs(void) { return mach_absolute_time(); }
int main(int argc, char **argv) {
  errno = EBUSY;
  FC_PROBE_TIMING("main-entry");
  require(errno == EBUSY);
  usleep(20000);
  errno = EBUSY;
  FC_PROBE_TIMING("report");
  require(errno == EBUSY);
  FC_REQUIRE(argc == 1, 7);
  FC_PROBE_TIMING("main-return");
  return 0;
}
''')
            include = Path(__file__).resolve().parents[1]/'spikes/s18-native-time/ios'
            binary = root/'timing'
            subprocess.run(['cc', '-I', str(include), str(source), '-Wl,-exported_symbol,_fc_real_abs',
                            '-o', str(binary)], capture_output=True, check=True, timeout=30)
            path = root/'native-phases.jsonl'
            env = {**os.environ, 'FC_PROBE_TIMING_PATH': str(path)}
            before = time.monotonic_ns()
            passed = subprocess.run([str(binary)], env=env, capture_output=True, text=True, timeout=5)
            after = time.monotonic_ns()
            self.assertEqual((passed.returncode, passed.stdout, passed.stderr), (0, '', ''))
            events = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertEqual([row['phase'] for row in events], ['main-entry', 'report', 'main-return'])
            stamps = [row['host_monotonic_ns'] for row in events]
            self.assertEqual(stamps, sorted(stamps))
            self.assertTrue(all(before <= stamp <= after for stamp in stamps))
            self.assertEqual(len({row['pid'] for row in events}), 1)
            original = path.read_bytes()
            failed = subprocess.run([str(binary), 'fail'], env=env, capture_output=True, text=True, timeout=5)
            self.assertEqual(failed.returncode, 7)
            self.assertTrue(path.read_bytes().startswith(original))
            appended = [json.loads(line) for line in path.read_bytes()[len(original):].splitlines()]
            self.assertEqual([row['phase'] for row in appended], ['main-entry', 'report'])
            unavailable = subprocess.run([str(binary), 'fail'], env={**env, 'FC_PROBE_TIMING_PATH': str(root/'missing/log')},
                                         capture_output=True, text=True, timeout=5)
            self.assertEqual(unavailable.returncode, 7)
            self.assertIn('Cannot record probe timing: open failed', unavailable.stderr)
            retained = path.read_bytes()
            disabled = subprocess.run([str(binary)], env={**env, 'FC_PROBE_TIMING_PATH': ''},
                                      capture_output=True, text=True, timeout=5)
            self.assertEqual((disabled.returncode, disabled.stdout, disabled.stderr), (0, '', ''))
            self.assertEqual(path.read_bytes(), retained)

    @unittest.skipUnless(shutil.which('cc'), 'C compiler required for native diagnostic validation')
    def test_predicate_is_evaluated_once_and_failed_logging_keeps_exit_code(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'probe.c'
            source.write_text('''#include "probe_assert.h"
int main(int argc, char **argv) {
  int evaluated = 0;
  if (argc == 1) {
    require(++evaluated == 1);
    return evaluated != 1;
  }
  if (argc > 2) close(STDERR_FILENO);
  errno = EBUSY;
  FC_REQUIRE(++evaluated == 2, 7);
  return 99;
}
''')
            include = Path(__file__).resolve().parents[1] / 'spikes/s18-native-time/ios'
            executable = root / 'probe'
            subprocess.run(['cc', '-I', str(include), str(source), '-o', str(executable)],
                           capture_output=True, check=True, timeout=30)
            passed = subprocess.run([str(executable)], capture_output=True, text=True, timeout=5)
            self.assertEqual((passed.returncode, passed.stdout, passed.stderr), (0, '', ''))
            failed = subprocess.run([str(executable), 'fail'], capture_output=True, text=True, timeout=5)
            self.assertEqual(failed.returncode, 7)
            self.assertIn(f'file={source}', failed.stderr)
            self.assertRegex(failed.stderr, r'line=\d+ condition=\+\+evaluated == 2')
            self.assertIn(f'errno_snapshot={errno.EBUSY} exit_code=7', failed.stderr)
            unwritable = subprocess.run([str(executable), 'fail', 'closed-stderr'],
                                       capture_output=True, timeout=5)
            self.assertEqual(unwritable.returncode, 7)
