import copy
import hashlib
import unittest

from native_trace_alignment import align, refine_with_markers


class TraceAlignmentTests(unittest.TestCase):
    def setUp(self):
        self.log = b'ab'
        self.reads = [dict(offset=i, length=1, sha256=hashlib.sha256(bytes([b])).hexdigest(),
                           started_monotonic_ns=125 + i * 100, finished_monotonic_ns=130 + i * 100,
                           previous_empty_read=dict(offset=i, started_monotonic_ns=115 + i * 100,
                                                    finished_monotonic_ns=116 + i * 100))
                      for i, b in enumerate(self.log)]
        self.trace = dict(unavailable=[], unmatched_starts=[], unmatched_ends=[], paired_events=[
            dict(syscall='BSC_write_nocancel', thread_id=9, start_ns=10 + i * 100, end_ns=20 + i * 100,
                 start_arguments=dict(arg1=2, arg3=1), end_arguments=dict(arg1=0, arg2=1))
            for i in range(2)])

    def test_visibility_inside_write_keeps_possible_offset(self):
        # At offset 100, each empty read overlaps its corresponding write before visibility.
        # Bounding write START from the empty read would incorrectly exclude that offset.
        result = align(self.log, self.reads, self.trace)
        self.assertEqual(result['common_bounds_ns'], [95, 120])
        self.assertEqual(result['status'], 'conditional_alignment')

    def test_conflicting_clocks_are_rejected(self):
        self.reads[1]['previous_empty_read']['started_monotonic_ns'] += 50
        self.reads[1]['previous_empty_read']['finished_monotonic_ns'] += 50
        self.reads[1]['started_monotonic_ns'] += 50
        self.reads[1]['finished_monotonic_ns'] += 50
        with self.assertRaisesRegex(ValueError, 'contradict'):
            align(self.log, self.reads, self.trace)

    def test_partial_or_ambiguous_evidence_cannot_supply_alignment(self):
        for kind in ('digest', 'missing_read', 'unmatched_write', 'overlap', 'partial_write', 'empty_after_read'):
            with self.subTest(kind=kind):
                reads, trace = copy.deepcopy(self.reads), copy.deepcopy(self.trace)
                if kind == 'digest': reads[0]['sha256'] = 'wrong'
                elif kind == 'missing_read': reads.pop()
                elif kind == 'unmatched_write': trace['unmatched_starts'].append(dict(syscall='BSC_write'))
                elif kind == 'overlap': trace['paired_events'][1]['start_ns'] = 15
                elif kind == 'partial_write': trace['paired_events'][1]['start_arguments']['arg3'] = 2
                elif kind == 'empty_after_read': reads[1]['previous_empty_read']['finished_monotonic_ns'] = 231
                with self.assertRaises(ValueError):
                    align(self.log, reads, trace)


    def markers(self):
        trace = copy.deepcopy(self.trace)
        trace.update(pid=42, calls=[])
        markers = dict(pid=42, rows=[])
        for i in range(2):
            start = 300 + i * 100
            path = f'/owned/marker-{i}'
            markers['rows'].append(dict(path=path, before_ns=start + 100, after_ns=start + 111))
            trace['calls'].append(dict(path=path, syscall='BSC_open', thread_id=9, start_ns=start,
                                       duration_ns=10, result=3, errno=0))
            trace['paired_events'].append(dict(syscall='BSC_open', thread_id=9, start_ns=start,
                                               end_ns=start + 10, duration_ns=10,
                                               end_arguments=dict(arg1=0, arg2=3)))
        return markers, trace

    def test_marker_bounds_refine_without_replacing_log_evidence(self):
        report = align(self.log, self.reads, self.trace)
        markers, trace = self.markers()
        refined = refine_with_markers(report, markers, trace)
        self.assertEqual(refined['common_bounds_ns'], [100, 101])
        self.assertEqual(refined['log_bounds_ns'], [95, 120])
        self.assertEqual(report['common_bounds_ns'], [95, 120])
        self.assertEqual(refined['anchors'], report['anchors'])
        self.assertEqual(refined['status'], 'conditional_alignment')

    def test_markers_cannot_borrow_identity_events_or_conflicting_time(self):
        for kind in ('pid', 'duplicate_path', 'duplicate_call', 'missing_raw', 'return_mismatch',
                     'duration_mismatch', 'failed_open', 'reordered', 'short_bracket', 'conflict', 'single'):
            with self.subTest(kind=kind):
                report = align(self.log, self.reads, self.trace)
                markers, trace = self.markers()
                if kind == 'pid': markers['pid'] = 43
                elif kind == 'duplicate_path': markers['rows'][1]['path'] = markers['rows'][0]['path']
                elif kind == 'duplicate_call': trace['calls'].append(copy.deepcopy(trace['calls'][0]))
                elif kind == 'missing_raw': trace['paired_events'].pop()
                elif kind == 'return_mismatch': trace['paired_events'][-1]['end_arguments']['arg2'] = 4
                elif kind == 'duration_mismatch': trace['calls'][-1]['duration_ns'] = 9
                elif kind == 'failed_open': trace['calls'][-1]['errno'] = 2
                elif kind == 'reordered': markers['rows'].reverse()
                elif kind == 'short_bracket': markers['rows'][0]['after_ns'] -= 2
                elif kind == 'conflict':
                    for row in markers['rows']:
                        row['before_ns'] += 100
                        row['after_ns'] += 100
                elif kind == 'single': markers['rows'].pop()
                with self.assertRaises(ValueError):
                    refine_with_markers(report, markers, trace)


if __name__ == '__main__':
    unittest.main()
