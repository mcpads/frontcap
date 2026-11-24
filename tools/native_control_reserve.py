"""Physical headroom owned by one live worker, separate from logical write admission."""
import os
import tempfile


def reserve_control(directory):
    """Hold 8 MiB through a private descriptor, never a saved cleanup path."""
    stream = tempfile.TemporaryFile(dir=directory)
    try:
        for _ in range(8):
            block = memoryview(os.urandom(1024 * 1024))
            while block:
                count = stream.write(block)
                if not count:
                    raise RuntimeError('Control reserve allocation made no progress')
                block = block[count:]
        stream.flush()
        os.fsync(stream.fileno())
        if os.fstat(stream.fileno()).st_blocks * 512 < 8 * 1024 * 1024:
            raise RuntimeError('Control reserve was not physically allocated')
        return stream
    except BaseException:
        stream.close()
        raise
