# encodec-api

`encodec-api` is the new public Rust service intended to replace the EnCodec-heavy parts of the current scratch.fm Python API.

Current scope:

- split-role service on the Wavey `web-service` crate
- CPU ingress deployment plus GPU worker deployment
- remote workers over `upload-response` with split ingress/worker scaling
- GHCR image builds for `encodec-api-ingress` and `encodec-api-worker`
- Linode LKE deploy workflow and manifests for `encodec.wavey.ai`

Target architecture:

- ingress owns the public HTTPS surface and the `upload-response` cache
- ingress normalizes uploaded media into canonical PCM through `av-api` + `soundkit`
- workers poll ingress over the private `/_upload_response` plane
- worker-side `encodec-rs` integration handles `/encode`, `/encode/ecdc`, and `/encode/stream`
- `/encode/stream` returns NDJSON event frames over streamed HTTP and websocket transports
- worker scaling is via deployment replicas plus `UPLOAD_RESPONSE_MAX_INFLIGHT`

Notes:

- `encodec-api` deploys to the shared Wavey Linode Kubernetes Engine cluster.
- The deploy workflow resolves cluster access through the Linode API, applies the Kubernetes manifests, and upserts the `encodec.wavey.ai` DNS record in Linode DNS.
- The worker runtime only needs `encodec` plus CUDA-capable PyTorch. Media decode and canonical PCM preparation happen on ingress through `av-api` and `soundkit`.

## GitHub secrets and variables

Secrets:

- `WAVEY_AI_GH_TOKEN`: classic PAT with package access for private dependency fetches and GHCR pulls
- `LINODE_TOKEN`: token that can read the shared LKE cluster and update Linode DNS

Variables:

- `ENCODEC_API_INGRESS_REPLICAS`: default `1`
- `ENCODEC_API_WORKER_REPLICAS`: default `1`
- `ENCODEC_API_NAMESPACE`: default `encodec-api`
- `ENCODEC_API_DOMAIN`: default `encodec.wavey.ai`

## Workflows

- `.github/workflows/build-image.yml`
- `.github/workflows/deploy-main.yml`

## Deploy layout

- manifests: `deploy/k8s/encodec-api/`
- Linode helper: `deploy/linode_api.py`
- helper script: `deploy/lke/manual-deploy.sh`

## Local run

```bash
cargo run -- --port 8443 --encodec-bin "$(command -v encodec)"
```

Health check:

```bash
curl --http2 -k https://127.0.0.1:8443/healthz
```

Split local dev:

```bash
# ingress
PORT=8443 ENCODEC_API_ROLE=ingress cargo run

# worker
PORT=9443 ENCODEC_API_ROLE=worker \
UPLOAD_RESPONSE_INGRESS_URLS=https://127.0.0.1:8443 \
UPLOAD_RESPONSE_INSECURE_TLS=true \
cargo run
```
