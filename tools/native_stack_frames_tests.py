"""Wrong-binary and cross-observation symbolization must never produce apparent evidence."""
import json
from pathlib import Path
import struct
import tempfile
import unittest

from native_stack_frames import analyze, binary_identity


class RawFrameIdentityTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.binary = self.root/'fixture'
        # A thin ARM64 image with one executable segment and a known UUID.
        self.binary.write_bytes(struct.pack('<IiiIIIII', 0xfeedfacf, 0x100000c, 0, 2, 2, 96, 0, 0)
            + struct.pack('<II16sQQQQiiII', 0x19, 72, b'__TEXT', 0x100000000, 4096, 0, 128, 5, 5, 0, 0)
            + struct.pack('<II', 0x1b, 24) + bytes.fromhex('11'*16))
        self.trace = self.root/'pending.txt'
        self.calls = []

    def resolve(self, binary, load, addresses):
        self.calls.append((load, addresses))
        return ['held_callback' for _ in addresses]

    def observation(self, *, uuid='11'*16, end=True, images=True):
        text = ('scheduler pid=123 host_ticks=10\n'
                'thread_stack id=7 state_result=0 pc=0x200000010 lr=0x300000010\n'
                'thread_frame id=7 depth=0 fp=0x400000000 return=0x200000020\n'
                'thread_stack_end id=7 frames=1 stop=depth_limit result=0 host_ticks=11\n')
        if images:
            text += f'stack_image index=0 header=0x200000000 uuid={uuid} stop=uuid result=0\n'
        if end:
            text += 'scheduler_end captured=1 omitted=0 host_ticks=12 numer=1 denom=1\n'
        return text

    def report(self, text):
        self.trace.write_text(text)
        return analyze(self.trace, self.binary, resolve=self.resolve)

    def test_matching_uuid_load_and_executable_range_are_required(self):
        report = self.report(self.observation())
        self.assertEqual(self.calls, [(0x200000000, [0x200000010, 0x200000020])])
        addresses = report['snapshots'][0]['threads'][0]['addresses']
        self.assertEqual([a['status'] for a in addresses], ['interpreted', 'outside_binary', 'interpreted'])
        self.assertNotIn('interpretation', addresses[1])

    def test_wrong_uuid_incomplete_or_ambiguous_image_never_calls_symbolizer(self):
        for text, status in [
            (self.observation(uuid='22'*16), 'binary_not_observed'),
            (self.observation(end=False), 'incomplete_snapshot'),
            (self.observation().replace('stop=uuid result=0', 'stop=uuid result=1'), 'invalid_trace'),
            (self.observation().replace('scheduler_end', 'stack_image index=1 header=0x210000000 uuid='+ '11'*16+' stop=uuid result=0\nscheduler_end'), 'ambiguous_load'),
        ]:
            with self.subTest(status=status):
                self.calls.clear()
                self.assertEqual(self.report(text)['snapshots'][0]['symbol_status'], status)
                self.assertFalse(self.calls)

    def test_image_inventory_does_not_cross_scheduler_intervals(self):
        report = self.report(self.observation(images=False) + self.observation())
        self.assertEqual(report['snapshots'][0]['symbol_status'], 'binary_not_observed')
        self.assertEqual(len(self.calls), 1)

    def submission(self):
        return ('task id=19 state=queued thread=0 queue=fixture\n'
                'task_frame id=19 depth=0 address=0x200000030\n')

    def test_submission_sites_are_separate_and_bound_to_their_own_failure(self):
        report = self.report(self.observation() + self.submission())
        snapshot = report['snapshots'][0]
        self.assertEqual(len(snapshot['threads']), 1)
        self.assertEqual(snapshot['tasks'][0]['id'], 19)
        address = snapshot['tasks'][0]['addresses'][0]
        self.assertEqual((address['kind'], address['status']), ('submission', 'interpreted'))
        self.assertEqual(self.calls[0][1], [0x200000010, 0x200000020, 0x200000030])
        self.calls.clear()
        report = self.report(self.observation() + 'timeout now_ns=0 pending=1\n' +
                             self.submission() + self.observation(images=False) + self.submission())
        self.assertEqual(report['snapshots'][0]['tasks'], [])
        self.assertEqual(report['snapshots'][1]['symbol_status'], 'binary_not_observed')
        self.assertNotIn('interpretation', report['snapshots'][1]['tasks'][0]['addresses'][0])

    def test_unbound_or_out_of_order_submission_frames_are_not_interpreted(self):
        for submission in (self.submission().replace('task_frame id=19', 'task_frame id=20'),
                           self.submission().replace('depth=0', 'depth=1'),
                           self.submission() + self.submission()):
            with self.subTest(submission=submission):
                self.calls.clear()
                report = self.report(self.observation() + submission)
                self.assertEqual(report['snapshots'][0]['symbol_status'], 'invalid_trace')
                self.assertFalse(self.calls)

    def test_inconsistent_execution_frames_never_reach_symbolizer(self):
        original = self.observation()
        frame = 'thread_frame id=7 depth=0 fp=0x400000000 return=0x200000020\n'
        end = 'thread_stack_end id=7 frames=1 stop=depth_limit result=0 host_ticks=11\n'
        damaged = [original.replace('depth=0', 'depth=1'),
                   original.replace(frame, frame + frame),
                   original.replace('frames=1', 'frames=2'),
                   original.replace(end, ''),
                   original.replace(end, end + frame),
                   original.replace(end, end + end),
                   original.replace('thread_frame id=7',
                                    'thread_stack id=7 stop=budget\nthread_frame id=7'),
                   original.replace('state_result=0', 'state_result=5')]
        for text in damaged:
            with self.subTest(text=text):
                self.calls.clear()
                report = self.report(text)
                self.assertEqual(report['snapshots'][0]['symbol_status'], 'invalid_trace')
                self.assertFalse(self.calls)

    def test_execution_records_after_scheduler_end_invalidate_only_that_snapshot(self):
        late_records = [line for line in self.observation().splitlines()
                        if not line.startswith('scheduler ')]
        late_records.append('thread id=7 id_result=0')
        for late in late_records:
            with self.subTest(late=late):
                self.calls.clear()
                report = self.report(self.observation() + late + '\n' + self.observation())
                self.assertEqual(report['snapshots'][0]['symbol_status'], 'invalid_trace')
                self.assertEqual(report['snapshots'][1]['symbol_status'], 'available')
                self.assertEqual(len(self.calls), 1)

    def test_partial_execution_walk_keeps_observed_addresses(self):
        text = self.observation().replace('stop=depth_limit result=0', 'stop=read_error result=1')
        text = text.replace('stack_image index=0',
                            'thread_stack id=8 stop=budget host_ticks=11\nstack_image index=0')
        report = self.report(text)
        self.assertEqual(report['snapshots'][0]['symbol_status'], 'available')
        self.assertEqual(self.calls, [(0x200000000, [0x200000010, 0x200000020])])
        self.assertEqual(report['snapshots'][0]['threads'][0]['stop'], 'read_error')
        self.assertEqual(report['snapshots'][0]['threads'][1]['addresses'], [])

    def test_unknown_thread_and_symbolizer_failure_stay_unavailable(self):
        text = self.observation().replace('thread_frame id=7', 'thread_frame id=8')
        self.assertEqual(self.report(text)['snapshots'][0]['symbol_status'], 'invalid_trace')
        self.assertFalse(self.calls)
        self.trace.write_text(self.observation())
        def fail(*args): raise OSError('symbolizer unavailable')
        report = analyze(self.trace, self.binary, resolve=fail)
        self.assertEqual(report['snapshots'][0]['symbol_status'], 'symbolizer_error')
        self.assertNotIn('interpretation', report['snapshots'][0]['threads'][0]['addresses'][0])

    def test_binary_changed_or_removed_during_interpretation_invalidates_symbols(self):
        original = self.binary.read_bytes()
        self.trace.write_text(self.observation() + self.submission())
        for mode in ('changed', 'unavailable'):
            with self.subTest(mode=mode):
                self.binary.write_bytes(original)
                def change(binary, load, addresses):
                    if mode == 'changed':
                        binary.write_bytes(original + b'changed after identity check')
                    else:
                        binary.unlink()
                    return ['apparently_valid_function' for _ in addresses]
                report = analyze(self.trace, self.binary, resolve=change)
                self.assertFalse(any('interpretation' in a for t in report['snapshots'][0]['threads'] + report['snapshots'][0]['tasks']
                                     for a in t['addresses']))
                self.assertEqual(report['binary_verification']['status'], mode)
                self.assertEqual(report['snapshots'][0]['symbol_status'], 'binary_unverified')

    def universal(self, wide=False):
        arm = self.binary.read_bytes()
        other = bytearray(arm)
        struct.pack_into('<i', other, 4, 0x1000007)
        other[-16:] = bytes.fromhex('22'*16)
        result = bytearray(640)
        struct.pack_into('>II', result, 0, 0xcafebabf if wide else 0xcafebabe, 2)
        layout = '>iiQQII' if wide else '>iiIII'
        width = struct.calcsize(layout)
        for i, (cpu, offset) in enumerate([(0x1000007, 256), (0x100000c, 512)]):
            values = (cpu, 0, offset, len(arm), 8) + ((0,) if wide else ())
            struct.pack_into(layout, result, 8 + i * width, *values)
        result[256:384] = other
        result[512:640] = arm
        return result

    def test_universal_binary_selects_arm64_identity_not_first_slice(self):
        original = self.binary.read_bytes()
        for wide in (False, True):
            with self.subTest(wide=wide):
                self.binary.write_bytes(original)
                data = self.universal(wide)
                self.binary.write_bytes(data)
                self.calls.clear()
                report = self.report(self.observation())
                self.assertEqual(report['binary_identity']['uuid'], '11'*16)
                self.assertEqual(report['binary_identity']['slice_offset'], 512)
                self.assertEqual(report['binary_verification']['status'], 'matched')
                self.assertEqual(self.calls, [(0x200000000, [0x200000010, 0x200000020])])
                self.assertEqual(self.binary.read_bytes(), data)

    def test_ambiguous_overlapping_or_truncated_universal_slices_are_rejected(self):
        data = self.universal()
        for offset, value in [(8, 0x100000c), (36, 256), (36, 768)]:
            with self.subTest(offset=offset, value=value):
                changed = bytearray(data)
                struct.pack_into('>I', changed, offset, value)
                self.binary.write_bytes(changed)
                with self.assertRaises(ValueError):
                    binary_identity(self.binary)

    def test_truncated_binary_is_rejected_and_inputs_remain_unchanged(self):
        original = self.binary.read_bytes()
        self.binary.write_bytes(original[:-1])
        with self.assertRaises(ValueError): binary_identity(self.binary)
        self.binary.write_bytes(original)
        text = self.observation()
        report = self.report(text)
        self.assertEqual(self.binary.read_bytes(), original)
        self.assertEqual(self.trace.read_text(), text)
        json.dumps(report)


if __name__ == '__main__':
    unittest.main()
