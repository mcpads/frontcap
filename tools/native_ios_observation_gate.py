"""Diagnostic XCTest transport wrapper holding the first observe before loopback dispatch."""
from dataclasses import asdict
from pathlib import Path
import time

from native_ios_transport import exchange
from native_observation_gate import wait_for_release


def gated_exchange(directory, endpoint, request, *, timeout=20, transport=exchange):
    started = time.monotonic()
    if request.operation == 'observe':
        wait_for_release(directory, {'endpoint':endpoint, 'request':asdict(request)},
                         wait_seconds=min(5, timeout))
    remaining = timeout-(time.monotonic()-started)
    if remaining <= 0:
        raise TimeoutError('Observation transport deadline elapsed at capture gate')
    return transport(endpoint, request, timeout=remaining)


def main(directory):
    import native_ios_ui_bridge as bridge
    class GatedBridge(bridge.Bridge):
        def __init__(self, descriptor, output, *, timeout=20):
            super().__init__(descriptor, output,
                             transport=lambda endpoint, request, **options:
                             gated_exchange(directory, endpoint, request, **options),timeout=timeout)
    bridge.Bridge = GatedBridge
    bridge.main()


def configure(config, root):
    wrapper = root/'capture-ui.py'
    wrapper.write_text('import sys\n'
                       +'sys.path.insert(0, '+repr(str(Path(__file__).resolve().parent))+')\n'
                       +'from native_ios_observation_gate import main\n'
                       +'main('+repr(str(root/'capture-gate'))+')\n')
    return {**config, 'ui_script':str(wrapper)}
