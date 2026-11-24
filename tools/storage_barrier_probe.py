#!/usr/bin/env python3
"""Observe storage callback serviceability without releasing the initial app boundary."""
from cookie_clock_probe import run_probe

if __name__ == "__main__":
    raise SystemExit(run_probe("storage_barrier_probe", "STORAGE_BARRIER_PROBE "))
