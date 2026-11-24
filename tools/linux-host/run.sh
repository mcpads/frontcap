#!/bin/sh
# Runs a command on a Linux local host in Docker with the repository mounted at /work, e.g.
# `sh tools/linux-host/run.sh cargo test -p frontcap-adapter-chromium --test navigate`. The build
# directory is the Docker volume `frontcap-linux-target`, built without incremental state and with line
# tables only for debug info: the full workspace's tests otherwise filled a 125 GB Docker disk. Privileged
# for CRIU and namespaces.
set -eu
root="$(cd "$(dirname "$0")/../.." && pwd)"
docker build -q --build-arg "UID=$(id -u)" -t frontcap-linux-host "$root/tools/linux-host" >/dev/null
# Engines `browser-pin engine allow-sandbox` installed for a host that restricts user namespaces, at the
# path its AppArmor profile names.
engines=""
[ -d /opt/frontcap/engines ] && engines="-v /opt/frontcap/engines:/opt/frontcap/engines:ro"
exec docker run --rm --init --privileged -v "$root:/work" $engines -v frontcap-linux-target:/target \
  -e CARGO_TARGET_DIR=/target -e CARGO_INCREMENTAL=0 -e CARGO_PROFILE_DEV_DEBUG=line-tables-only -w /work frontcap-linux-host sh -c 'sudo chown -R frontcap /target && exec "$@"' sh "$@"
