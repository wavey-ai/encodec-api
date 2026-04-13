# encodec-api

`encodec-api` is the new public Rust service intended to replace the EnCodec-heavy parts of the current scratch.fm Python API.

Current scope:

- split-role service on the Wavey `web-service` crate
- CPU ingress deployment plus GPU worker deployment
- remote workers over `upload-response` with split ingress/worker scaling
- GHCR image builds for `encodec-api-ingress` and `encodec-api-worker`
- EKS deploy workflow and manifests for `encodec.wavey.ai`

Target architecture:

- ingress owns the public HTTPS surface and the `upload-response` cache
- ingress normalizes uploaded media into canonical PCM through `av-api` + `soundkit`
- workers poll ingress over the private `/_upload_response` plane
- worker-side `encodec-rs` integration handles `/encode`, `/encode/ecdc`, and `/encode/stream`
- `/encode/stream` returns NDJSON event frames over streamed HTTP and websocket transports
- worker scaling is via deployment replicas plus `UPLOAD_RESPONSE_MAX_INFLIGHT`

Notes:

- `encodec.wavey.ai` DNS is managed outside AWS. The repo only handles image build and Kubernetes deploy; DNS/TLS stays in your Linode/Cloudflare setup.
- The worker runtime only needs `encodec` plus CUDA-capable PyTorch. Media decode and canonical PCM preparation happen on ingress through `av-api` and `soundkit`.

## GitHub secrets and variables

Secrets:

- `WAVEY_AI_GH_TOKEN`: classic PAT with package access for private dependency fetches and GHCR pulls
- `AWS_ROLE_TO_ASSUME`: IAM role assumed by GitHub Actions for EKS deploys

Variables:

- `AWS_REGION`: default `us-east-1`
- `EKS_CLUSTER_NAME`: target EKS cluster name
- `ENCODEC_API_NAMESPACE`: default `encodec-api`
- `ENCODEC_API_DOMAIN`: default `encodec.wavey.ai`
- `ENCODEC_API_REGISTRY_USERNAME`: default `jbrough`
- `ENCODEC_API_INGRESS_REPLICAS`: default `1`
- `ENCODEC_API_WORKER_REPLICAS`: default `1`

## Workflows

- `.github/workflows/build-image.yml`
- `.github/workflows/deploy-eks.yml`

## Deploy layout

- manifests: `deploy/k8s/encodec-api/`
- helper script: `deploy/eks/manual-deploy.sh`

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
