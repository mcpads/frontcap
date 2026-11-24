"""File observations preserve native identity, descriptor reuse and acquisition limits."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from native_process_files import process_files
from native_process_resources import process_resources


@unittest.skipUnless(sys.platform=='darwin','macOS descriptor observations required')
class ProcessFilesTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory()
        cls.binary = Path(cls.temporary.name)/'sampler_files'
        subprocess.run(['xcrun','clang','-Wall','-Wextra','-Werror','-O2',str(Path(__file__).with_name('native_sampler_files.c')),
                        '-o',str(cls.binary)],check=True,capture_output=True,timeout=30)
        subprocess.run([str(cls.binary),'--ready'],check=True,capture_output=True,timeout=5)

    @classmethod
    def tearDownClass(cls):cls.temporary.cleanup()

    def snapshot(self, pid=None, identity=None):
        pid = os.getpid() if pid is None else pid
        return process_files(self.binary,pid,identity or process_resources(pid)['identity'],deadline=time.monotonic()+1)

    def test_native_path_file_identity_offset_and_descriptor_reuse(self):
        with tempfile.TemporaryDirectory() as directory:
            paths=[Path(directory)/'quote" slash\\ newline\n한글',Path(directory)/'replacement']
            for p in paths:p.write_bytes(b'0123456789abcdef')
            previous_fd = None;inodes=[]
            for path in paths:
                fd=os.open(path,os.O_RDONLY)
                try:
                    os.lseek(fd,7,os.SEEK_SET);expected=os.fstat(fd)
                    result=self.snapshot();self.assertTrue(result['identity_verified'],result)
                    row=next(r for r in result['files'] if r['fd']==fd)
                    self.assertEqual((row['path'],row['device'],row['inode'],row['mode'],row['size'],row['offset']),
                                     (str(path.resolve()),expected.st_dev,expected.st_ino,expected.st_mode,expected.st_size,7))
                    if previous_fd is not None:self.assertEqual(fd,previous_fd)
                    previous_fd=fd;inodes.append(row['inode'])
                finally:os.close(fd)
            self.assertNotEqual(*inodes)

    def test_changed_target_identity_cannot_publish_files(self):
        identity=process_resources(os.getpid())['identity'];identity['start_abstime']+=1
        result=self.snapshot(identity=identity)
        self.assertFalse(result['identity_verified']);self.assertEqual(result['files'],[])
        self.assertIn('identity',result['error'])

    def test_deadline_and_interruption_retire_the_isolated_query(self):
        create=subprocess.Popen
        for interrupt in (False,True):
            children=[]
            def launch(command,**kwargs):
                child=create(['/bin/sleep','30'],**kwargs);children.append(child)
                if interrupt:child.communicate=lambda **kw:(_ for _ in ()).throw(InterruptedError('cancel'))
                return child
            with self.subTest(interrupt=interrupt), patch('native_process_files.subprocess.Popen',side_effect=launch):
                if interrupt:
                    with self.assertRaises(InterruptedError):self.snapshot()
                else:
                    result=process_files(self.binary,os.getpid(),process_resources(os.getpid())['identity'],deadline=time.monotonic()+.02)
                    self.assertFalse(result['identity_verified']);self.assertEqual(result['files'],[])
                    self.assertIn('deadline',result['error'])
            self.assertEqual(len(children),1);self.assertIsNotNone(children[0].returncode)
            self.assertEqual(process_resources(children[0].pid).get('errno'),3)

    def test_expired_budget_never_starts_a_query(self):
        with patch('native_process_files.subprocess.Popen') as launch:
            result=process_files(self.binary,os.getpid(),process_resources(os.getpid())['identity'],deadline=time.monotonic()-1)
        launch.assert_not_called();self.assertFalse(result['identity_verified'])


if __name__=='__main__':unittest.main()
