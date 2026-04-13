#!/usr/bin/env bash
set -euo pipefail

: "${AWS_REGION:?set AWS_REGION}"
: "${EKS_CLUSTER_NAME:?set EKS_CLUSTER_NAME}"
: "${REGISTRY_SERVER:=ghcr.io}"
: "${REGISTRY_USERNAME:=jbrough}"
: "${REGISTRY_PASSWORD:?set REGISTRY_PASSWORD}"

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

aws eks update-kubeconfig \
  --region "$AWS_REGION" \
  --name "$EKS_CLUSTER_NAME"

kubectl apply -f "${ENCODEC_API_KUSTOMIZE_PATH}/namespace.yaml"

kubectl -n "$ENCODEC_API_NAMESPACE" create secret docker-registry ghcr \
  --docker-server="$REGISTRY_SERVER" \
  --docker-username="$REGISTRY_USERNAME" \
  --docker-password="$REGISTRY_PASSWORD" \
  --dry-run=client -o yaml | kubectl apply -f -

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
kubectl -n "$ENCODEC_API_NAMESPACE" get deploy,pods,svc,ingress

echo "encodec-api deployed for ${ENCODEC_API_DOMAIN}"
