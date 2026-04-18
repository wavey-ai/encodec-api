# syntax=docker/dockerfile:1.7

ARG OPUS_BASE_IMAGE=ghcr.io/wavey-ai/encodec-api-opus-base:main

FROM ${OPUS_BASE_IMAGE} AS chef

WORKDIR /app

FROM chef AS planner

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=secret,id=github_token \
    set -eu; \
    if [ -s /run/secrets/github_token ]; then \
      token="$(cat /run/secrets/github_token)"; \
      git config --global url."https://x-access-token:${token}@github.com/".insteadOf https://github.com/; \
    fi; \
    cargo chef prepare --recipe-path recipe.json

FROM chef AS build

COPY --from=planner /app/recipe.json recipe.json

RUN --mount=type=secret,id=github_token \
    set -eu; \
    if [ -s /run/secrets/github_token ]; then \
      token="$(cat /run/secrets/github_token)"; \
      git config --global url."https://x-access-token:${token}@github.com/".insteadOf https://github.com/; \
    fi; \
    cargo chef cook --release --locked --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=secret,id=github_token \
    set -eu; \
    if [ -s /run/secrets/github_token ]; then \
      token="$(cat /run/secrets/github_token)"; \
      git config --global url."https://x-access-token:${token}@github.com/".insteadOf https://github.com/; \
    fi; \
    cargo build --release --locked

RUN set -eu; \
    bundle_root="$(find "${CARGO_HOME}/git/checkouts" -maxdepth 4 -type d -path '*/onnx-bundles' | head -n 1)"; \
    test -n "${bundle_root}"; \
    mkdir -p /app/onnx-bundles; \
    cp -R "${bundle_root}/." /app/onnx-bundles/

FROM nvidia/cuda:12.6.3-cudnn-runtime-ubuntu24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
  && rm -rf /var/lib/apt/lists/*

COPY --from=chef /usr/local/lib/libopus.so* /usr/local/lib/
COPY --from=build /app/target/release/encodec-api /usr/local/bin/encodec-api
COPY --from=build /app/onnx-bundles /opt/encodec-rs/onnx-bundles

ENV LD_LIBRARY_PATH=/usr/local/lib
ENV ENCODEC_API_ROLE=worker
ENV ENCODEC_ONNX_BUNDLE_ROOT=/opt/encodec-rs/onnx-bundles
ENV PORT=8443
EXPOSE 8443

CMD ["encodec-api"]
