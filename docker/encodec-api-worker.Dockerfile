# syntax=docker/dockerfile:1.7

FROM rust:1.88-bookworm AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    git \
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

FROM nvidia/cuda:12.4.1-cudnn-runtime-ubuntu22.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    ffmpeg \
    python3 \
    python3-venv \
  && rm -rf /var/lib/apt/lists/*

RUN python3 -m venv /opt/encodec-env \
  && /opt/encodec-env/bin/pip install --no-cache-dir --upgrade pip \
  && /opt/encodec-env/bin/pip install --no-cache-dir --index-url https://download.pytorch.org/whl/cu124 torch==2.5.1 torchaudio==2.5.1 \
  && /opt/encodec-env/bin/pip install --no-cache-dir encodec==0.1.1

COPY --from=build /app/target/release/encodec-api /usr/local/bin/encodec-api

ENV ENCODEC_API_ROLE=worker
ENV ENCODEC_BIN=/opt/encodec-env/bin/encodec
ENV FFMPEG_BIN=/usr/bin/ffmpeg
ENV PORT=8443
EXPOSE 8443

CMD ["encodec-api"]
