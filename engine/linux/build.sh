#!/bin/sh
# Builds the patch set for Linux: `sh engine/linux/build.sh [linux-arm64|linux64]` (default linux-arm64).
# The Chromium checkout lives in the Docker volume `frontcap-chromium-linux`, synced at the pinned tag on
# first use with both sysroots; the build lands in browsers/patched/<patchset>/<platform>/ like a host build.
# Six parallel compiles (FRONTCAP_BUILD_JOBS) fit a 16 GB Docker VM under Rosetta, where twelve were killed
# for memory; twelve fit a 29 GB VM.
set -eu
platform="${1:-linux-arm64}"
case "$platform" in
  linux-arm64|linux64) ;;
  *) echo "not a Linux platform: $platform" >&2; exit 2 ;;
esac
root="$(cd "$(dirname "$0")/../.." && pwd)"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/browsers.toml")"
docker build --platform linux/amd64 --build-arg "CHROMIUM_VERSION=$version" -t frontcap-engine-linux "$root/engine/linux"
docker run --rm --platform linux/amd64 -v frontcap-chromium-linux:/chromium -v frontcap-engine-target:/target \
  -v "$root:/work" -e CARGO_TARGET_DIR=/target -e VERSION="$version" -e PLATFORM="$platform" \
  -e FRONTCAP_BUILD_JOBS="${FRONTCAP_BUILD_JOBS:-6}" -e OWNER="$(id -u):$(id -g)" \
  frontcap-engine-linux sh -c '
    set -eu
    if [ ! -f /chromium/src/chrome/VERSION ]; then
      cd /chromium
      printf "%s\n" "solutions = [{\"name\": \"src\", \"url\": \"https://chromium.googlesource.com/chromium/src.git\"," \
        "  \"managed\": False, \"custom_deps\": {}, \"custom_vars\": {\"checkout_nacl\": False}}]" \
        "target_os = [\"linux\"]" "target_cpu = [\"arm64\", \"x64\"]" > .gclient
      gclient sync --no-history --nohooks --revision "src@refs/tags/$VERSION"
      gclient runhooks
    fi
    # The container runs as root; what it installs belongs to the user who ran the build.
    status=0
    cd /work && cargo run --release -p browser-pin -- engine build --src /chromium/src --platform "$PLATFORM" || status=$?
    chown -R "$OWNER" /work/browsers
    exit "$status"'
