#!/bin/sh
# Serves frontcap-mcp over stdio from the Linux local host with browsers in cells (as root,
# FRONTCAP_CELLS=65534:65534), for checkpoint bases and branches: set FRONTCAP_MCP to this script for
# `tools/mcp-relay.py serve`. The workspace inside the container is FRONTCAP_WORKSPACE_IN_CONTAINER
# (default /tmp/frontcap-workspace); an optional FRONTCAP_APPS directory under the repository
# is served at http://127.0.0.1:18765 there.
set -eu
root="$(cd "$(dirname "$0")/../.." && pwd)"
docker build -q --build-arg "UID=$(id -u)" -t frontcap-linux-host "$root/tools/linux-host" >/dev/null
exec docker run --rm -i --init --privileged -v "$root:/work" -v frontcap-linux-target:/target \
  -e CARGO_TARGET_DIR=/target -e CARGO_INCREMENTAL=0 -e CARGO_PROFILE_DEV_DEBUG=line-tables-only \
  -e "WORKSPACE=${FRONTCAP_WORKSPACE_IN_CONTAINER:-/tmp/frontcap-workspace}" \
  -e "APPS=${FRONTCAP_APPS:-}" -w /work frontcap-linux-host sh -c '
    sudo chown -R frontcap /target
    cargo build -q -p frontcap-mcp -p frontcap-recorder --all-features >&2
    if [ -n "$APPS" ]; then
      test -d "$APPS" || exit 1
      python3 -m http.server 18765 --bind 127.0.0.1 --directory "$APPS" >/dev/null 2>&1 &
    fi
    exec sudo env "PATH=$PATH:/usr/local/sbin" FRONTCAP_CELLS=65534:65534 FRONTCAP_WORKSPACE="$WORKSPACE" \
      FRONTCAP_HOME=/work FRONTCAP_RECORDER=/target/debug/frontcap-recorder /target/debug/frontcap-mcp'
