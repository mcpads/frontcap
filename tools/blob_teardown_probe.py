#!/usr/bin/env python3
"""Opt-in frame/worker teardown while native Blob finalization is held."""
from cookie_clock_probe import run_probe

if __name__ == "__main__":
    raise SystemExit(run_probe("blob_teardown_probe", "BLOB_TEARDOWN_PROBE "))
