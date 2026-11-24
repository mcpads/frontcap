#!/bin/sh
set -eu
cd "$(dirname "$0")"
output="$(mktemp -d "${TMPDIR:-/tmp}/frontcap-clock-kernel.XXXXXX")"
trap 'rm -rf "$output"' EXIT HUP INT TERM
"${CXX:-c++}" -std=c++20 -O2 -Wall -Wextra -Werror -pedantic -pthread probe.cc -o "$output/probe"
"$output/probe"
