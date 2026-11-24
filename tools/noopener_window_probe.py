#!/usr/bin/env python3
"""Run the opt-in probe of windows opened without an opener (P06) outside the default test suite."""
from cookie_clock_probe import run_probe


if __name__ == "__main__":
    raise SystemExit(run_probe("noopener_window_probe", "NOOPENER_PROBE "))
