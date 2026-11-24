"""Spawn an owned Darwin child suspended before user code, keeping its existing process group."""
import ctypes
import os
from pathlib import Path
import subprocess
import sys
import time


# Darwin <sys/spawn.h>; opaque attr/actions types are void* in <spawn.h>.
POSIX_SPAWN_START_SUSPENDED = 0x0080


def spawn_suspended(command, stdout_fd):
    if sys.platform != 'darwin':
        raise RuntimeError('Suspended process launch requires Darwin')
    if not command or not Path(command[0]).is_absolute() or any('\0' in part for part in command):
        raise ValueError('Expected an absolute executable and NUL-free arguments')
    library = ctypes.CDLL('/usr/lib/libSystem.B.dylib')
    opaque = ctypes.POINTER(ctypes.c_void_p)
    strings = ctypes.POINTER(ctypes.c_char_p)
    signatures = {
        'posix_spawnattr_init': [opaque], 'posix_spawnattr_destroy': [opaque],
        'posix_spawnattr_setflags': [opaque, ctypes.c_short],
        'posix_spawn_file_actions_init': [opaque], 'posix_spawn_file_actions_destroy': [opaque],
        'posix_spawn_file_actions_adddup2': [opaque, ctypes.c_int, ctypes.c_int],
        'posix_spawn_file_actions_addclose': [opaque, ctypes.c_int],
        'posix_spawn': [ctypes.POINTER(ctypes.c_int), ctypes.c_char_p, opaque, opaque, strings, strings],
    }
    for name, arguments in signatures.items():
        function = getattr(library, name)
        function.argtypes, function.restype = arguments, ctypes.c_int
    def checked(name, *args):
        error = getattr(library, name)(*args)
        if error:
            raise OSError(error, os.strerror(error), command[0])
    attr, actions = ctypes.c_void_p(), ctypes.c_void_p()
    checked('posix_spawnattr_init', ctypes.byref(attr))
    try:
        checked('posix_spawnattr_setflags', ctypes.byref(attr), POSIX_SPAWN_START_SUSPENDED)
        checked('posix_spawn_file_actions_init', ctypes.byref(actions))
        try:
            checked('posix_spawn_file_actions_adddup2', ctypes.byref(actions), stdout_fd, 1)
            if stdout_fd != 1:
                checked('posix_spawn_file_actions_addclose', ctypes.byref(actions), stdout_fd)
            argv = (ctypes.c_char_p*(len(command)+1))(*[os.fsencode(part) for part in command], None)
            environment = [os.fsencode(key+'='+value) for key,value in os.environ.items()]
            envp = (ctypes.c_char_p*(len(environment)+1))(*environment, None)
            pid = ctypes.c_int()
            checked('posix_spawn', ctypes.byref(pid), argv[0], ctypes.byref(actions), ctypes.byref(attr), argv, envp)
            return pid.value
        finally:
            library.posix_spawn_file_actions_destroy(ctypes.byref(actions))
    finally:
        library.posix_spawnattr_destroy(ctypes.byref(attr))


def wait_child(pid, timeout):
    """Reap only this direct child; stopped state does not count as termination."""
    deadline = time.monotonic()+timeout
    while True:
        waited, status = os.waitpid(pid, os.WNOHANG)
        if waited:
            return os.waitstatus_to_exitcode(status)
        if time.monotonic() >= deadline:
            raise subprocess.TimeoutExpired(['owned-child', str(pid)], timeout)
        time.sleep(.01)
