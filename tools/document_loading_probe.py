#!/usr/bin/env python3
"""Opt-in native document loading snapshots at stopped boundaries."""
from cookie_clock_probe import run_probe

if __name__ == "__main__":
    raise SystemExit(run_probe("document_loading_probe", "DOCUMENT_LOADING_PROBE "))
