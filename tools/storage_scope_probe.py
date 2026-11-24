#!/usr/bin/env python3
"""Opt-in document and session ownership checks for native storage capabilities."""
from cookie_clock_probe import run_probe

if __name__ == "__main__":
    raise SystemExit(run_probe("storage_scope_probe", "STORAGE_SCOPE_PROBE "))
