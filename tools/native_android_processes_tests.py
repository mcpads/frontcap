"""A missing or malformed guest inventory cannot establish fixture retirement."""
import unittest
from native_android_processes import parse_inventory, process_absent


class AndroidProcessesTests(unittest.TestCase):
    def test_requires_complete_inventory_and_rejects_pid_or_package_survivors(self):
        for header, init, target, replacement in (
                ('PID STAT NAME', '1 S init', '42 Z [dead fixture]', '99 S dev.fixture'),
                ('PID PPID STAT NAME', '1 0 S init', '42 7 Z [dead fixture]', '99 7 S dev.fixture')):
            with self.subTest(header=header):
                absent = header+'\n'+init+'\n'
                self.assertTrue(process_absent(absent, 42, 'dev.fixture'))
                for remaining in (target, replacement):
                    self.assertFalse(process_absent(absent+remaining+'\n', 42, 'dev.fixture'))
                self.assertEqual(parse_inventory(absent+target+'\n')[-1]['name'], '[dead fixture]')

    def test_incomplete_or_ambiguous_inventory_is_not_absence(self):
        for text in ('', 'PID PPID STAT NAME\n', 'PID PPID STAT NAME\n1 0 S\n',
                     'PID STAT NAME\n1 S init\n42\n', 'PID STAT NAME\nno S init\n',
                     'PID STAT NAME\n1 S init\n1 S duplicate\n',
                     'PID PPID STAT NAME\n1 -2 S init\n', 'PID STAT NAME\n0 S init\n',
                     'PID NAME\n1 init\n', 'PID STAT NAME\n1 S init\n\n'):
            with self.subTest(text=text):
                with self.assertRaises(ValueError): parse_inventory(text)
                self.assertFalse(process_absent(text, 42, 'dev.fixture'))
