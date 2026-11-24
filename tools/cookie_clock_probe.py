#!/usr/bin/env python3
"""Run the opt-in cookie clock diagnostic outside the default test suite.

The payload prints observations, not an expiry-conformance verdict. Its only assertion is that
waiting on the host does not advance the generation clock. Temporary Cargo test ownership is
exclusive; cleanup never overwrites or deletes a file another process changed.
"""
from pathlib import Path
import subprocess
import argparse


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", action="store_true", help="Inspect native setters at a future controlled epoch")
    native = parser.parse_args().native
    name = "native_cookie_clock_probe" if native else "cookie_clock_probe"
    return run_probe(name, "COOKIE_CLOCK_PROBE ")


def run_probe(name, marker):
    root = Path(__file__).resolve().parent.parent
    source = Path(__file__).with_name(name + ".rs").read_bytes()
    target = root / "crates/adapter-chromium/tests" / ("frontcap_" + name + ".rs")
    output = root / "target/browser-finish-checks" / (name.replace("_", "-") + ".log")
    output.parent.mkdir(parents=True, exist_ok=True)
    with target.open("xb") as file:
        file.write(source)
    try:
        with output.open("w") as log:
            result = subprocess.run(
                ["cargo", "test", "-p", "frontcap-adapter-chromium", "--test",
                 "frontcap_" + name, "--", "--nocapture"],
                cwd=root, stdout=log, stderr=subprocess.STDOUT,
            )
        print(output)
        for line in output.read_text().splitlines():
            if marker in line:
                print(line)
        return result.returncode
    finally:
        if target.exists() and target.read_bytes() == source:
            target.unlink()


if __name__ == "__main__":
    raise SystemExit(main())
