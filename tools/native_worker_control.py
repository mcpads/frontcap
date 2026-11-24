"""One bounded terminal record on a separately selected recorder filesystem.

This routes evidence; it does not reserve disk capacity or authorize device/process adoption.
"""
import json
import os
from pathlib import Path
import stat
from native_write_budget import charge_payload

LIMIT = 64 * 1024


class TerminalRecord:
    def __init__(self, directory):
        self.directory = Path(directory)
        metadata = self.directory.lstat()
        if not self.directory.is_absolute() or not stat.S_ISDIR(metadata.st_mode):
            raise ValueError('Control directory must be absolute and not linked')
        self.identity = (metadata.st_dev, metadata.st_ino)
        if any(self.directory.iterdir()):
            raise ValueError('Control directory must be fresh')

    def publish(self, value):
        data = (json.dumps(value, indent=2)+'\n').encode()
        if len(data) > LIMIT:
            raise ValueError('Worker terminal record exceeds 64 KiB')
        metadata = self.directory.lstat()
        if not stat.S_ISDIR(metadata.st_mode) or (metadata.st_dev, metadata.st_ino) != self.identity:
            raise ValueError('Worker control directory identity changed')
        # Hold the checked directory itself: a pathname replacement must not redirect writes.
        descriptor = os.open(self.directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            opened = os.fstat(descriptor)
            if (opened.st_dev, opened.st_ino) != self.identity:
                raise ValueError('Worker control directory changed while opening')
            try:
                os.stat('result.json', dir_fd=descriptor, follow_symlinks=False)
            except FileNotFoundError:
                pass
            else:
                raise ValueError('Worker terminal record already exists')
            charge_payload(len(data), 'retirement')
            fd = os.open('result.tmp', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                         0o600, dir_fd=descriptor)
            with os.fdopen(fd, 'wb') as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            # link is atomic and refuses an existing destination, unlike replace/rename.
            # Failed publication retains the temporary evidence and never removes another result.
            try:
                os.link('result.tmp', 'result.json', src_dir_fd=descriptor, dst_dir_fd=descriptor,
                        follow_symlinks=False)
            except FileExistsError as error:
                raise ValueError('Worker terminal record already exists') from error
            os.unlink('result.tmp', dir_fd=descriptor)
            current = self.directory.lstat()
            if not stat.S_ISDIR(current.st_mode) or (current.st_dev, current.st_ino) != self.identity:
                raise ValueError('Worker control directory changed during publication')
        finally:
            os.close(descriptor)
