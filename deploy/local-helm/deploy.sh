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

# Image SOURCE (repository + clone/sidecar refs) is a per-invocation choice —
# build locally vs pull a published tag — so a caller (e.g. `just local-helm`)
# may override it even though .env is otherwise file-wins for durable config +
# secrets. Capture caller-provided image refs now; re-apply after the .env load.
_img_repo="${IMAGE_REPOSITORY:-}"
_img_clone="${SCARAB_CLONE_IMAGE:-}"
_img_sidecar="${SCARAB_SIDECAR_IMAGE:-}"

# Load .env. It is authoritative (file-wins) for durable config + secrets: a
# stale ambient value — e.g. a direnv-loaded .env.local — must NOT silently
# override the in-cluster config.
ENVFILE="${ENVFILE:-$HERE/.env}"
if [ -f "$ENVFILE" ]; then
  # Split on the FIRST '=' only, so base64 values that end in '=' survive
  # (IFS='=' read would truncate them).
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

# Caller-provided image source wins over .env (see capture above).
[ -n "$_img_repo" ] && export IMAGE_REPOSITORY="$_img_repo"
[ -n "$_img_clone" ] && export SCARAB_CLONE_IMAGE="$_img_clone"
[ -n "$_img_sidecar" ] && export SCARAB_SIDECAR_IMAGE="$_img_sidecar"

# HARD GUARD: only ever touch the local colima cluster, never an ACME EKS ctx.
ctx="$(kubectl config current-context)"
[ "$ctx" = "colima" ] || { echo "refusing: kubectl context is '$ctx', not 'colima'." >&2; exit 1; }

NS="${NAMESPACE:-scarab}"
TAG="${1:-${IMAGE_TAG:-dogfood-local}}"

# Force a fresh Pod on every deploy. The image tags we use are MUTABLE — `edge`
# (ghcr) and `dogfood-local` (local build) never change string-wise — so a
# re-run of `helm upgrade` renders a byte-identical Deployment, K8s sees no diff,
# and the old Pod keeps running the stale image (pullPolicy: Always only re-pulls
# when a container is actually (re)created). A per-deploy annotation changes the
# Pod template each run, so Helm rolls a new Pod that pulls `edge` fresh / adopts
# the just-rebuilt `dogfood-local` — and `helm ... && kubectl rollout status`
# below waits on that roll.
DEPLOY_NONCE="$(date +%s)"

: "${SCARAB_MASTER_KEY:?set SCARAB_MASTER_KEY in .env}"
: "${SCARAB_DATABASE_URL:?set SCARAB_DATABASE_URL in .env}"

# The workspace service (ADR-0061) is in the STANDARD path, so the dogfood runs it
# by default rather than behind a flag — one path in every deployment mode, because
# two paths is two mental models.
#
# The secret is generated once and kept on disk rather than regenerated per
# deploy: the control plane mints tokens with it and the service verifies them, so
# a value that changed on every `just local-helm` would invalidate every
# in-flight Step's credential mid-run and look exactly like the service being
# down. Deliberately NOT SCARAB_RESULTS_TOKEN_SECRET — see .env.example.
WS_SECRET_FILE="$HERE/.workspace-token-secret"
if [ -z "${SCARAB_WORKSPACE_TOKEN_SECRET:-}" ]; then
  if [ ! -f "$WS_SECRET_FILE" ]; then
    head -c 32 /dev/urandom | base64 | tr -d '\n' > "$WS_SECRET_FILE"
    chmod 600 "$WS_SECRET_FILE"
    echo "==> generated a workspace token secret at deploy/local-helm/.workspace-token-secret"
  fi
  SCARAB_WORKSPACE_TOKEN_SECRET="$(cat "$WS_SECRET_FILE")"
fi
if [ "$SCARAB_WORKSPACE_TOKEN_SECRET" = "${SCARAB_RESULTS_TOKEN_SECRET:-}" ]; then
  echo "refusing: SCARAB_WORKSPACE_TOKEN_SECRET must differ from SCARAB_RESULTS_TOKEN_SECRET." >&2
  echo "  Sharing them turns a results-write credential into a content read+write" >&2
  echo "  credential and lets the workspace service forge step results (ADR-0061)." >&2
  exit 1
fi
WS_PV_SIZE="${SCARAB_WORKSPACE_PV_SIZE:-20Gi}"

# Object store: default to the in-cluster MinIO (minio.yaml). Backing the CAS
# with durable storage is REQUIRED — the local-dir fallback lives on the server
# Pod's `scratch` emptyDir, so a restart (every deploy now rolls the Pod) wipes
# all workspace snapshots and any rerun of a prior run hangs restoring them.
# Overridable via .env to point at real S3. Creds default to minio.yaml's.
S3_BUCKET="${SCARAB_S3_BUCKET:-scarab-logs}"
S3_ENDPOINT="${SCARAB_S3_ENDPOINT:-http://scarab-minio:9000}"
S3_REGION="${SCARAB_S3_REGION:-us-east-1}"
S3_ACCESS_KEY="${SCARAB_S3_ACCESS_KEY:-scarab}"
S3_SECRET_KEY="${SCARAB_S3_SECRET_KEY:-scarabsecret}"

