"""Prevent timer diagnostics from inventing a deadline or misattributing a stack."""
import json
import unittest

from native_timer_trace import describe

STACK = '0 vclock.dylib 0x10 trace_timer + 20\n1 UpdateCycle 0x20 driver + 8\n'


def header():
    return json.dumps(dict(timer='mach-leeway', port=12, deadline_ticks=101, base_ticks=100,
                           now_ns=0, numer=125, denom=3, manual=True)) + '\n'


class TimerTraceTests(unittest.TestCase):
    def test_exact_relative_deadline_and_due_boundary(self):
        row = describe(header() + STACK)['registrations'][0]
        self.assertEqual(row['remaining_ns'], dict(numerator=125, denominator=3))
        self.assertEqual(row['relation'], 'future')
        self.assertEqual(row['caller']['module'], 'UpdateCycle')
        data = json.loads(header())
        data.update(deadline_ticks=103, now_ns=125)
        row = describe(json.dumps(data) + '\n' + STACK)['registrations'][0]
        self.assertEqual(row['remaining_ns'], dict(numerator=0, denominator=1))
        self.assertEqual(row['relation'], 'due')

    def test_legacy_and_scaled_records_do_not_invent_manual_deadlines(self):
        legacy = 'timer=mach deadline=100 virtual_ns=0\n' + STACK
        data = json.loads(header())
        data['manual'] = False
        rows = describe(legacy + json.dumps(data) + '\n' + STACK)['registrations']
        self.assertEqual([r['basis'] for r in rows], ['legacy_without_origin', 'not_manual'])
        self.assertTrue(all(r['remaining_ns'] is None and r['relation'] == 'unknown' for r in rows))

    def test_interleaved_or_truncated_stacks_are_rejected(self):
        for trace in (header(), header() + STACK + STACK, header() + '1 UIKit 0x20 dispatch\n'):
            with self.subTest(trace=trace), self.assertRaises(ValueError):
                describe(trace)

    def test_invalid_timebase_is_rejected(self):
        data = json.loads(header())
        data['denom'] = 0
        with self.assertRaises(ValueError):
            describe(json.dumps(data) + '\n' + STACK)


if __name__ == '__main__':
    unittest.main()
