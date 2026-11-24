"""Shared input receipt, application-clock and raw capture checks."""
import re
import struct
import xml.etree.ElementTree as ET

from native_android_input import InputRequest, validate_reply


def app_clocks_match(value, count, millis):
    return (value['count'] == count and value['fired'] == 0 and value['manual'] is True
            and value['host_render'] is True and value['nano_ns'] == millis*1000000
            and all(value[k] == millis for k in ('uptime_ms', 'elapsed_ms', 'wall_ms')))


def input_receipt_matches(value, request):
    try:
        validate_reply(value, InputRequest(**request))
        return True
    except (RuntimeError, ValueError, KeyError, TypeError):
        return False


def capture_matches(root, value, count, millis, pixel):
    tree = ET.parse(root/(value['phase']+'.xml'))
    labels = [n.get('text') for n in tree.iter('node') if n.get('content-desc') == 'native-count-'+str(count)]
    times = [n.get('text') for n in tree.iter('node') if n.get('content-desc') == 'native-time']
    colors = [n for n in tree.iter('node') if n.get('content-desc') == 'native-color']
    if len(colors) != 1:
        return False
    bounds = list(map(int, re.findall(r'\d+', colors[0].get('bounds'))))
    png = (root/(value['phase']+'.png')).read_bytes()
    return (labels == ['Count: '+str(count)] and times == [f'Time: {millis} Timer: 0']
            and len(bounds) == 4 and png[:8] == b'\x89PNG\r\n\x1a\n'
            and list(struct.unpack('>II', png[16:24])) == value['size']
            and value['pixel'] == pixel and value['text'] == labels[0])
