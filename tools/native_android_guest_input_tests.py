"""Preserve advertised feature semantics and refuse signals after guest identity changes."""
import unittest
from unittest.mock import Mock
from native_android_guest_input import GuestInputGate, require_shell_protocol


class GuestInputTests(unittest.TestCase):
    def test_shell_feature_requires_an_exact_line(self):
        require_shell_protocol('cmd\nshell_v2\nstat_v2\n')
        for missing in ('cmd\nstat_v2\n','not_shell_v2\n','shell_v2_disabled\n','shell_v2,cmd'):
            with self.subTest(features=missing), self.assertRaisesRegex(RuntimeError,'shell_v2'):
                require_shell_protocol(missing)

    def test_reused_pid_or_changed_executable_is_never_signalled(self):
        gate=GuestInputGate(None,[])
        identity=dict(pid=42,start_ticks=100,command='/owned/input-gate /owned/condition',exe='/owned/input-gate',uid=2000,state='T')
        row=dict(identity=identity)
        gate.shell=Mock()
        for changed in ({'start_ticks':101},{'exe':'/system/bin/sh'},{'uid':1000},{'state':'R'}):
            gate.snapshot=Mock(return_value={**identity,**changed})
            with self.subTest(change=changed),self.assertRaisesRegex(RuntimeError,'refusing signal'):
                gate.signal(row,'KILL')
        gate.shell.assert_not_called()
        gate.snapshot=Mock(return_value=identity)
        gate.signal(row,'CONT')
        gate.shell.assert_called_once_with('kill -CONT 42')


if __name__=='__main__':unittest.main()
