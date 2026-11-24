"""Read cumulative resources for an owned macOS process without spawning a command."""
import ctypes
from functools import lru_cache
import os
import sys
import time


class RusageInfo(ctypes.Structure):
    # SDK sys/resource.h, rusage_info_v4. The flavor fixes the returned buffer size.
    _fields_ = [('ri_uuid', ctypes.c_uint8 * 16)] + [(name, ctypes.c_uint64) for name in (
        'ri_user_time', 'ri_system_time', 'ri_pkg_idle_wkups', 'ri_interrupt_wkups',
        'ri_pageins', 'ri_wired_size', 'ri_resident_size', 'ri_phys_footprint',
        'ri_proc_start_abstime', 'ri_proc_exit_abstime', 'ri_child_user_time',
        'ri_child_system_time', 'ri_child_pkg_idle_wkups', 'ri_child_interrupt_wkups',
        'ri_child_pageins', 'ri_child_elapsed_abstime', 'ri_diskio_bytesread',
        'ri_diskio_byteswritten', 'ri_cpu_time_qos_default', 'ri_cpu_time_qos_maintenance',
        'ri_cpu_time_qos_background', 'ri_cpu_time_qos_utility', 'ri_cpu_time_qos_legacy',
        'ri_cpu_time_qos_user_initiated', 'ri_cpu_time_qos_user_interactive',
        'ri_billed_system_time', 'ri_serviced_system_time', 'ri_logical_writes',
        'ri_lifetime_max_phys_footprint', 'ri_instructions', 'ri_cycles',
        'ri_billed_energy', 'ri_serviced_energy', 'ri_interval_max_phys_footprint',
        'ri_runnable_time')]


class MachTimebase(ctypes.Structure):
    _fields_ = [('numer', ctypes.c_uint32), ('denom', ctypes.c_uint32)]


@lru_cache(maxsize=1)
def resource_query():
    if sys.platform != 'darwin':
        raise OSError('proc_pid_rusage requires macOS')
    library = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    query = library.proc_pid_rusage
    query.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    query.restype = ctypes.c_int
    system = ctypes.CDLL('/usr/lib/libSystem.B.dylib')
    clock = system.mach_timebase_info
    clock.argtypes = [ctypes.POINTER(MachTimebase)]
    clock.restype = ctypes.c_int
    scale = MachTimebase()
    code = clock(ctypes.byref(scale))
    if code or not scale.numer or not scale.denom:
        raise OSError(f'mach_timebase_info unavailable: {code}')
    return query, scale.numer, scale.denom


def process_resources(pid):
    """Unavailable observations contain no counters; timestamps bracket the query."""
    result = dict(pid=pid, api='proc_pid_rusage', flavor=4, status='unavailable',
                  started_monotonic_ns=time.monotonic_ns())
    try:
        if type(pid) is not int or not 0 < pid < 2**31:
            raise ValueError('Expected a positive process ID')
        query, numer, denom = resource_query()
        info = RusageInfo()
        ctypes.set_errno(0)
        code = query(pid, 4, ctypes.byref(info))
        if code:
            error = ctypes.get_errno()
            raise OSError(error, os.strerror(error))
        if not info.ri_proc_start_abstime:
            raise ValueError('Process start identity unavailable')
        raw = {name: getattr(info, name) for name in (
            'ri_user_time', 'ri_system_time', 'ri_runnable_time', 'ri_proc_exit_abstime',
            'ri_pageins', 'ri_resident_size', 'ri_phys_footprint',
            'ri_diskio_bytesread', 'ri_diskio_byteswritten', 'ri_logical_writes',
            'ri_pkg_idle_wkups', 'ri_interrupt_wkups')}
        # XNU fills CPU/runnable totals in Mach units, not nanoseconds. Runnable
        # includes running time and aggregates threads; it is not a wait reason.
        result.update(status='available',
                      identity=dict(pid=pid, start_abstime=info.ri_proc_start_abstime,
                                    executable_uuid=bytes(info.ri_uuid).hex()),
                      raw=raw, timebase=dict(numer=numer, denom=denom),
                      cpu_user_ns=info.ri_user_time * numer // denom,
                      cpu_system_ns=info.ri_system_time * numer // denom,
                      runnable_ns=info.ri_runnable_time * numer // denom)
    except InterruptedError:
        raise
    except (OSError, AttributeError, ValueError) as error:
        result['error'] = str(error)
        if isinstance(error, OSError) and error.errno is not None:
            result['errno'] = error.errno
    finally:
        result['finished_monotonic_ns'] = time.monotonic_ns()
    return result
