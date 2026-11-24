"""Loaded only inside LLDB for an owned WebView process inspection."""
import json
from pathlib import Path

import lldb


def inspect(pid, destination):
    debugger = lldb.debugger
    debugger.SetAsync(False)
    target = debugger.CreateTarget('')
    process = None
    report = dict(pid=pid, attached=False, detached=False, modules=[], symbols=[], omitted=0)
    try:
        error = lldb.SBError()
        process = target.AttachToProcessWithID(debugger.GetListener(), pid, error)
        if error.Fail():
            report['error'] = error.GetCString()
            return
        report['attached'] = process.IsValid() and process.GetProcessID() == pid
        if not report['attached']:
            raise RuntimeError('Debugger process identity mismatch')
        for module in target.modules:
            name = module.GetFileSpec().GetFilename()
            if name not in ('WebCore', 'WebKit', 'AVFoundation', 'AVFCore', 'CoreMedia', 'MediaToolbox'):
                continue
            report['modules'].append(dict(name=name, uuid=module.GetUUIDString()))
            for symbol in module:
                label = symbol.GetName() or ''
                if not any(owner in label for owner in ('MediaPlayerPrivate', 'RemoteMediaPlayerProxy', 'AVPlayer')):
                    continue
                if not any(action in label for action in ('currentTime', 'timeChanged', 'TimeChanged', 'setRate', 'seekTo')):
                    continue
                if len(report['symbols']) >= 128:
                    report['omitted'] += 1
                    continue
                report['symbols'].append(dict(module=name, name=label,
                    address=symbol.GetStartAddress().GetLoadAddress(target)))
    except Exception as error:
        report['error'] = str(error)
    finally:
        if process and process.IsValid() and process.GetState() not in (
                lldb.eStateExited, lldb.eStateDetached, lldb.eStateInvalid):
            detached = process.Detach(False)
            report['detached'] = detached.Success()
            if detached.Fail():
                report['detach_error'] = detached.GetCString()
        Path(destination).write_text(json.dumps(report, indent=2) + '\n')
