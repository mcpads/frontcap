"""Bounded merged process output, drained by the same owner that retires its writer.

Call finish only after attempting writer retirement. This collector grants no process authority
and must not be placed in a shorter-lived controller of an independent worker.
"""
import os
import threading
from native_clock_control import atomic_json


class OutputLog:
    def __init__(self, reader, stream, limit=4 * 1024 * 1024, *, admit=None):
        if type(limit) is not int or limit < 0:
            raise ValueError('Invalid command log bound')
        self.reader, self.stream = reader, stream
        self.admit = admit
        self.stop = threading.Event()
        self.failed = threading.Event()
        self.report = dict(limit_bytes=limit, retained_bytes=0, observed_bytes=0,
                           eof=False, error=None)
        self.thread = threading.Thread(target=self._drain, daemon=True)
        self.thread.start()

    def _drain(self):
        try:
            os.set_blocking(self.reader.fileno(), False)
            while not self.stop.is_set():
                try:
                    data = os.read(self.reader.fileno(), 65536)
                except BlockingIOError:
                    self.stop.wait(.005)
                    continue
                if not data:
                    self.report['eof'] = True
                    break
                self.report['observed_bytes'] += len(data)
                keep = data[:max(0, self.report['limit_bytes'] - self.report['retained_bytes'])]
                if self.report['error'] is None:
                    try:
                        if self.admit is not None:
                            self.admit(len(keep))
                        self.stream.write(keep)
                        self.stream.flush()  # Endpoint discovery reads the retained prefix while alive.
                        self.report['retained_bytes'] += len(keep)
                    except Exception as error:
                        self.report['error'] = f'Command log retention failed: {error}'
                        self.failed.set()
                if len(keep) != len(data) and self.report['error'] is None:
                    self.report['error'] = 'Command output limit exceeded'
                    self.failed.set()
                # Drain even after overflow, including while the owner is retiring writers.
        except Exception as error:
            self.report['error'] = f'Command log collection failed: {error}'
            self.failed.set()

    def check(self):
        if self.failed.is_set():
            raise RuntimeError(self.report['error'])

    def finish(self, path, *, write_report=atomic_json):
        self.thread.join(timeout=2)
        self.stop.set()
        self.thread.join()
        self.reader.close()
        self.stream.flush()
        os.fsync(self.stream.fileno())
        self.report['omitted_bytes'] = self.report['observed_bytes'] - self.report['retained_bytes']
        write_report(path, self.report)
        return dict(self.report)
