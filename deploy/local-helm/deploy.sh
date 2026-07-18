#!/usr/bin/env bash
# Deploy the Scarab dogfood stack (in-cluster Postgres + scarab-server via Helm)
# onto the LOCAL colima cluster. Reproducible; run it as many times as needed.
#
# Config comes from deploy/local-helm/.env (gitignored — see .env.example). A
# real environment variable already set in your shell overrides the file. Pass
# an image tag to override IMAGE_TAG (e.g. a GHA `sha-<gitsha>`):
#
#   deploy/local-helm/deploy.sh [IMAGE_TAG]
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

# Load .env WITHOUT clobbering vars already set in the environment (env wins).
ENVFILE="${ENVFILE:-$HERE/.env}"
if [ -f "$ENVFILE" ]; then
  # The .env file is authoritative for a deploy (file-wins): a stale ambient
  # value — e.g. direnv-loaded .env.local — must NOT silently override the
  # in-cluster config. Split on the FIRST '=' only, so base64 values that end
  # in '=' survive (IFS='=' read would truncate them).
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      ''|\#*) continue ;;   # skip blank + comment lines
      *=*) ;;               # KEY=VALUE — process
      *) continue ;;        # anything else — skip
    esac
    export "${line%%=*}=${line#*=}"
  done < "$ENVFILE"
else
  echo "missing $ENVFILE (cp deploy/local-helm/.env.example deploy/local-helm/.env and fill it)" >&2
  exit 1
fi

# HARD GUARD: only ever touch the local colima cluster, never a Acme EKS ctx.
ctx="$(kubectl config current-context)"
[ "$ctx" = "colima" ] || { echo "refusing: kubectl context is '$ctx', not 'colima'." >&2; exit 1; }

NS="${NAMESPACE:-scarab}"
TAG="${1:-${IMAGE_TAG:-dogfood-local}}"

: "${SCARAB_MASTER_KEY:?set SCARAB_MASTER_KEY in .env}"
: "${SCARAB_DATABASE_URL:?set SCARAB_DATABASE_URL in .env}"

# Render a transient values file from .env (deleted on exit — no secrets on the
# CLI, none left on disk).
VALUES="$(mktemp)"
trap 'rm -f "$VALUES"' EXIT
cat > "$VALUES" <<YAML
image:
  repository: ${IMAGE_REPOSITORY:-scarab-server}
  tag: ${TAG}
  pullPolicy: IfNotPresent
scarab:
  role: converged
  executor: k8s
  namespace: ${NS}
  devInsecure: ${DEV_INSECURE:-true}
  githubAppId: "${SCARAB_GITHUB_APP_ID:-}"
  publicUrl: "${SCARAB_PUBLIC_URL:-}"
  cloneImage: "${SCARAB_CLONE_IMAGE:-}"
  sidecarImage: "${SCARAB_SIDECAR_IMAGE:-ghcr.io/thulasi-ram/scarab-results-sidecar:edge}"
  stepTimeoutSecs: "${SCARAB_STEP_TIMEOUT_SECS:-3600}"
  extraEnv:
    RUST_LOG: "info,scarab=debug"
secrets:
  databaseUrl: "${SCARAB_DATABASE_URL}"
  masterKey: "${SCARAB_MASTER_KEY}"
  githubWebhookSecret: "${SCARAB_GITHUB_WEBHOOK_SECRET:-}"
  resultsTokenSecret: "${SCARAB_RESULTS_TOKEN_SECRET:-}"
YAML

echo "==> namespace"
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -

echo "==> in-cluster Postgres"
kubectl apply -n "$NS" -f "$ROOT/deploy/local-helm/postgres.yaml"
kubectl rollout status -n "$NS" deploy/scarab-postgres --timeout=120s

echo "==> scarab-server (image tag: $TAG)"
helm upgrade --install scarab "$ROOT/deploy/helm/scarab" -n "$NS" -f "$VALUES"
kubectl rollout status -n "$NS" deploy/scarab --timeout=180s

cat <<EOF

Deployed. Next:
  kubectl port-forward -n $NS svc/scarab 8899:80   # leave running; cloudflared -> :8899
  deploy/local-helm/reseed.sh                      # fresh DB only: store PEM + register
EOF
