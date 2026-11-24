"""Incomplete raw syscalls must not become completed operations or borrow another PID."""
from pathlib import Path
import tempfile
import unittest
import xml.etree.ElementTree as ET

from native_file_trace import analyze


def element(tag, text=None, **attributes):
    node = ET.Element(tag, attributes)
    if text is not None:
        node.text = str(text)
    return node


def process(pid):
    node = element('process')
    node.append(element('pid', pid))
    return node


def thread(pid, tid):
    node = element('thread')
    node.append(process(pid))
    node.append(element('tid', tid))
    return node


class FileTraceTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.write('syscall-name-map', [dict(name=element('syscall', 'BSC_open'),
            code=element('code', 5), subclass=element('subclass', 12), **{'class': element('class', 4)})])
        self.write('FsSyscall', [dict(process=process(31), start=element('time', 10),
            duration=element('duration', 10), syscall=element('syscall', 'BSC_open'),
            path=element('path', '/owned/file'), **{'return': element('return', 3), 'errno': element('errno', 0)})])

    def write(self, schema, rows):
        root = element('trace-query-result')
        node = ET.SubElement(root, 'node')
        declaration = ET.SubElement(node, 'schema', name=schema)
        columns = list(rows[0])
        for name in columns:
            ET.SubElement(ET.SubElement(declaration, 'col'), 'mnemonic').text = name
        for row in rows:
            record = ET.SubElement(node, 'row')
            record.extend(row[name] for name in columns)
        ET.ElementTree(root).write(self.root/(schema+'.xml'))

    def event(self, when, phase, pid=31, tid=7):
        return dict(time=element('time', when), thread=thread(pid, tid),
                    function=element('function', fmt=phase), code=element('code', 5),
                    subclass=element('subclass', 12), **{'class': element('class', 4)})

    def test_finished_and_unfinished_calls_remain_distinct(self):
        self.write('kdebug', [self.event(5, 'END'), self.event(10, 'START'),
                              self.event(20, 'END'), self.event(30, 'START')])
        # A recorder's target label does not provide this process's authoritative exit.
        (self.root/'toc.xml').write_text('<process return-exit-status="0"/>')
        report = analyze(self.root, 31)
        self.assertEqual(report['paired_events'], [dict(thread_id=7, syscall='BSC_open',
                                                       start_ns=10, end_ns=20, duration_ns=10,
                                                       start_arguments=dict.fromkeys(('arg1','arg2','arg3','arg4')),
                                                       end_arguments=dict.fromkeys(('arg1','arg2','arg3','arg4')))])
        self.assertEqual(report['unmatched_starts'], [dict(thread_id=7, syscall='BSC_open', time_ns=30,
            arguments=dict.fromkeys(('arg1','arg2','arg3','arg4')))])
        self.assertEqual(report['unmatched_ends'], [dict(thread_id=7, syscall='BSC_open', time_ns=5,
            arguments=dict.fromkeys(('arg1','arg2','arg3','arg4')))])
        self.assertEqual(report['calls'][0]['path'], '/owned/file')
        self.assertEqual(len(report['sources']), 3)

    def test_raw_arguments_preserve_zero_missing_and_phase_values(self):
        start, end, unfinished = self.event(10, 'START'), self.event(20, 'END'), self.event(30, 'START')
        for event in (start, end, unfinished):
            event.update(arg1=element('arg', 0), arg2=element('arg', 4),
                         arg3=element('sentinel'), arg4=element('arg', 18446744073709551615))
        end['arg2'] = element('arg', 0)
        start['arg2'].set('id', 'flags')
        unfinished['arg2'] = element('arg', ref='flags')
        self.write('kdebug', [start, end, unfinished])
        report = analyze(self.root, 31)
        pair = report['paired_events'][0]
        self.assertEqual(pair['start_arguments'], dict(arg1=0, arg2=4, arg3=None,
                                                       arg4=18446744073709551615))
        self.assertEqual(pair['end_arguments']['arg2'], 0)
        self.assertEqual(report['unmatched_starts'][0]['arguments']['arg2'], 4)
        self.assertIsNone(report['calls'][0]['thread_id'])

    def test_other_process_cannot_complete_owned_event(self):
        self.write('kdebug', [self.event(10, 'START'), self.event(20, 'END', pid=32)])
        report = analyze(self.root, 31)
        self.assertFalse(report['paired_events'])
        self.assertEqual(len(report['unmatched_starts']), 1)
        self.assertEqual(report['other_pid_rows'], 1)

    def test_repeated_start_does_not_guess_a_pair(self):
        self.write('kdebug', [self.event(10, 'START'), self.event(11, 'START'), self.event(20, 'END')])
        report = analyze(self.root, 31)
        self.assertFalse(report['paired_events'])
        self.assertEqual(len(report['unmatched_starts']), 2)
        self.assertEqual(len(report['unmatched_ends']), 1)
        self.assertEqual(report['unavailable'][0]['reason'], 'repeated_start')

    def test_references_resolve_and_invalid_references_fail(self):
        first = self.event(10, 'START')
        first['thread'].set('id', 'owned')
        last = self.event(20, 'END')
        last['thread'] = element('thread', ref='owned')
        self.write('kdebug', [first, last])
        self.assertEqual(len(analyze(self.root, 31)['paired_events']), 1)
        for attributes in (dict(ref='missing'), dict(id='loop', ref='loop'), dict(id='owned')):
            with self.subTest(attributes=attributes):
                last['thread'] = element('thread', **attributes)
                self.write('kdebug', [first, last])
                with self.assertRaises(ValueError):
                    analyze(self.root, 31)

    def test_missing_identity_stays_unavailable(self):
        event = self.event(10, 'START')
        event['thread'] = element('sentinel')
        self.write('kdebug', [event])
        report = analyze(self.root, 31)
        self.assertFalse(report['paired_events'])
        self.assertFalse(report['unmatched_starts'])
        self.assertEqual(report['unavailable'][0]['reason'], 'missing_process_identity')

    def test_backwards_events_are_rejected(self):
        self.write('kdebug', [self.event(20, 'START'), self.event(10, 'END')])
        with self.assertRaisesRegex(ValueError, 'backwards'):
            analyze(self.root, 31)


if __name__ == '__main__':
    unittest.main()
