# encodec-api

`encodec-api` is the new public Rust service intended to replace the EnCodec-heavy parts of the current scratch.fm Python API.

Current scope:

- split-role service on the Wavey `web-service` crate
- CPU ingress deployment plus GPU worker deployment
- remote workers over `upload-response` with split ingress/worker scaling
- reusable GHCR base image for Opus 1.5.2 build tooling
- GHCR image builds for `encodec-api-ingress` and `encodec-api-worker`
- self-managed kubeadm-cluster deploy workflow and manifests for `encodec.wavey.ai`

Target architecture:

- ingress owns the public HTTPS surface and the `upload-response` cache
- ingress normalizes uploaded media into canonical PCM through `av-api` + `soundkit`
- workers poll ingress over the private `/_upload_response` plane
- worker-side `encodec-rs` ONNX execution handles `/encode`, `/encode/ecdc`, and `/encode/stream`
- `/encode/stream` returns NDJSON event frames over streamed HTTP and websocket transports
- worker scaling is via deployment replicas plus `UPLOAD_RESPONSE_MAX_INFLIGHT`

Notes:

- `encodec-api` deploys to the shared Wavey self-managed kubeadm cluster on Linode.
- The deploy workflow reads a kubeconfig from GitHub secrets, applies the Kubernetes manifests, and upserts `encodec.wavey.ai` to the CPU ingress node IP in Linode DNS.
- Ingress is CPU-only and is responsible for upload parsing, media decode, and canonical PCM preparation through `av-api` + `soundkit`.
- Worker pods are GPU-targeted and request `nvidia.com/gpu: 1`; they run pure-Rust `encodec-rs`.
- ONNX bundles are stored on a static local persistent volume pinned to the GPU node and seeded into that volume from the worker image on first deployment.
- The worker path no longer shells out to Python or an external `encodec` binary.

## GitHub secrets and variables

Secrets:

- `WAVEY_AI_GH_TOKEN`: classic PAT with package access for private dependency fetches and GHCR pulls
- `LINODE_TOKEN`: token that updates Linode DNS
- `KUBEADM_CLUSTER_KUBECONFIG_B64`: base64-encoded kubeconfig for the self-managed cluster

Variables:

- `ENCODEC_API_INGRESS_REPLICAS`: default `1`
- `ENCODEC_API_WORKER_REPLICAS`: default `1`
- `ENCODEC_API_NAMESPACE`: default `encodec-api`
- `ENCODEC_API_DOMAIN`: default `encodec.wavey.ai`

## Workflows

- `.github/workflows/build-image.yml`
- `.github/workflows/deploy-main.yml`

The image workflow now publishes:

- `ghcr.io/wavey-ai/encodec-api-opus-base`
- `ghcr.io/wavey-ai/encodec-api-ingress`
- `ghcr.io/wavey-ai/encodec-api-worker`

## Deploy layout

- manifests: `deploy/k8s/encodec-api/`
- Linode helper: `deploy/linode_api.py`
- helper script: `deploy/kubeadm/manual-deploy.sh`

## Local run

```bash
ENCODEC_ONNX_BUNDLE_ROOT=/Users/jamieb/wavey.ai/encodec-rs/onnx-bundles \
cargo run -- --port 8443
```

Health check:

```bash
curl --http2 -k https://127.0.0.1:8443/healthz
```

Split local dev:

```bash
# ingress
PORT=8443 ENCODEC_API_ROLE=ingress \
ENCODEC_ONNX_BUNDLE_ROOT=/Users/jamieb/wavey.ai/encodec-rs/onnx-bundles \
cargo run

# worker
PORT=9443 ENCODEC_API_ROLE=worker \
ENCODEC_ONNX_BUNDLE_ROOT=/Users/jamieb/wavey.ai/encodec-rs/onnx-bundles \
ENCODEC_EXECUTION_TARGET=cpu \
UPLOAD_RESPONSE_INGRESS_URLS=https://127.0.0.1:8443 \
UPLOAD_RESPONSE_INSECURE_TLS=true \
cargo run
```
