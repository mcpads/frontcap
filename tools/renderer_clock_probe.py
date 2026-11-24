#!/usr/bin/env python3
"""Observe renderer ownership and clock admission; no process termination or repair is performed."""
from cookie_clock_probe import run_probe

if __name__ == "__main__":
    raise SystemExit(run_probe("renderer_clock_probe", "RENDERER_CLOCK_PROBE "))
