# syntax=docker/dockerfile:1.7

FROM ubuntu:24.04 AS chef

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:/usr/local/rustup/bin:${PATH}

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    clang \
    cmake \
    curl \
    git \
    libclang-dev \
    libopus-dev \
    libssl-dev \
    pkg-config \
  && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain 1.88.0

RUN cargo install cargo-chef --locked

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
    libopus0 \
  && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/encodec-api /usr/local/bin/encodec-api
COPY --from=build /app/onnx-bundles /opt/encodec-rs/onnx-bundles

ENV ENCODEC_API_ROLE=worker
ENV ENCODEC_ONNX_BUNDLE_ROOT=/opt/encodec-rs/onnx-bundles
ENV PORT=8443
EXPOSE 8443

CMD ["encodec-api"]
