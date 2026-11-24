"""Observe ordinary fixture presentation within one budget, retaining every read-only attempt."""
import hashlib
import json
from pathlib import Path
import re
import shutil
import time
import xml.etree.ElementTree as ET

from native_android_input import focused_component
from native_clock_control import atomic_json


COMPONENT = 'dev.frontcap.s18.probe/dev.frontcap.s18.ProbeActivity'


def node(tree, description):
    nodes = [n for n in tree.iter('node') if n.get('content-desc') == description]
    if len(nodes) != 1:
        raise RuntimeError(f'Expected one {description}, got {len(nodes)}')
    return nodes[0]


def center(element):
    values = [int(n) for n in re.findall(r'\d+', element.attrib['bounds'])]
    if len(values) != 4 or values[2] <= values[0] or values[3] <= values[1]:
        raise RuntimeError('Invalid element bounds')
    return (values[0]+values[2])//2, (values[1]+values[3])//2


def read_image(path, point):
    from PIL import Image
    with Image.open(path) as image:
        image.load()
        valid = point is not None and 0 <= point[0] < image.width and 0 <= point[1] < image.height
        return list(image.size), list(image.convert('RGB').getpixel(point)) if valid else None


def observe(command, directory, name, count, *, timeout=20):
    if name not in ('app-before', 'app-after') or count not in (0, 1):
        raise ValueError('Ordinary fixture capture requires a declared phase and count')
    root = Path(directory)
    captures = root/(name+'-captures'); captures.mkdir()
    path = root/(name+'-presentation.json')
    started = time.monotonic(); deadline = started+timeout
    expected_pixel = [255, 0, 0] if count == 0 else [0, 255, 0]
    report = dict(status='observing', phase=name, count=count, component=COMPONENT,
                  expected_pixel=expected_pixel, timeout_s=timeout, started_monotonic_s=started, attempts=[])
    atomic_json(path, report)

    def run(*args):
        remaining = deadline-time.monotonic()
        if remaining <= 0:
            raise TimeoutError('App presentation deadline reached')
        return command(*args, timeout=remaining)

    try:
        while True:
            row = dict(index=len(report['attempts']), started_s=time.monotonic()-started, files={})
            report['attempts'].append(row); atomic_json(path, report)
            prefix = f"attempt-{row['index']:03d}"
            def save(suffix, data):
                file = captures/(prefix+suffix); file.write_bytes(data)
                row['files'][suffix] = dict(path=file.relative_to(root).as_posix(), sha256=hashlib.sha256(data).hexdigest())
                return file
            run('shell', 'uiautomator', 'dump', '/sdcard/frontcap-window.xml')
            raw = run('shell', 'cat', '/sdcard/frontcap-window.xml'); save('.xml', raw)
            tree = ET.fromstring(raw)
            point, text, app_nodes = None, None, False
            try:
                label, color = node(tree, 'native-count-'+str(count)), node(tree, 'native-color')
                point, text = center(color), label.get('text')
                app_nodes = all(element.get('package') == COMPONENT.split('/')[0] for element in (label, color))
            except RuntimeError as error:
                row['selection_error'] = str(error)
            png = save('.png', run('exec-out', 'screencap', '-p'))
            window = run('shell', 'dumpsys', 'window'); save('-window.txt', window)
            save('-crash.txt', run('logcat', '-b', 'crash', '-d'))
            size, pixel = read_image(png, point)
            focus = focused_component(window)
            matched = app_nodes and text == f'Count: {count}' and pixel == expected_pixel and focus == COMPONENT
            row.update(finished_s=time.monotonic()-started, point=point, text=text, app_nodes=app_nodes,
                       size=size, pixel=pixel, focus=focus, matched=matched)
            atomic_json(path, report)
            if time.monotonic() > deadline:
                raise TimeoutError('App presentation deadline reached')
            if matched:
                for suffix, entry in row['files'].items():
                    shutil.copyfile(root/entry['path'], root/(name+suffix))
                report.update(status='ready', selected=row['index'], finished_s=time.monotonic()-started)
                if report['finished_s'] > timeout:
                    raise TimeoutError('App presentation publication exceeded deadline')
                atomic_json(path, report)
                return tree, dict(phase=name, count=count, text=text, pixel=pixel, size=size)
            time.sleep(min(.02, max(0, deadline-time.monotonic())))
    except BaseException as error:
        report.update(status='failed', finished_s=time.monotonic()-started, error=f'{type(error).__name__}: {error}')
        atomic_json(path, report)
        raise


def check_presentation(directory, observation, record):
    """Validate attempt identity, matching state/focus, image binding and the original budget."""
    root = Path(directory); name = observation['phase']
    if name not in ('app-before', 'app-after'):
        raise ValueError('Unknown ordinary capture phase')
    count = 0 if name == 'app-before' else 1
    expected = [255, 0, 0] if count == 0 else [0, 255, 0]
    report = json.loads((root/(name+'-presentation.json')).read_text())
    attempts = report['attempts']
    valid = (report['status'] == 'ready' and report['phase'] == name and report['count'] == count
             and report['component'] == COMPONENT and report['expected_pixel'] == expected
             and report['timeout_s'] == 20 and 0 <= report['finished_s'] <= 20
             and bool(attempts) and report['selected'] == len(attempts)-1)
    previous = 0
    for index, row in enumerate(attempts):
        valid &= row['index'] == index and previous <= row['started_s'] <= row['finished_s'] <= report['finished_s']
        previous = row['finished_s']
        for suffix in ('.xml', '.png', '-window.txt', '-crash.txt'):
            entry = row['files'][suffix]
            relative = f'{name}-captures/attempt-{index:03d}'+suffix
            valid &= entry['path'] == relative and hashlib.sha256((root/relative).read_bytes()).hexdigest() == entry['sha256']
            if index == len(attempts)-1:
                valid &= (root/relative).read_bytes() == (root/(name+suffix)).read_bytes()
        prefix = root/(f'{name}-captures/attempt-{index:03d}')
        tree = ET.fromstring(Path(str(prefix)+'.xml').read_bytes())
        focus = focused_component(Path(str(prefix)+'-window.txt').read_bytes())
        point, text, app_nodes = None, None, False
        try:
            label, color = node(tree, 'native-count-'+str(count)), node(tree, 'native-color')
            point, text = list(center(color)), label.get('text')
            app_nodes = all(element.get('package') == COMPONENT.split('/')[0] for element in (label, color))
        except RuntimeError:
            pass
        matched = app_nodes and text == f'Count: {count}' and row['pixel'] == expected and focus == COMPONENT
        valid &= (row['point'] == point and row['text'] == text and row['app_nodes'] == app_nodes
                  and row['focus'] == focus and row['matched'] is matched
                  and matched is (index == len(attempts)-1) and len(row['size']) == 2 and min(row['size']) > 0)
    if attempts:
        selected = attempts[-1]
        valid &= all(observation[key] == selected[key] for key in ('text', 'pixel', 'size'))
    record(name+'.presentation', valid, report)
