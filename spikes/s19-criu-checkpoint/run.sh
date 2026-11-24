#!/bin/sh
# P05: checkpoint a running headless shell's process tree with CRIU and restore it twice. CRIU needs a
# privileged Linux container; Docker Desktop must be running.
set -eu
root="$(cd "$(dirname "$0")/../.." && pwd)"
docker build -t frontcap-s19 "$root/spikes/s19-criu-checkpoint"
docker run --rm --init --privileged -v "$root:/work" -w /work/spikes -e CARGO_TARGET_DIR=/target -v frontcap-s19-target:/target frontcap-s19 \
  sh -c 'cargo run --manifest-path ../Cargo.toml -p browser-pin -- install && cargo run -p s19-criu-checkpoint'
