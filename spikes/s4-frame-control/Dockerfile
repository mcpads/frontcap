FROM rust:1.98-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
      curl unzip ca-certificates libnss3 libnspr4 libatk1.0-0 libatk-bridge2.0-0 libcups2 libdrm2 \
      libxkbcommon0 libxcomposite1 libxdamage1 libxfixes3 libxrandr2 libgbm1 libasound2 libpango-1.0-0 \
      libcairo2 libexpat1 libdbus-1-3 fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /work
