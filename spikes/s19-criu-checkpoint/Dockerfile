FROM rust:1.98-trixie
RUN apt-get update && apt-get install -y --no-install-recommends \
      curl unzip ca-certificates libnss3 libnspr4 libatk1.0-0t64 libatk-bridge2.0-0t64 libcups2t64 libdrm2 \
      libxkbcommon0 libxcomposite1 libxdamage1 libxfixes3 libxrandr2 libgbm1 libasound2t64 libpango-1.0-0 \
      libcairo2 libexpat1 libdbus-1-3 fonts-dejavu-core procps \
      build-essential pkg-config git libprotobuf-dev libprotobuf-c-dev protobuf-c-compiler protobuf-compiler \
      python3-protobuf libnl-3-dev libnet1-dev libcap-dev libbsd-dev libaio-dev libgnutls28-dev libnftables-dev \
      uuid-dev libdrm-dev iproute2 iptables python3 \
    && rm -rf /var/lib/apt/lists/*
# Kernels from 6.16 refuse SO_PASSCRED/SO_PASSSEC outside AF_UNIX/AF_NETLINK; CRIU handles that from v4.2.
RUN git clone --depth 1 --branch v4.2.1 https://github.com/checkpoint-restore/criu.git /opt/criu \
    && make -C /opt/criu -j"$(nproc)" criu \
    && install -m 755 /opt/criu/criu/criu /usr/local/sbin/criu
WORKDIR /work
