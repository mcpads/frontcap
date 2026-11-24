"""Bounded JSON framing shared by Simulator clock and XCTest control endpoints."""
from dataclasses import asdict
import json
import socket
import struct
import time


def exchange(endpoint, request, *, timeout=10, max_response_bytes=8192):
    """One bounded attempt; failure details never authorize a retry or prove an effect."""
    if type(max_response_bytes) is not int or not 1 <= max_response_bytes <= 1024 * 1024:
        raise ValueError('Invalid native response byte budget')
    data = json.dumps(asdict(request)).encode()
    frame = struct.pack('!I', len(data)) + data
    started = time.monotonic()
    deadline = started + timeout
    observation = dict(phase='connect', started_monotonic_s=started, phase_started_monotonic_s=started,
                       deadline_monotonic_s=deadline, timeout_s=timeout, port=endpoint['port'],
                       connected=False, send_started=False, send_completed=False,
                       request_bytes=len(frame), response_header_bytes=0, response_body_bytes=0,
                       expected_response_bytes=None)

    def enter_phase(name):
        observation.update(phase=name, phase_started_monotonic_s=time.monotonic())

    def remaining():
        value = deadline-time.monotonic()
        if value <= 0:
            raise TimeoutError('Native response deadline expired')
        return value

    try:
        with socket.create_connection(('127.0.0.1', endpoint['port']), timeout=remaining()) as stream:
            observation['connected'] = True
            enter_phase('send')
            stream.settimeout(remaining())
            observation['send_started'] = True
            stream.sendall(frame)
            observation['send_completed'] = True
            remaining()

            def receive(length, counter):
                chunks = bytearray()
                while len(chunks) < length:
                    stream.settimeout(remaining())
                    chunk = stream.recv(length - len(chunks))
                    observation[counter] += len(chunk)
                    remaining()
                    if not chunk:
                        raise EOFError('Clock connection closed before its complete reply')
                    chunks.extend(chunk)
                return chunks

            enter_phase('header')
            length, = struct.unpack('!I', receive(4, 'response_header_bytes'))
            observation['expected_response_bytes'] = length
            if not 0 < length <= max_response_bytes:
                raise ValueError('Invalid clock frame length')
            enter_phase('body')
            body = receive(length, 'response_body_bytes')
            enter_phase('decode')
            reply = json.loads(body)
            remaining()
            enter_phase('close')
        return reply
    except BaseException as error:
        observation['failed_monotonic_s'] = time.monotonic()
        # Keep the original exception class/object and traceback. In particular,
        # sendall failure cannot tell us how many bytes or app effects occurred.
        try:
            error.native_transport_failure = observation
        except Exception:
            pass
        raise
