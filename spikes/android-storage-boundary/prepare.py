"""Stage a debuggable APK for an exact rename diagnostic; never changes the input product."""
import argparse
import hashlib
import json
from pathlib import Path
import shlex
import struct
import subprocess
import zipfile


def extract_native_libraries(data):
    result = bytearray(data)
    kind, header, total = struct.unpack_from('<HHI', data)
    if kind != 3 or total != len(data):
        raise ValueError('Expected one Android binary XML document')
    resources = []
    debug = []
    changed = []
    offset = header
    while offset < total:
        kind, header_size, size = struct.unpack_from('<HHI', data, offset)
        if size < header_size or header_size < 8 or offset + size > total:
            raise ValueError('Invalid binary XML chunk')
        if kind == 0x180:
            resources = list(struct.unpack_from('<' + 'I' * ((size-header_size)//4), data, offset+header_size))
        if kind == 0x102:
            extension = offset + header_size
            attribute_start, attribute_size, count = struct.unpack_from('<HHH', data, extension+8)
            if attribute_size != 20 or extension+attribute_start+count*attribute_size > offset+size:
                raise ValueError('Unsupported attribute layout')
            for number in range(count):
                entry = extension + attribute_start + number*attribute_size
                name, raw, value_size, zero, value_type, value = struct.unpack_from('<IIHBBI', data, entry+4)
                resource = resources[name] if name < len(resources) else None
                if resource not in (0x0101000f, 0x010104ea):
                    continue
                if value_size != 8 or zero != 0 or value_type != 0x12 or raw != 0xffffffff:
                    raise ValueError('Expected typed manifest boolean without a raw string')
                if resource == 0x0101000f:
                    debug.append(value != 0)
                else:
                    changed.append(dict(offset=entry+16, before=value, after=0xffffffff))
                    struct.pack_into('<I', result, entry+16, 0xffffffff)
        offset += size
    if debug != [True] or len(changed) != 1:
        raise ValueError('One existing debuggable=true and extractNativeLibs attribute required')
    return bytes(result), changed


def prepare(apk, output, compiler, build_tools, keytool, paths):
    apk = apk.resolve(strict=True)
    output = output.resolve()
    if output.exists() or output == apk or len(set(paths)) != 3 or any(
            not path.startswith('/') or any(ord(c) < 32 for c in path) for path in paths):
        raise ValueError('New output and three distinct absolute guest paths required')
    original_sha = hashlib.sha256(apk.read_bytes()).hexdigest()
    output.mkdir(parents=True)
    source = Path(__file__).resolve().parent.parent/'ios-storage-boundary/rename-stop.c'
    commands = []
    def run(args):
        commands.append(list(map(str, args)))
        subprocess.run(args, check=True, timeout=60)
    run([compiler, '-shared', '-fPIC', '-Wall', '-Wextra', '-Werror', str(source), '-ldl', '-o', str(output/'libfrontcap_rename.so')])
    wrapper = '#!/system/bin/sh\n'
    for key, value in zip(['SOURCE','DESTINATION','MARKER'], paths):
        wrapper += 'export FC_RENAME_'+key+'='+shlex.quote(value)+'\n'
    wrapper += 'export LD_PRELOAD="$(dirname "$0")/libfrontcap_rename.so"\nexec "$@"\n'
    (output/'wrap.sh').write_text(wrapper)
    before = {}
    signatures = []
    with zipfile.ZipFile(apk) as original, zipfile.ZipFile(output/'unsigned.apk', 'w') as staged:
        if len(original.namelist()) != len(set(original.namelist())):
            raise ValueError('Duplicate APK entries')
        if any(n.endswith('/wrap.sh') or n.endswith('/libfrontcap_rename.so') for n in original.namelist()):
            raise ValueError('APK already contains diagnostic attachment')
        if not any(n.startswith('lib/arm64-v8a/') for n in original.namelist()):
            raise ValueError('arm64 APK required')
        for info in original.infolist():
            value = original.read(info.filename)
            before[info.filename] = hashlib.sha256(value).hexdigest()
            upper = info.filename.upper()
            if upper.startswith('META-INF/') and (upper.endswith(('.RSA','.DSA','.EC','.SF')) or upper=='META-INF/MANIFEST.MF'):
                signatures.append(info.filename)
                continue
            if info.filename == 'AndroidManifest.xml':
                value, edits = extract_native_libraries(value)
            staged.writestr(info, value)
        staged.write(output/'libfrontcap_rename.so', 'lib/arm64-v8a/libfrontcap_rename.so')
        staged.write(output/'wrap.sh', 'lib/arm64-v8a/wrap.sh')
    run([build_tools/'zipalign','-P','16','-f','4',output/'unsigned.apk',output/'prepared.apk'])
    run([keytool,'-genkeypair','-keystore',output/'fixture.keystore','-alias','fixture','-keyalg','RSA','-keysize','2048','-validity','30','-storepass','android','-keypass','android','-dname','CN=Owned Diagnostic Fixture'])
    run([build_tools/'apksigner','sign','--v1-signing-enabled','false','--ks',output/'fixture.keystore','--ks-pass','pass:android',output/'prepared.apk'])
    run([build_tools/'apksigner','verify',output/'prepared.apk'])
    run([build_tools/'zipalign','-c','-P','16','4',output/'prepared.apk'])
    with zipfile.ZipFile(output/'prepared.apk') as staged:
        after = {name:hashlib.sha256(staged.read(name)).hexdigest() for name in staged.namelist()}
    added = set(after)-set(before)
    allowed = {'lib/arm64-v8a/libfrontcap_rename.so','lib/arm64-v8a/wrap.sh'}
    assert added == allowed and set(before)-set(after) == set(signatures)
    assert all(after[name] == sha for name, sha in before.items() if name not in signatures and name != 'AndroidManifest.xml')
    assert hashlib.sha256(apk.read_bytes()).hexdigest() == original_sha
    report = dict(state='passed', original=str(apk), original_sha256=original_sha,
                  prepared_sha256=hashlib.sha256((output/'prepared.apk').read_bytes()).hexdigest(),
                  files_before=before, files_after=after, removed_signatures=signatures,
                  manifest_edits=edits, rename_boundary=list(paths), commands=commands,
                  preparation_sources={str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in [source,Path(__file__)]})
    (output/'preparation.json').write_text(json.dumps(report,indent=2)+'\n')
    # Intermediate archives are not evidence inputs after final entry/hash/signature verification.
    (output/'unsigned.apk').unlink()
    print(json.dumps({key: report[key] for key in ['state','original_sha256','prepared_sha256']}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ['apk','output','compiler','build_tools','keytool']:
        parser.add_argument(name,type=Path)
    parser.add_argument('--rename-boundary',nargs=3,required=True)
    args = parser.parse_args()
    prepare(args.apk,args.output,args.compiler,args.build_tools,args.keytool,args.rename_boundary)
