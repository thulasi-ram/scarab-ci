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
_img_wsfetch="${SCARAB_WSFETCH_IMAGE:-}"

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
[ -n "$_img_wsfetch" ] && export SCARAB_WSFETCH_IMAGE="$_img_wsfetch"

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
  # The ADR-0061 s3-feed fetcher. `just local-helm local` builds it from the tree
  # like the other three images; otherwise it tracks the published tag.
  # ⚠ DELETE ME with the node driver (git-bug 0628369).
  fetcherImage: "${SCARAB_WSFETCH_IMAGE:-ghcr.io/thulasi-ram/scarab-wsfetch:edge}"
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

# ---------------------------------------------------------------------------
# PREFLIGHT — before a single cluster object is touched.
#
# The workspace service (ADR-0061) runs the SAME image as the server with
# SCARAB_ROLE=workspace, and it is deployed unconditionally, and its readiness is
# a hard deploy gate. All three are deliberate. Together they mean the deploy
# CANNOT succeed against an image whose `scarab-server` predates ADR-0061 — clap
# rejects the role, the container exits 2, the Pod CrashLoopBackOffs, and
# `kubectl rollout status statefulset/scarab-workspace` sits there for 180s before
# timing out with nothing that names the cause.
#
# `edge` and `sha-*` are built by image.yml from `main`, so until ADR-0061 merges
# every mode except `just local-helm local` is in exactly that state. Failing here
# instead, with the reason and the way out, is the whole point of this block.
#
# Deliberately NOT a fallback: there is no flag here that disables the workspace
# service. ADR-0061 puts it in the standard path in every deployment mode because
# a fast path plus a fallback path is two mental models, and the moment
# `workspace.enabled=false` becomes a documented escape hatch, the dogfood stops
# exercising the thing being dogfooded.
# ---------------------------------------------------------------------------
SERVER_IMAGE="${IMAGE_REPOSITORY:-scarab-server}:${TAG}"
WSFETCH_IMAGE="${SCARAB_WSFETCH_IMAGE:-ghcr.io/thulasi-ram/scarab-wsfetch:edge}"

# Make an image ref available locally, pulling only if it is not already there
# (`just local-helm local` builds into the Docker store and must not be
# overwritten by a pull).
ensure_image() {
  docker image inspect "$1" >/dev/null 2>&1 && return 0
  echo "==> pulling $1"
  docker pull "$1" >/dev/null 2>&1
}

echo "==> preflight: images"
if ! ensure_image "$SERVER_IMAGE"; then
  cat >&2 <<EOF
refusing: cannot obtain the server image
    $SERVER_IMAGE
It is neither in the local Docker store nor pullable. The kubelet will not manage
any better, so this would deploy straight into ImagePullBackOff.

  * to deploy the working tree:   just local-helm local
  * to deploy a published build:  just local-helm sha-<gitsha>   (see the ghcr
    package list for tags image.yml has actually published)
EOF
  exit 1
fi

# Ask the image itself which roles it knows. `--role <nonsense>` makes clap print
# its possible-values list and exit non-zero, which needs no configuration, no
# database and no network — and it reports the truth for ANY image version rather
# than us inferring it from a tag name.
ROLES="$(docker run --rm "$SERVER_IMAGE" --role __scarab_preflight__ 2>&1 \
          | sed -n 's/.*\[possible values: \([^]]*\)\].*/\1/p' | tr -d ' ')"
if [ -z "$ROLES" ]; then
  cat >&2 <<EOF
refusing: could not determine which roles $SERVER_IMAGE supports.
Expected \`scarab-server --role <invalid>\` to print a clap possible-values list.
This image may not be a scarab-server image at all. Check IMAGE_REPOSITORY in
$ENVFILE.
EOF
  exit 1
fi
case ",$ROLES," in
  *,workspace,*) ;;
  *)
    cat >&2 <<EOF
refusing: $SERVER_IMAGE does not support --role workspace.

  it supports: $ROLES

That image was built from a commit without the ADR-0061 workspace service. This
deploy would have:
  1. rolled the control plane fine,
  2. created statefulset/scarab-workspace from the SAME image,
  3. had the container exit 2 with
       error: invalid value 'workspace' for '--role <ROLE>'
     into CrashLoopBackOff,
  4. and blocked for 180s on \`kubectl rollout status\` before failing with a
     timeout that names none of the above.

The workspace service is standard-path (ADR-0061), so it is not disabled to work
around this. Two honest ways forward:

  * build from this working tree:
      just local-helm local
  * deploy a published tag that CONTAINS ADR-0061 (i.e. after it merges to main
    and image.yml republishes):
      just local-helm sha-<gitsha>
EOF
    exit 1
    ;;
esac

# The wsfetch image rides in EVERY workspace Step Pod since ADR-0061 s3-drain:
# the s3-feed fetcher init container (Steps with "needs:") and the egress
# helper (`scarab-wsfetch hold` / the in-Pod `drain`) on all of them. A missing
# image does NOT break this deploy — it breaks *runs*, later, quietly: every
# workspace Step sits in Init:ImagePullBackOff while this script has already
# printed "Deployed." Check it here, where the message can say so. An image
# that EXISTS but predates s3-drain fails differently and legibly: a stale
# wsfetch has no subcommands, so the drain exec ignores its argv, runs fetch
# and exits 0 with NO drain record — and the Attempt fails promptly as a
# Config error naming the skew.
if ! ensure_image "$WSFETCH_IMAGE"; then
  cat >&2 <<EOF
refusing: cannot obtain the workspace fetcher image
    $WSFETCH_IMAGE

This would NOT have failed the deploy — it would have failed every RUN, after
the fact: the wsfetch image is the fetcher init container of every Step Pod
that inherits a workspace AND the egress drain helper of every workspace Step
(ADR-0061 s3-drain), so each one would sit in Init:ImagePullBackOff long
after this script printed "Deployed."

\`ghcr.io/thulasi-ram/scarab-wsfetch:edge\` does not exist until image.yml
publishes it, which happens post-merge. Until then:

  * build it from this working tree:  just local-helm local
  * or point SCARAB_WSFETCH_IMAGE at a tag that exists.
EOF
  exit 1
fi
echo "    server $SERVER_IMAGE (roles: $ROLES)"
echo "    fetcher $WSFETCH_IMAGE"

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
