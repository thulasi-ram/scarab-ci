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
# Same tolerance as `just test`: something already serving 55432 (an earlier
# stack, or an ad-hoc dev container) is reused — compose then only provides
# MinIO — instead of failing the port bind.
if (exec 3<>/dev/tcp/127.0.0.1/55432) 2>/dev/null; then
  echo "    reusing the Postgres already listening on 127.0.0.1:55432"
  pg_managed=0
  docker compose -f "$here/compose.yaml" up -d minio createbuckets
else
  pg_managed=1
  docker compose -f "$here/compose.yaml" up -d
fi

echo "==> creating kind cluster '$cluster' (kubeconfig: $kubeconfig)"
if ! kind get clusters 2>/dev/null | grep -qx "$cluster"; then
  kind create cluster --name "$cluster" --config "$here/kind.yaml" --kubeconfig "$kubeconfig"
else
  kind export kubeconfig --name "$cluster" --kubeconfig "$kubeconfig"
fi

echo "==> ensuring namespace 'scarab'"
KUBECONFIG="$kubeconfig" kubectl create namespace scarab \
  --dry-run=client -o yaml | KUBECONFIG="$kubeconfig" kubectl apply -f -

if [ "$pg_managed" = 1 ]; then
  echo "==> waiting for Postgres to be healthy"
  for _ in $(seq 1 30); do
    if docker compose -f "$here/compose.yaml" exec -T postgres pg_isready -U scarab -d scarab >/dev/null 2>&1; then
      break
    fi
    sleep 1
  done
fi

echo "==> building scarab-server"
( cd "$root" && cargo build -p scarab-server )

echo "==> starting scarab-server (converged) in the background"
# Env (DB/S3/namespace/addr) comes from the one dev env file — single source of
# truth, shared with `just serve`/`just ui`. Gitignored; seed it from the
# committed template on first run.
[ -f "$here/.env" ] || cp "$here/.env.example" "$here/.env"
set -a; . "$here/.env"; set +a
# A caller-supplied OVERLAY, applied last so it wins over the dev defaults. This
# is how a verification tier reconfigures the stack for its own run without
# editing (or racing on) the shared dev env file — e.g. `just forgejo-verify`
# needs the server bound on 0.0.0.0 with a public URL a container can reach.
if [ -n "${SCARAB_ENV_EXTRA:-}" ]; then
  [ -f "$SCARAB_ENV_EXTRA" ] || { echo "error: SCARAB_ENV_EXTRA=$SCARAB_ENV_EXTRA does not exist" >&2; exit 1; }
  echo "    applying env overlay: $SCARAB_ENV_EXTRA"
  set -a; . "$SCARAB_ENV_EXTRA"; set +a
fi
# KUBECONFIG is dynamic (this run's kind cluster), so it's set here, not in .env.
export KUBECONFIG="$kubeconfig"

nohup "$root/target/debug/scarab-server" --role converged --serve \
  > "$here/server.log" 2>&1 &
echo $! > "$here/server.pid"

echo "==> waiting for the API (/healthz)"
# Probe the address the server actually binds (from .env — e.g. a dev machine
# whose :8080 is taken sets SCARAB_ADDR to another port); 0.0.0.0 via loopback.
addr="${SCARAB_ADDR:-127.0.0.1:8080}"
base="http://${addr/0.0.0.0/127.0.0.1}"
for _ in $(seq 1 30); do
  if curl -sf "$base/healthz" >/dev/null 2>&1; then
    echo "==> scarab-server is up on $base (logs: deploy/local-proc/server.log)"
    echo "    run 'just demo' to submit a pipeline and watch it complete."
    exit 0
  fi
  sleep 1
done
echo "error: scarab-server did not become healthy; see deploy/local-proc/server.log" >&2
exit 1
