"""Phase diagnostics preserve a real probe shell's failed command and owned cleanup."""
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr

from native_phase import record_phase


class NativePhaseTests(unittest.TestCase):
    def test_unavailable_recording_does_not_replace_probe_failure_or_skip_cleanup(self):
        script=Path(__file__).resolve().parents[1]/'spikes/s18-native-time/ios/run.sh'
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);fake=root/'bin';fake.mkdir()
            xcrun=fake/'xcrun'
            xcrun.write_text('''#!/bin/sh
printf '%s\\n' "$*" >> "$FC_TEST_TOOL_LOG"
case "$1" in
  --sdk) printf '/unused-sdk\\n';;
  clang) exit 0;;
  simctl) case "$2" in
    create) printf '00000000-0000-4000-8000-000000000001\\n';;
    boot) if [ "$FC_TEST_FAIL" = boot ]; then exit 7; fi;;
    bootstatus) exit 0;;
    spawn) if [ "${4##*/}" = "$FC_TEST_FAIL" ]; then exit 7; fi;;
    get_app_container) exit 1;;
    shutdown|delete) exit 0;;
    *) exit 99;;
  esac;;
  *) exit 99;;
esac
''')
            xcrun.chmod(0o755)
            (fake/'codesign').write_text('#!/bin/sh\nexit 0\n');(fake/'codesign').chmod(0o755)
            for failed_command in ("boot", "dispatch_wait"):
                for unavailable in (False,True):
                    with self.subTest(failed_command=failed_command,unavailable=unavailable):
                        label=failed_command+'-'+str(unavailable)
                        folder=root/label;folder.mkdir();calls=root/(label+'.calls')
                        (fake/'python3').unlink(missing_ok=True)
                        if unavailable:
                            (fake/'python3').write_text('#!/bin/sh\necho "phase writer unavailable" >&2\nexit 77\n')
                            (fake/'python3').chmod(0o755)
                        else:
                            (fake/'python3').symlink_to(sys.executable)
                        result=subprocess.run(['bash',str(script)],env={**os.environ,'PATH':str(fake)+':'+os.environ['PATH'],
                            'FC_OUTPUT_DIR':str(folder),'FC_TEST_TOOL_LOG':str(calls),'FC_TEST_FAIL':failed_command},capture_output=True,text=True,timeout=10)
                        self.assertEqual(result.returncode,7,result.stderr)
                        commands=calls.read_text().splitlines()
                        self.assertTrue(any(c.startswith('simctl shutdown ') for c in commands))
                        self.assertTrue(any(c.startswith('simctl delete ') for c in commands))
                        if unavailable:
                            self.assertIn('phase writer unavailable',result.stderr)
                        else:
                            events=[json.loads(line) for line in (folder/'host-phases.jsonl').read_text().splitlines()]
                            names=[r['phase'] for r in events]
                            self.assertLess(names.index('simulator-boot'),names.index('cleanup'))
                            self.assertEqual('native-fixtures' in names,failed_command!='boot')
                            self.assertNotIn('fade',names)
                            if failed_command!='boot':
                                trace=folder/'probe-timing'/failed_command/'host-phases.jsonl'
                                probe=[json.loads(line) for line in trace.read_text().splitlines()]
                                self.assertEqual([r['phase'] for r in probe],['command-start','command-finished'])
                                self.assertEqual(probe[-1]['exit_code'],7)
                            self.assertEqual(events[-1]['phase'],'finished')
                            self.assertEqual(events[-1]['exit_code'],7)
                            self.assertEqual(len({r['pid'] for r in events}),1)
                            self.assertEqual([r['host_monotonic_ns'] for r in events],sorted(r['host_monotonic_ns'] for r in events))

    def test_storage_failure_is_reported_without_truncating_prior_phases(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            self.assertTrue(record_phase(root,os.getpid(),'native-build'))
            before=(root/'host-phases.jsonl').read_bytes()
            self.assertTrue(record_phase(root,os.getpid(),'simulator-create'))
            self.assertTrue((root/'host-phases.jsonl').read_bytes().startswith(before))
            with redirect_stderr(io.StringIO()) as error:
                self.assertFalse(record_phase(root/'missing',os.getpid(),'cleanup',7))
            self.assertIn('Cannot save host phase evidence',error.getvalue())


if __name__ == '__main__': unittest.main()
