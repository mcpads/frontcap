#!/usr/bin/env python3
"""Verify native IndexedDB initialization at the committed pre-script boundary."""
from cookie_clock_probe import run_probe

if __name__ == "__main__":
    raise SystemExit(run_probe("idb_initialization_probe", "IDB_INITIALIZATION_PROBE "))
