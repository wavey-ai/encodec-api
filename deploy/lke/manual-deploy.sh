#!/usr/bin/env bash
set -euo pipefail

: "${KUBECONFIG:?set KUBECONFIG to the target cluster kubeconfig}"
: "${LINODE_TOKEN:?set LINODE_TOKEN for Linode DNS updates}"
: "${REGISTRY_SERVER:=ghcr.io}"
: "${REGISTRY_USERNAME:=jbrough}"
: "${REGISTRY_PASSWORD:?set REGISTRY_PASSWORD for private image pulls}"

ENCODEC_API_NAMESPACE="${ENCODEC_API_NAMESPACE:-encodec-api}"
ENCODEC_API_DOMAIN="${ENCODEC_API_DOMAIN:-encodec.wavey.ai}"
ENCODEC_API_KUSTOMIZE_PATH="${ENCODEC_API_KUSTOMIZE_PATH:-deploy/k8s/encodec-api}"
ENCODEC_API_INGRESS_IMAGE="${ENCODEC_API_INGRESS_IMAGE:-ghcr.io/wavey-ai/encodec-api-ingress:main}"
ENCODEC_API_WORKER_IMAGE="${ENCODEC_API_WORKER_IMAGE:-ghcr.io/wavey-ai/encodec-api-worker:main}"
ENCODEC_API_INGRESS_REPLICAS="${ENCODEC_API_INGRESS_REPLICAS:-1}"
ENCODEC_API_WORKER_REPLICAS="${ENCODEC_API_WORKER_REPLICAS:-1}"

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

openssl req -x509 -nodes -newkey rsa:2048 -sha256 \
  -keyout "$tmpdir/encodec-api.key" \
  -out "$tmpdir/encodec-api.crt" \
  -days 30 \
  -subj "/CN=encodec-api" \
  -addext "subjectAltName=DNS:encodec-api-ingress,DNS:encodec-api-ingress.${ENCODEC_API_NAMESPACE}.svc.cluster.local,DNS:encodec-api-ingress-internal,DNS:encodec-api-ingress-internal.${ENCODEC_API_NAMESPACE}.svc.cluster.local" \
  >/dev/null 2>&1

openssl req -x509 -nodes -newkey rsa:2048 -sha256 \
  -keyout "$tmpdir/public.key" \
  -out "$tmpdir/public.crt" \
  -days 30 \
  -subj "/CN=${ENCODEC_API_DOMAIN}" \
  -addext "subjectAltName=DNS:${ENCODEC_API_DOMAIN}" \
  >/dev/null 2>&1

kubectl apply -f "${ENCODEC_API_KUSTOMIZE_PATH}/namespace.yaml"
kubectl -n "$ENCODEC_API_NAMESPACE" delete pod -l wavey.ai/image-loader=true --ignore-not-found=true --wait=true || true

kubectl -n "$ENCODEC_API_NAMESPACE" create secret docker-registry ghcr-wavey-ai \
  --docker-server="$REGISTRY_SERVER" \
  --docker-username="$REGISTRY_USERNAME" \
  --docker-password="$REGISTRY_PASSWORD" \
  --dry-run=client -o yaml | kubectl apply -f -

kubectl -n "$ENCODEC_API_NAMESPACE" create secret tls encodec-api-tls \
  --cert="$tmpdir/encodec-api.crt" \
  --key="$tmpdir/encodec-api.key" \
  --dry-run=client -o yaml | kubectl apply -f -

kubectl -n "$ENCODEC_API_NAMESPACE" create secret tls encodec-wavey-ai-tls \
  --cert="$tmpdir/public.crt" \
  --key="$tmpdir/public.key" \
  --dry-run=client -o yaml | kubectl apply -f -

kubectl apply -k "$ENCODEC_API_KUSTOMIZE_PATH"
kubectl -n "$ENCODEC_API_NAMESPACE" set image deployment/encodec-api-ingress ingress="$ENCODEC_API_INGRESS_IMAGE"
kubectl -n "$ENCODEC_API_NAMESPACE" set image deployment/encodec-api-worker worker="$ENCODEC_API_WORKER_IMAGE"
kubectl -n "$ENCODEC_API_NAMESPACE" scale deployment/encodec-api-ingress --replicas="$ENCODEC_API_INGRESS_REPLICAS"
kubectl -n "$ENCODEC_API_NAMESPACE" scale deployment/encodec-api-worker --replicas="$ENCODEC_API_WORKER_REPLICAS"
kubectl -n "$ENCODEC_API_NAMESPACE" rollout restart deployment/encodec-api-ingress
kubectl -n "$ENCODEC_API_NAMESPACE" rollout restart deployment/encodec-api-worker
kubectl -n "$ENCODEC_API_NAMESPACE" rollout status deployment/encodec-api-ingress --timeout=20m
kubectl -n "$ENCODEC_API_NAMESPACE" rollout status deployment/encodec-api-worker --timeout=30m

ingress_ip=""
for _ in $(seq 1 60); do
  ingress_ip="$(kubectl -n "$ENCODEC_API_NAMESPACE" get ingress encodec-api -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)"
  if [[ -z "$ingress_ip" ]]; then
    ingress_ip="$(kubectl -n ingress-nginx get svc ingress-nginx-controller -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)"
  fi
  if [[ -n "$ingress_ip" ]]; then
    break
  fi
  sleep 5
done

if [[ -z "$ingress_ip" ]]; then
  echo "failed to resolve ingress IP" >&2
  exit 1
fi

python3 deploy/linode_api.py upsert-domain-a-record \
  --domain wavey.ai \
  --name encodec \
  --target "$ingress_ip" \
  --ttl-sec 30

kubectl -n "$ENCODEC_API_NAMESPACE" get deploy,pods,svc,ingress
echo "encodec-api deployed to ${ENCODEC_API_DOMAIN} (${ingress_ip})"
