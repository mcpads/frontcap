"""Host effects under the live Flutter SDK worker; no saved state grants a device lease."""
import hashlib
import json
from pathlib import Path
import plistlib
import re
import shlex
import struct


def digest(path):
    with path.open('rb') as stream:
        value = hashlib.sha256()
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
        return value.hexdigest()


def app_name(value, platform):
    pattern = r'[A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)+' if platform == 'android' else r'[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+'
    if not isinstance(value, str) or len(value) > 255 or not re.fullmatch(pattern, value):
        raise ValueError('Invalid Flutter installed app identity')
    return value


def read_plist(path):
    if path.stat().st_size > 4 * 1024 * 1024:
        raise ValueError('Flutter app metadata exceeds bound')
    return plistlib.loads(path.read_bytes())


def bundle_inventory(root):
    """The Runner executable alone does not identify compiled Dart code or Flutter assets."""
    if not root.is_absolute() or root.is_symlink() or not root.is_dir():
        raise ValueError('Invalid Flutter bundle root')
    files = {}; directories = []
    for path in sorted(root.rglob('*')):
        name = str(path.relative_to(root))
        if path.is_symlink(): raise ValueError('Linked Flutter bundle product')
        if path.is_dir(): directories.append(name)
        elif path.is_file():
            files[name] = dict(bytes=path.stat().st_size, sha256=digest(path),
                               executable=bool(path.stat().st_mode & 0o111))
        else: raise ValueError('Special Flutter bundle product')
    return dict(files=files, directories=directories)


def bundle_difference(expected, actual):
    before, after = expected['files'], actual['files']
    return dict(missing=sorted(set(before)-set(after)), unexpected=sorted(set(after)-set(before)),
                changed={name: dict(expected=before[name], actual=after[name])
                         for name in sorted(set(before)&set(after)) if before[name] != after[name]},
                missing_directories=sorted(set(expected['directories'])-set(actual['directories'])),
                unexpected_directories=sorted(set(actual['directories'])-set(expected['directories'])))


def is_shared_library(path):
    """Recognize MH_DYLIB slices; installing a library may clear its Unix execute bit."""
    def library_header(header):
        endian = {b'\xce\xfa\xed\xfe': '<', b'\xcf\xfa\xed\xfe': '<',
                  b'\xfe\xed\xfa\xce': '>', b'\xfe\xed\xfa\xcf': '>'}.get(header[:4])
        return len(header) == 16 and endian is not None and struct.unpack(endian+'I', header[12:16])[0] == 6
    with path.open('rb') as stream:
        header = stream.read(16)
        if library_header(header): return True
        fat = {b'\xca\xfe\xba\xbe': ('>', 20, 'I'), b'\xbe\xba\xfe\xca': ('<', 20, 'I'),
               b'\xca\xfe\xba\xbf': ('>', 32, 'Q'), b'\xbf\xba\xfe\xca': ('<', 32, 'Q')}.get(header[:4])
        if fat is None or len(header) != 16: return False
        endian, size, number = fat
        count = struct.unpack(endian+'I', header[4:8])[0]
        if not 1 <= count <= 32: return False
        for index in range(count):
            stream.seek(8+index*size);entry = stream.read(size)
            if len(entry) != size: return False
            offset = struct.unpack(endian+number, entry[8:8+struct.calcsize(number)])[0]
            stream.seek(offset)
            if not library_header(stream.read(16)): return False
        return True


def installed_bundle_difference(product, expected, actual):
    normalized = dict(files={name: dict(value) for name, value in actual['files'].items()}, directories=actual['directories'])
    modes = {}
    for name in expected['files'].keys() & actual['files'].keys():
        before, after = expected['files'][name], actual['files'][name]
        if (before['executable'] and not after['executable'] and before['sha256'] == after['sha256']
                and before['bytes'] == after['bytes'] and is_shared_library(product/name)):
            normalized['files'][name]['executable'] = True
            modes[name] = dict(product_executable=True, installed_executable=False)
    return bundle_difference(expected, normalized), modes


