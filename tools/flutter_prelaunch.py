"""Live prelaunch barrier for the owned Flutter SDK worker.

The Rust controller owns recipe verification and file application. This bridge only
executes commands on the worker's already-owned device and waits for its completion
receipt. It neither acquires a device nor launches an app. Callers must let every
exception escape into worker retirement, never continue to app.start after failure.
"""
import json
import queue
import time
import uuid

from native_host_process import WorkerInterrupted, write_json


def wait_for_initial_data(controls, directory, installation, deadline, command, verify):
    """Return only after a live acknowledgement and unchanged installation proof.

    `command(arguments, directory, timeout)` is bound by the owner to its device;
    it must record execution and return bytes. `verify()` must prove the app is
    still unstarted and return the same installation identity. EOF interrupts this
    function through the worker's existing control-reader signal handler.
    """
    directory.mkdir(exist_ok=False)
    # Detach the comparison from a mutable caller-owned dictionary.
    installation = json.loads(json.dumps(installation))
    identity = dict(token=str(uuid.uuid4()), installation=installation)
    number = 0

    def remaining():
        value = deadline-time.monotonic()
        if value <= 0:
            raise TimeoutError('Flutter initial-data deadline reached')
        return value

    remaining()
    if verify() != installation:
        raise ValueError('Flutter initial-data installation changed before preparation')
    write_json(directory/'ready.json', identity)
    while True:
        try:
            notification = controls.get(timeout=remaining())
        except queue.Empty as error:
            raise TimeoutError('Flutter initial-data acknowledgement missing') from error
        remaining()
        if notification == {'command': 'stop'}:
            raise WorkerInterrupted('Flutter stopped before initial-data completion')
        if (isinstance(notification, dict) and type(notification.get('number')) is int
                and notification == dict(command='initial_data_applied', token=identity['token'], number=number)):
            receipt_path = directory/'applied.json'
            with receipt_path.open('rb') as stream:
                raw = stream.read(65537)
            if len(raw) > 65536:
                raise ValueError('Flutter initial-data receipt exceeds bound')
            receipt = json.loads(raw)
            if (not isinstance(receipt, dict) or type(receipt.get('number')) is not int
                    or receipt != dict(identity, number=number)):
                raise ValueError('Flutter initial-data receipt differs from live preparation')
            if verify() != installation:
                raise ValueError('Flutter initial-data installation changed after preparation')
            remaining()
            write_json(directory/'completed.json', receipt)
            return
        if (not isinstance(notification, dict)
                or set(notification) != {'command', 'token', 'number'}
                or notification['command'] != 'initial_data_command'
                or notification['token'] != identity['token']
                or type(notification['number']) is not int
                or notification['number'] != number+1):
            raise ValueError('Invalid Flutter initial-data control')
        number += 1
        destination = directory/str(number)
        try:
            with (destination/'request.json').open('rb') as stream:
                raw = stream.read(65537)
            if len(raw) > 65536:
                raise ValueError('Flutter initial-data request exceeds bound')
            request = json.loads(raw)
            if (not isinstance(request, dict)
                    or set(request) != {'number', 'arguments', 'timeout_ms'}
                    or type(request['number']) is not int or request['number'] != number
                    or type(request['timeout_ms']) is not int
                    or not 1 <= request['timeout_ms'] <= 120000
                    or not isinstance(request['arguments'], list)
                    or not 1 <= len(request['arguments']) <= 64
                    or any(not isinstance(arg, str) or '\0' in arg for arg in request['arguments'])):
                raise ValueError('Invalid Flutter initial-data SDK request')
            result = command(request['arguments'], destination,
                             min(remaining(), request['timeout_ms']/1000))
            remaining()
            if not isinstance(result, bytes) or len(result) > 4*1024*1024:
                raise ValueError('Flutter initial-data SDK response exceeds bound')
            (destination/'command.log').write_bytes(result)
            write_json(destination/'reply.json', dict(number=number, error=None))
        except BaseException as error:
            # A command error is terminal even if its effect may have happened.
            # Do not accept another command or a later completion acknowledgement.
            if destination.is_dir():
                write_json(destination/'reply.json', dict(number=number, error=f'{type(error).__name__}: {error}'))
            raise
