#!/usr/bin/env python3
"""Process-level ADB substitute: count effects independently of bridge/client state."""
import json
import os
from pathlib import Path
import sys
import time

if os.environ.get('FRONTCAP_FAKE_NO_TARGET'): sys.exit(57)
path = Path(os.environ['FRONTCAP_FAKE_STATE'])
args = sys.argv[1:]
assert args[:2] == ['-s', 'emulator-5580']; args = args[2:]
if args == ['shell', 'pidof', 'org.example.app']: output = '42'
elif args == ['shell', 'dumpsys', 'window']:
    output = 'mCurrentFocus=Window{org.example.app/org.example.app.ClientActivity}'
else: raise AssertionError(args)
print(output)
