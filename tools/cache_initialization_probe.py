#!/usr/bin/env python3
"""Check native cache write/readback at the pre-script boundary on the storage-control candidate."""
from cookie_clock_probe import run_probe

if __name__ == "__main__":
    raise SystemExit(run_probe("cache_initialization_probe", "CACHE_INITIALIZATION_PROBE "))
