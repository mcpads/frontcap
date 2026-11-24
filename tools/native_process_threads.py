"""Bounded thread snapshots for an already identified, owned macOS process."""
import ctypes
from functools import lru_cache
import os
import sys
import time

from native_process_resources import process_resources


class ThreadInfo(ctypes.Structure):
    # SDK sys/proc_info.h, proc_threadinfo. CPU values are ns with us precision.
    _fields_ = [('pth_user_time', ctypes.c_uint64), ('pth_system_time', ctypes.c_uint64)] + [
        (name, ctypes.c_int32) for name in ('pth_cpu_usage', 'pth_policy', 'pth_run_state',
        'pth_flags', 'pth_sleep_time', 'pth_curpri', 'pth_priority', 'pth_maxpriority')
    ] + [('pth_name', ctypes.c_char * 64)]


@lru_cache(maxsize=1)
def thread_query():
    if sys.platform != 'darwin':
        raise OSError('proc_pidinfo thread observations require macOS')
    library = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    query = library.proc_pidinfo
    query.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
    query.restype = ctypes.c_int
    return query


def process_threads(pid, identity, *, deadline, limit=64):
    """Keep partial rows explicit; never join a reused PID to the prior process."""
    started = time.monotonic_ns()
    result = dict(pid=pid, api='proc_pidinfo', list_flavor=28, info_flavor=15,
                  started_monotonic_ns=started, status='unavailable', threads=[], limit=limit,
                  expected_identity=identity, identity_verified=False)
    # Check the budget between native calls; an individual syscall cannot be preempted.
    until = min(deadline, time.monotonic() + .02)
    try:
        if (type(pid) is not int or not 0 < pid < 2**31 or not isinstance(identity, dict)
                or identity.get('pid') != pid or not identity.get('start_abstime')
                or type(limit) is not int or not 1 <= limit <= 64):
            raise ValueError('Expected an identified process and a thread limit from 1 to 64')
        query = thread_query()

        def read(flavor, thread_id, buffer):
            if time.monotonic() >= until:
                raise TimeoutError('Thread observation budget exhausted')
            ctypes.set_errno(0)
            count = query(pid, flavor, thread_id, ctypes.byref(buffer), ctypes.sizeof(buffer))
            if count <= 0:
                error = ctypes.get_errno()
                raise OSError(error, os.strerror(error) if error else 'Empty thread observation')
            return count

        # Flavor 28 enumerates kernel thread IDs. Flavor 6 instead returns pthread
        # addresses, which can be reused and must not identify counter histories.
        ids = (ctypes.c_uint64 * (limit + 1))()
        count = read(28, 0, ids)
        if count > ctypes.sizeof(ids) or count % ctypes.sizeof(ctypes.c_uint64):
            raise ValueError('Invalid thread enumeration byte count')
        selected = list(ids[:count // ctypes.sizeof(ctypes.c_uint64)])
        if not all(selected) or len(set(selected)) != len(selected):
            raise ValueError('Invalid or duplicate kernel thread IDs')
        result.update(enumerated=len(selected), truncated=len(selected) > limit, status='partial')
        for thread_id in selected[:limit]:
            row = dict(thread_id=thread_id, started_monotonic_ns=time.monotonic_ns(), status='unavailable')
            try:
                info = ThreadInfo()
                if read(15, thread_id, info) != ctypes.sizeof(info):
                    raise ValueError('Incomplete thread information')
                row.update(status='available', raw={name: getattr(info, name) for name, _ in info._fields_[:-1]},
                           name=info.pth_name.decode(errors='replace'))
            except InterruptedError:
                raise
            except (OSError, ValueError) as error:
                row['error'] = str(error)
                if isinstance(error, OSError): row['errno'] = error.errno
            finally:
                row['finished_monotonic_ns'] = time.monotonic_ns()
                result['threads'].append(row)
            if time.monotonic() >= until:
                result['budget_exhausted'] = True
                break
        if time.monotonic() >= until:
            raise TimeoutError('Thread observation budget exhausted before identity recheck')
        after = process_resources(pid)
        result['identity_after'] = {k: after[k] for k in (
            'status', 'identity', 'error', 'started_monotonic_ns', 'finished_monotonic_ns') if k in after}
        if after.get('status') != 'available' or after.get('identity') != identity:
            result['status'] = 'identity_unverified'
            raise ValueError('Process identity changed or disappeared during thread observation')
        result['identity_verified'] = True
        if not result['truncated'] and all(row['status'] == 'available' for row in result['threads']):
            result['status'] = 'available'
    except InterruptedError:
        raise
    except (OSError, AttributeError, ValueError) as error:
        result['error'] = str(error)
        if isinstance(error, OSError): result['errno'] = error.errno
    finally:
        result['finished_monotonic_ns'] = time.monotonic_ns()
    return result
