# syntax=docker/dockerfile:1.7

FROM rust:1.88-bookworm AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    git \
    pkg-config \
    libopus-dev \
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
    libopus0 \
  && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/encodec-api /usr/local/bin/encodec-api

ENV ENCODEC_API_ROLE=ingress
ENV PORT=8443
EXPOSE 8443

CMD ["encodec-api"]