def installed_target(platform, project, device, adb, command):
    """Bind standard Flutter debug products to this worker's installed application.

    A custom build layout remains usable without external-entry capability. A present but
    inconsistent product fails acquisition instead of granting a different app's authority.
    """
    if platform == 'android':
        metadata = project/'build/app/outputs/apk/debug/output-metadata.json'
        if not metadata.exists():
            return None, dict(unavailable='No standard Flutter debug APK metadata')
        if metadata.stat().st_size > 65536:
            raise ValueError('Flutter APK metadata exceeds bound')
        info = json.loads(metadata.read_text())
        app = app_name(info['applicationId'], platform)
        elements = info['elements']
        if len(elements) != 1 or elements[0].get('filters') or info.get('artifactType', {}).get('type') != 'APK':
            raise ValueError('External entry requires one installed Flutter APK')
        name = elements[0]['outputFile']
        if not isinstance(name, str) or Path(name).name != name or not name.endswith('.apk'):
            raise ValueError('Invalid Flutter APK product path')
        product = metadata.parent/name
        package_paths = command('package-path', [str(adb), '-s', device, 'shell', 'pm', 'path', app]).decode().splitlines()
        if len(package_paths) != 1 or not package_paths[0].startswith('package:/'):
            raise ValueError('Installed Flutter package is missing or split')
        installed = package_paths[0][len('package:'):]
        hashed = command('package-hash', [str(adb), '-s', device, 'shell', 'sha256sum', shlex.quote(installed)]).decode().split()
        expected = digest(product)
        if not hashed or hashed[0] != expected:
            raise ValueError('Installed Flutter APK differs from built product')
        return dict(platform=platform, serial=device, package=app), dict(
            product=str(product), sha256=expected, installed=installed, metadata_sha256=digest(metadata))
    products = list((project/'build/ios/iphonesimulator').glob('*.app'))
    if not products:
        return None, dict(unavailable='No standard Flutter Simulator app product')
    if len(products) != 1:
        raise ValueError('Ambiguous Flutter Simulator product')
    product = products[0]
    info = read_plist(product/'Info.plist')
    app = app_name(info['CFBundleIdentifier'], platform)
    installed = Path(command('app-container', ['xcrun', 'simctl', 'get_app_container', device, app, 'app']).decode().strip())
    if not installed.is_absolute() or installed.suffix != '.app':
        raise ValueError('Invalid installed Flutter app path')
    actual = read_plist(installed/'Info.plist')
    executable = info['CFBundleExecutable']
    if (not isinstance(executable, str) or Path(executable).name != executable
            or actual.get('CFBundleIdentifier') != app or actual.get('CFBundleExecutable') != executable):
        raise ValueError('Installed Flutter bundle differs from product')
    expected = digest(product/executable)
    if digest(installed/executable) != expected:
        raise ValueError('Installed Flutter executable differs from product')
    inventory = bundle_inventory(product)
    actual_inventory = bundle_inventory(installed)
    difference, modes = installed_bundle_difference(product, inventory, actual_inventory)
    if any(difference.values()):
        raise ValueError('Installed Flutter bundle differs: '+json.dumps(difference))
    return dict(platform=platform, simulator=device.lower(), bundle=app), dict(
        product=str(product), executable_sha256=expected, installed=str(installed),
        bundle_inventory=inventory,
        installed_library_modes=modes,
        product_plist_sha256=digest(product/'Info.plist'), installed_plist_sha256=digest(installed/'Info.plist'))


def host_arguments(operation, ready, adb):
    target = ready.get('external_url_target')
    if target is None:
        raise ValueError('Flutter host entry was not acquired')
    if operation['operation'] == 'installed_apps' and set(operation) == {'operation'} and target['platform'] == 'ios':
        return ['xcrun', 'simctl', 'listapps', target['simulator']]
    if operation.get('operation') != 'open_url' or set(operation) != {'operation', 'request'}:
        raise ValueError('Unsupported Flutter host operation')
    request = operation['request']
    if (set(request) != {'target', 'generation', 'url'} or request['target'] != target
            or request['generation'] != ready['owner']['generation']):
        raise ValueError('Flutter host request differs from live owner')
    url = request['url']
    if (not isinstance(url, str) or len(url.encode()) > 8192
            or not re.fullmatch(r'[A-Za-z][A-Za-z0-9+.-]{0,63}:.+', url)
            or any(c.isspace() or ord(c) < 32 or 127 <= ord(c) <= 159 for c in url)):
        raise ValueError('Invalid external URL transport')
    if target['platform'] == 'android':
        return [str(adb), '-s', target['serial'], 'shell', 'am', 'start', '-W', '-a',
                'android.intent.action.VIEW', '-d', shlex.quote(url), '-p', target['package']]
    return ['xcrun', 'simctl', 'openurl', target['simulator'], url]
