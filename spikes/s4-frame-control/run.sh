#!/bin/sh
set -eu
root="$(cd "$(dirname "$0")/../.." && pwd)"
docker build -t frontcap-s4 "$root/spikes/s4-frame-control"
docker run --rm -v "$root:/work" -w /work/spikes -e CARGO_TARGET_DIR=/tmp/target frontcap-s4 \
  sh -c 'cargo run --manifest-path ../Cargo.toml -p browser-pin -- install && cargo run -p s4-frame-control'
