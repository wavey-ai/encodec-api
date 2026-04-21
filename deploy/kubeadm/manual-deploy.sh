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
ENCODEC_API_EDGE_IP="${ENCODEC_API_EDGE_IP:-172.238.175.161}"
ENCODEC_API_GPU_NODE="${ENCODEC_API_GPU_NODE:-wavey-kubeadm-gpu-01}"
ENCODEC_API_MODEL_PATH="${ENCODEC_API_MODEL_PATH:-/var/lib/encodec-api/models}"
ENCODEC_API_PUBLIC_TLS_CERT_PATH="${ENCODEC_API_PUBLIC_TLS_CERT_PATH:-}"
ENCODEC_API_PUBLIC_TLS_KEY_PATH="${ENCODEC_API_PUBLIC_TLS_KEY_PATH:-}"

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

if [ -n "$ENCODEC_API_PUBLIC_TLS_CERT_PATH" ] || [ -n "$ENCODEC_API_PUBLIC_TLS_KEY_PATH" ]; then
  if [ ! -f "$ENCODEC_API_PUBLIC_TLS_CERT_PATH" ] || [ ! -f "$ENCODEC_API_PUBLIC_TLS_KEY_PATH" ]; then
    echo "public TLS cert/key paths must both exist" >&2
    exit 1
  fi
  cp "$ENCODEC_API_PUBLIC_TLS_CERT_PATH" "$tmpdir/public.crt"
  cp "$ENCODEC_API_PUBLIC_TLS_KEY_PATH" "$tmpdir/public.key"
fi

kubectl apply -f "${ENCODEC_API_KUSTOMIZE_PATH}/namespace.yaml"

kubectl -n "$ENCODEC_API_NAMESPACE" delete pod/encodec-api-model-bootstrap --ignore-not-found --wait=true

cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: encodec-api-model-bootstrap
  namespace: ${ENCODEC_API_NAMESPACE}
spec:
  nodeName: ${ENCODEC_API_GPU_NODE}
  restartPolicy: Never
  tolerations:
    - operator: Exists
  containers:
    - name: bootstrap
      image: busybox:1.36
      command:
        - sh
        - -lc
        - |
          set -eu
          mkdir -p /model-store
      volumeMounts:
        - name: model-store
          mountPath: /model-store
  volumes:
    - name: model-store
      hostPath:
        path: ${ENCODEC_API_MODEL_PATH}
        type: DirectoryOrCreate
EOF

kubectl -n "$ENCODEC_API_NAMESPACE" wait --for=jsonpath='{.status.phase}'=Succeeded pod/encodec-api-model-bootstrap --timeout=180s
kubectl -n "$ENCODEC_API_NAMESPACE" delete pod/encodec-api-model-bootstrap --wait=true

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
kubectl -n "$ENCODEC_API_NAMESPACE" set image deployment/encodec-api-worker worker="$ENCODEC_API_WORKER_IMAGE" seed-bundles="$ENCODEC_API_WORKER_IMAGE"
kubectl -n "$ENCODEC_API_NAMESPACE" scale deployment/encodec-api-ingress --replicas="$ENCODEC_API_INGRESS_REPLICAS"
kubectl -n "$ENCODEC_API_NAMESPACE" scale deployment/encodec-api-worker --replicas="$ENCODEC_API_WORKER_REPLICAS"
kubectl -n "$ENCODEC_API_NAMESPACE" rollout restart deployment/encodec-api-ingress
kubectl -n "$ENCODEC_API_NAMESPACE" rollout restart deployment/encodec-api-worker
kubectl -n "$ENCODEC_API_NAMESPACE" rollout status deployment/encodec-api-ingress --timeout=20m
kubectl -n "$ENCODEC_API_NAMESPACE" rollout status deployment/encodec-api-worker --timeout=30m

python3 deploy/linode_api.py upsert-domain-a-record \
  --domain wavey.ai \
  --name encodec \
  --target "$ENCODEC_API_EDGE_IP" \
  --ttl-sec 30

kubectl -n "$ENCODEC_API_NAMESPACE" get pv,pvc,deploy,pods,svc,ingress
echo "encodec-api deployed to ${ENCODEC_API_DOMAIN} (${ENCODEC_API_EDGE_IP})"
