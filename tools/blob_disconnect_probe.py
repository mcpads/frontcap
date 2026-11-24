#!/usr/bin/env python3
"""Opt-in A/B probe of a real BlobRegistry client-pipe disconnect after input EOF."""
from cookie_clock_probe import run_probe

if __name__ == "__main__":
    raise SystemExit(run_probe("blob_disconnect_probe", "BLOB_DISCONNECT_PROBE "))
