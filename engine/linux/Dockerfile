# The patched engine's Linux build environment. Chromium publishes its toolchain for x64 Linux hosts only,
# so this is an amd64 image (Rosetta on Apple silicon) that cross-builds linux-arm64 as well as linux64.
FROM --platform=linux/amd64 rust:1.99-trixie
ARG CHROMIUM_VERSION
RUN apt-get update && apt-get install -y --no-install-recommends lsb-release sudo file xz-utils python3 \
    && rm -rf /var/lib/apt/lists/*
# The build dependencies of the pinned tag, from its own installer.
RUN curl -fsSL "https://chromium.googlesource.com/chromium/src/+/refs/tags/${CHROMIUM_VERSION}/build/install-build-deps.py?format=TEXT" \
      | base64 -d > /tmp/install-build-deps.py \
    && python3 /tmp/install-build-deps.py --no-prompt --no-syms --no-arm --no-chromeos-fonts --no-backwards-compatible \
    && rm -rf /var/lib/apt/lists/* /tmp/install-build-deps.py
# Bootstrapped here: with updates off, gclient does not bootstrap depot_tools' own Python, which gn needs.
RUN git clone --depth 1 https://chromium.googlesource.com/chromium/tools/depot_tools.git /opt/depot_tools \
    && cd /opt/depot_tools && ./ensure_bootstrap
ENV PATH=/opt/depot_tools:$PATH DEPOT_TOOLS_UPDATE=0
WORKDIR /work