# GitHub App PEM at BOOT (enh 245a99c) rather than a post-boot PUT /v1/secrets:
# the PEM is mounted from a k8s Secret this script maintains from the SAME
# .env path reseed.sh used, so a wiped/recreated DB no longer loses the App
# credential — only the installation registration has to be replayed. Skipped
# entirely when SCARAB_APP_PEM is unset (token mode / no App).
APP_PEM_SECRET="${APP_PEM_SECRET:-scarab-github-app}"
APP_PEM_KEY=github-app.pem
PEM_VALUES=""
if [ -n "${SCARAB_APP_PEM:-}" ]; then
  [ -f "$SCARAB_APP_PEM" ] || { echo "SCARAB_APP_PEM not found: $SCARAB_APP_PEM" >&2; exit 1; }
  PEM_VALUES="  githubAppPemSecret:
    name: \"${APP_PEM_SECRET}\"
    key: \"${APP_PEM_KEY}\""
fi

# Render a transient values file from .env (deleted on exit — no secrets on the
# CLI, none left on disk).
VALUES="$(mktemp)"
trap 'rm -f "$VALUES"' EXIT
cat > "$VALUES" <<YAML
image:
  repository: ${IMAGE_REPOSITORY:-scarab-server}
  tag: ${TAG}
  pullPolicy: ${IMAGE_PULL_POLICY:-IfNotPresent}
# Changes every deploy => Pod template differs => Helm rolls a fresh Pod that
# re-pulls the mutable tag (see DEPLOY_NONCE note above).
podAnnotations:
  scarab.dev/deployed-at: "${DEPLOY_NONCE}"
scarab:
  role: converged
  executor: k8s
  namespace: ${NS}
  s3:
    bucket: "${S3_BUCKET}"
    endpoint: "${S3_ENDPOINT}"
    region: "${S3_REGION}"
  devInsecure: ${DEV_INSECURE:-true}
  githubAppId: "${SCARAB_GITHUB_APP_ID:-}"
  publicUrl: "${SCARAB_PUBLIC_URL:-}"
  cloneImage: "${SCARAB_CLONE_IMAGE:-}"
  sidecarImage: "${SCARAB_SIDECAR_IMAGE:-ghcr.io/thulasi-ram/scarab-results-sidecar:edge}"
  stepTimeoutSecs: "${SCARAB_STEP_TIMEOUT_SECS:-3600}"
  extraEnv:
    RUST_LOG: "info,scarab=debug"
# The workspace service (ADR-0061) — a StatefulSet with a real PVC, in the
# standard path. This is also what kills the emptyDir-CAS failure class that has
# been biting this dogfood: the local-dir cold fallback lives on the server Pod's
# `scratch` emptyDir, so every deploy (which rolls the Pod) wiped every workspace
# snapshot and a rerun of an older Run hung at Init:1/3 and dead-lettered. A warm
# tier on a PV survives the roll.
workspace:
  enabled: true
  replicaCount: 1
  persistence:
    size: "${WS_PV_SIZE}"
secrets:
  databaseUrl: "${SCARAB_DATABASE_URL}"
  masterKey: "${SCARAB_MASTER_KEY}"
  githubWebhookSecret: "${SCARAB_GITHUB_WEBHOOK_SECRET:-}"
  resultsTokenSecret: "${SCARAB_RESULTS_TOKEN_SECRET:-}"
  workspaceTokenSecret: "${SCARAB_WORKSPACE_TOKEN_SECRET}"
  s3AccessKey: "${S3_ACCESS_KEY}"
  s3SecretKey: "${S3_SECRET_KEY}"
${PEM_VALUES}
YAML

echo "==> namespace"
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -

# The App PEM Secret the chart mounts (see PEM_VALUES above). Re-applied every
# deploy so rotating the file in .env is one `deploy.sh` away.
if [ -n "${SCARAB_APP_PEM:-}" ]; then
  echo "==> GitHub App PEM secret ($APP_PEM_SECRET)"
  kubectl create secret generic "$APP_PEM_SECRET" -n "$NS" \
    --from-file="$APP_PEM_KEY=$SCARAB_APP_PEM" \
    --dry-run=client -o yaml | kubectl apply -f -
fi

echo "==> in-cluster Postgres"
kubectl apply -n "$NS" -f "$ROOT/deploy/local-helm/postgres.yaml"
kubectl rollout status -n "$NS" deploy/scarab-postgres --timeout=120s

# In-cluster MinIO only when the server points at the in-cluster service. A
# real-S3 override (SCARAB_S3_ENDPOINT set to something else in .env) skips it.
if [ "$S3_ENDPOINT" = "http://scarab-minio:9000" ]; then
  echo "==> in-cluster MinIO"
  kubectl apply -n "$NS" -f "$ROOT/deploy/local-helm/minio.yaml"
  kubectl rollout status -n "$NS" deploy/scarab-minio --timeout=120s
  kubectl wait -n "$NS" --for=condition=complete job/scarab-minio-mkbucket --timeout=90s
fi

echo "==> scarab-server + workspace service (image tag: $TAG)"
helm upgrade --install scarab "$ROOT/deploy/helm/scarab" -n "$NS" -f "$VALUES"
kubectl rollout status -n "$NS" deploy/scarab --timeout=180s
# The workspace service is standard-path, so its readiness is a deploy gate, not a
# footnote: a Ready StatefulSet means its PVC bound AND its /readyz passed (warm
# writable + cold reachable). Silently deploying a control plane whose data plane
# never came up is the "reports success but structurally cannot work" shape.
echo "==> workspace service (ADR-0061)"
kubectl rollout status -n "$NS" statefulset/scarab-workspace --timeout=180s

cat <<EOF

Deployed. Next:
  kubectl port-forward -n $NS svc/scarab 8899:80   # leave running; cloudflared -> :8899
  deploy/local-helm/reseed.sh                      # fresh DB only: register the installation
EOF
