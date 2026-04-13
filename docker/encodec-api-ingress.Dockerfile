# syntax=docker/dockerfile:1.7

FROM debian:bookworm AS opus

ARG OPUS_VERSION=1.5.2

RUN apt-get update && apt-get install -y --no-install-recommends \
    autoconf \
    automake \
    build-essential \
    ca-certificates \
    curl \
    libtool \
    pkg-config \
  && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL "https://downloads.xiph.org/releases/opus/opus-${OPUS_VERSION}.tar.gz" \
    | tar -xz -C /tmp \
  && cd "/tmp/opus-${OPUS_VERSION}" \
  && ./configure --prefix=/usr/local \
  && make -j"$(nproc)" \
  && make install

FROM rust:1.88-bookworm AS build

COPY --from=opus /usr/local /usr/local

ENV PKG_CONFIG_PATH=/usr/local/lib/pkgconfig

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    clang \
    git \
    libclang-dev \
    pkg-config \
    libssl-dev \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=secret,id=github_token \
    set -eu; \
    if [ -s /run/secrets/github_token ]; then \
      token="$(cat /run/secrets/github_token)"; \
      git config --global url."https://x-access-token:${token}@github.com/".insteadOf https://github.com/; \
    fi; \
    cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
  && rm -rf /var/lib/apt/lists/*

COPY --from=opus /usr/local/lib/libopus.so* /usr/local/lib/

COPY --from=build /app/target/release/encodec-api /usr/local/bin/encodec-api

ENV LD_LIBRARY_PATH=/usr/local/lib
ENV ENCODEC_API_ROLE=ingress
ENV PORT=8443
EXPOSE 8443

CMD ["encodec-api"]
