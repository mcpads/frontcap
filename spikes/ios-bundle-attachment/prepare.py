"""Opt-in arm64 Simulator packaging experiment; always stages a separate app copy.

Requires LIEF in an isolated Python environment and the iPhoneSimulator SDK. This is not yet
adapter acquisition or a general signing solution. Source and asset bytes stay local.
"""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import lief


def inventory(root):
    result = {}
    for p in root.rglob('*'):
        if p.is_symlink(): raise ValueError('Linked product input: '+str(p))
        if p.is_file(): result[str(p.relative_to(root))] = hashlib.sha256(p.read_bytes()).hexdigest()
        elif not p.is_dir(): raise ValueError('Special product input')
    return result


def prepare(app, library, script, output, rename_boundary=None):
    app, library, script = [p.resolve(strict=True) for p in (app, library, script)]
    output = output.resolve()
    if app.suffix != '.app' or output == app or output.is_relative_to(app):
        raise ValueError('Expected independent staging directory')
    info = plistlib.loads((app/'Info.plist').read_bytes())
    executable = info['CFBundleExecutable']
    if not isinstance(executable, str) or executable in ('', '.', '..') or Path(executable).name != executable:
        raise ValueError('Invalid main executable')
    if 'iPhoneSimulator' not in info.get('CFBundleSupportedPlatforms', []):
        raise ValueError('Simulator product required')
    if (app/'FrontcapAttachment').exists(): raise ValueError('Product already contains platform attachment')
    rename_source = Path(__file__).resolve().parent.parent/'ios-storage-boundary/rename-stop.c'
    rename_flags = []
    if rename_boundary is not None:
        if len(set(rename_boundary)) != 3 or any(
                not value or Path(value).is_absolute() or '..' in Path(value).parts
                or any(ord(char) < 32 for char in value) for value in rename_boundary):
            raise ValueError('Rename boundary requires three distinct relative container paths')
        for key, value in zip(['SOURCE', 'DESTINATION', 'MARKER'], rename_boundary):
            rename_flags.append('-DFC_RENAME_RELATIVE_'+key+'='+json.dumps(value))
        rename_flags.append(str(rename_source))
    before = inventory(app)
    output.mkdir(parents=True, exist_ok=False)
    staged = output/app.name
    shutil.copytree(app, staged)
    folder = staged/'FrontcapAttachment';folder.mkdir()
    shutil.copy2(library, folder/'native-input.dylib')
    shutil.copy2(script, folder/'timeline.js')
    # Keep the original universal executable; this explicitly scoped run uses an arm64 clone.
    parsed = lief.MachO.parse(staged/executable)
    binary = parsed.get(lief.MachO.Header.CPU_TYPE.ARM64)
    if binary is None: raise ValueError('Missing arm64 executable')
    def bindings(value):
        return Counter((b.symbol.name if b.has_symbol else None,
                        b.library.name if b.has_library else None,
                        b.library_ordinal,b.addend,b.weak_import) for b in value.bindings)
    original_bindings=bindings(binary)
    original_libraries=[item.name for item in binary.libraries]
    dependency = '@executable_path/FrontcapAttachment/loader.dylib'
    # Appending preserves existing library ordinals. Inserting before them can silently
    # retarget existing chained imports even when the rewritten image signs successfully.
    binary.add_library(dependency)
    binary.write(staged/executable)
    subprocess.run(['xcrun', '--sdk', 'iphonesimulator', 'clang', '-arch', 'arm64',
        '-mios-simulator-version-min=16.0', '-dynamiclib', '-Wall', '-Wextra', '-Werror',
        str(Path(__file__).with_name('loader.c')), *rename_flags, '-o', str(folder/'loader.dylib')], check=True)
    for item in [folder/'loader.dylib', folder/'native-input.dylib', staged]:
        subprocess.run(['codesign','--force','--sign','-', '--preserve-metadata=entitlements',str(item)],check=True)
    subprocess.run(['codesign','--verify','--deep','--strict',str(staged)],check=True)
    actual = lief.MachO.parse(staged/executable).at(0)
    if ([item.name for item in actual.libraries] != original_libraries+[dependency]
            or bindings(actual) != original_bindings):
        raise ValueError('Staged executable changed original symbol-library bindings')
    if inventory(app) != before: raise ValueError('Original product changed')
    after = inventory(staged)
    changed = sorted(p for p in before if after.get(p) != before[p])
    if any(p != executable and not p.startswith('_CodeSignature/') for p in changed):
        raise ValueError('Unexpected consumer file change: '+str(changed))
    report = dict(product=str(app), staged=str(staged), scope='arm64 iOS Simulator prerequisite',
        lief_version=lief.__version__, preserved_import_bindings=sum(original_bindings.values()), source_files=before, staged_files=after, changed=changed,
        library_sha256=hashlib.sha256(library.read_bytes()).hexdigest(),
        script_sha256=hashlib.sha256(script.read_bytes()).hexdigest(),
        preparation_files={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in
                           [Path(__file__),Path(__file__).with_name('loader.c'), *([rename_source] if rename_boundary else [])]})
    if rename_boundary is not None: report['rename_boundary'] = dict(zip(['source', 'destination', 'marker'], rename_boundary))
    (output/'preparation.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(dict(staged=str(staged),changed=changed,lief_version=lief.__version__)))


if __name__ == '__main__':
    p=argparse.ArgumentParser(description=__doc__)
    for name in ['app','library','script','output']:p.add_argument(name,type=Path)
    p.add_argument('--rename-boundary', nargs=3, metavar=('SOURCE', 'DESTINATION', 'MARKER'), help='Opt-in diagnostic: exact paths relative to the owned Simulator data container')
    a=p.parse_args();prepare(a.app,a.library,a.script,a.output,a.rename_boundary)
