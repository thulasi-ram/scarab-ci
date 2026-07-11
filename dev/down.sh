#!/usr/bin/env bash
# Tear down the Scarab dev stack: stop the server, remove the kind cluster and
# the Postgres/MinIO containers. Best-effort; safe to run repeatedly.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cluster="scarab-dev"

if [[ -f "$here/server.pid" ]]; then
  pid="$(cat "$here/server.pid")"
  echo "==> stopping scarab-server (pid $pid)"
  kill "$pid" 2>/dev/null || true
  rm -f "$here/server.pid"
fi

echo "==> docker compose down"
docker compose -f "$here/compose.yaml" down -v || true

echo "==> deleting kind cluster '$cluster'"
kind delete cluster --name "$cluster" 2>/dev/null || true

rm -f "$here/.kubeconfig" "$here/server.log"
echo "==> down."
