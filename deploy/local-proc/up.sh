#!/usr/bin/env bash
# Bring up the full Scarab dev stack and start scarab-server against it:
#   - Postgres + MinIO via docker compose
#   - a kind cluster (isolated kubeconfig at deploy/local-proc/.kubeconfig)
#   - scarab-server --role converged --serve in the background
#
# Idempotent: safe to re-run. Requires docker, kind, kubectl.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
kubeconfig="$here/.kubeconfig"
cluster="scarab-dev"

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: '$1' not found on PATH" >&2; exit 1; }; }
need docker; need kind; need kubectl

echo "==> starting Postgres + MinIO (docker compose)"
docker compose -f "$here/compose.yaml" up -d

echo "==> creating kind cluster '$cluster' (kubeconfig: $kubeconfig)"
if ! kind get clusters 2>/dev/null | grep -qx "$cluster"; then
  kind create cluster --name "$cluster" --config "$here/kind.yaml" --kubeconfig "$kubeconfig"
else
  kind export kubeconfig --name "$cluster" --kubeconfig "$kubeconfig"
fi

echo "==> ensuring namespace 'scarab'"
KUBECONFIG="$kubeconfig" kubectl create namespace scarab \
  --dry-run=client -o yaml | KUBECONFIG="$kubeconfig" kubectl apply -f -

echo "==> waiting for Postgres to be healthy"
for _ in $(seq 1 30); do
  if docker compose -f "$here/compose.yaml" exec -T postgres pg_isready -U scarab -d scarab >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

echo "==> building scarab-server"
( cd "$root" && cargo build -p scarab-server )

echo "==> starting scarab-server (converged) in the background"
export SCARAB_DATABASE_URL="postgres://scarab:scarab@127.0.0.1:55432/scarab"
# Dev-only escape hatch (ADR-0048): boots without an authenticator/KEK, with
# loud warnings. Production must never set this.
export SCARAB_DEV_INSECURE=1
export SCARAB_S3_BUCKET="scarab-logs"
export SCARAB_S3_ENDPOINT="http://127.0.0.1:9000"
export SCARAB_S3_ACCESS_KEY="scarab"
export SCARAB_S3_SECRET_KEY="scarabsecret"
export SCARAB_S3_REGION="us-east-1"
export KUBECONFIG="$kubeconfig"
export SCARAB_NAMESPACE="scarab"

nohup "$root/target/debug/scarab-server" --role converged --serve --addr 127.0.0.1:8080 \
  > "$here/server.log" 2>&1 &
echo $! > "$here/server.pid"

echo "==> waiting for the API (/healthz)"
for _ in $(seq 1 30); do
  if curl -sf http://127.0.0.1:8080/healthz >/dev/null 2>&1; then
    echo "==> scarab-server is up on http://127.0.0.1:8080 (logs: deploy/local-proc/server.log)"
    echo "    run 'just demo' to submit a pipeline and watch it complete."
    exit 0
  fi
  sleep 1
done
echo "error: scarab-server did not become healthy; see deploy/local-proc/server.log" >&2
exit 1
