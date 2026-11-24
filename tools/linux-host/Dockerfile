# A Linux local host for verification on another OS: the pinned browser's runtime libraries, CRIU and the
# repository's Rust toolchain. Commands run as an unprivileged user whose id is the caller's, so the
# browser's own sandbox works and files written to the mounted repository keep their owner.
FROM rust:1.99-trixie
ARG UID=1000
RUN apt-get update && apt-get install -y --no-install-recommends \
      libnss3 libnspr4 libatk1.0-0t64 libatk-bridge2.0-0t64 libcups2t64 libdrm2 libxkbcommon0 libxcomposite1 \
      libxdamage1 libxfixes3 libxrandr2 libgbm1 libasound2t64 libpango-1.0-0 libcairo2 libexpat1 libdbus-1-3 \
      fonts-dejavu-core procps iproute2 iptables python3 sudo \
      build-essential pkg-config libprotobuf-dev libprotobuf-c-dev protobuf-c-compiler protobuf-compiler \
      python3-protobuf libnl-3-dev libnet1-dev libcap-dev libbsd-dev libaio-dev libgnutls28-dev libnftables-dev \
      uuid-dev libdrm-dev \
    && rm -rf /var/lib/apt/lists/*
# Kernels from 6.16 refuse SO_PASSCRED/SO_PASSSEC outside AF_UNIX/AF_NETLINK; CRIU handles that from v4.2.
RUN git clone --depth 1 --branch v4.2.1 https://github.com/checkpoint-restore/criu.git /opt/criu \
    && make -C /opt/criu -j"$(nproc)" criu \
    && install -m 755 /opt/criu/criu/criu /usr/local/sbin/criu
RUN rustup toolchain install 1.99.1 --profile minimal --component clippy && useradd -m -u "$UID" frontcap && echo 'frontcap ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/frontcap \
    && chmod -R a+w /usr/local/cargo /usr/local/rustup
USER frontcap
WORKDIR /work
