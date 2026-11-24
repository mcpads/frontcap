"""Emit explicit native-host launch configuration for an already verified staged app copy.

This reuses the prepared bytes; it does not rebuild, resign, install or launch an application.
"""
import argparse
import hashlib
import json
from pathlib import Path
import plistlib
import lief
from prepare import inventory


def emit(preparation, output):
    report=json.loads(preparation.read_text())
    original=Path(report['product']);staged=Path(report['staged'])
    if (not original.is_absolute() or not staged.is_absolute() or original==staged
            or inventory(original)!=report['source_files'] or inventory(staged)!=report['staged_files']):
        raise ValueError('Original or staged bytes differ from preparation')
    info=plistlib.loads((staged/'Info.plist').read_bytes())
    def bundle(root, hashes):
        return dict(directories=sorted(str(p.relative_to(root)) for p in root.rglob('*') if p.is_dir()),
            files={name:dict(sha256=sha,bytes=(root/name).stat().st_size,
                executable=bool((root/name).stat().st_mode&0o111)) for name,sha in hashes.items()})
    libraries=[]
    for name in report['staged_files']:
        path=staged/name
        if lief.is_macho(str(path)):
            parsed=lief.MachO.parse(path)
            if parsed is not None and parsed.size>0 and all(b.header.file_type==lief.MachO.Header.FILE_TYPE.DYLIB for b in parsed):
                libraries.append(name)
    library=staged/'FrontcapAttachment/native-input.dylib';script=staged/'FrontcapAttachment/timeline.js'
    def attachment(path):return dict(path=str(path),sha256=hashlib.sha256(path.read_bytes()).hexdigest())
    launch=dict(app=str(staged),bundle=info['CFBundleIdentifier'],environment={},library=attachment(library),
        script=attachment(script),mute_audio=True,page_registry=True,prepared=dict(original=str(original),
        original_inventory=bundle(original,report['source_files']),inventory=bundle(staged,report['staged_files']),
        executable=info['CFBundleExecutable'],shared_libraries=sorted(libraries)))
    with output.open('x') as stream:json.dump(launch,stream,indent=2);stream.write('\n')
    print(json.dumps(dict(output=str(output),shared_libraries=len(libraries),files=len(report['staged_files']))))


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('preparation',type=Path);parser.add_argument('output',type=Path)
    args=parser.parse_args();emit(args.preparation,args.output)
