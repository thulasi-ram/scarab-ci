#!/usr/bin/env bash
# Deploy the PUBLIC Scarab demo onto the Oracle Always Free box: in-cluster
# Postgres + the Helm release (control plane + workspace service) + cloudflared.
# Reproducible; run it as many times as needed. Run it ON the box, as the
# unprivileged user bootstrap.sh gave a kubeconfig to.
#
# Config comes from deploy/demo-oracle/.env (gitignored — see .env.example). A
# real environment variable already set in your shell overrides the file for the
# IMAGE SOURCE only; everything else is file-wins. Pass an image tag to override
# IMAGE_TAG (e.g. a published `sha-<gitsha>`):
#
#   deploy/demo-oracle/deploy.sh [IMAGE_TAG]
#
# Modeled on deploy/local-helm/deploy.sh — same env-file-wins loader, same
# stable-workspace-token-on-disk handling, same refusal when the workspace token
# equals the results token, same "ask the image which roles it knows before
# touching the cluster" preflight. The differences are all consequences of this
# box being PUBLIC and having no Docker daemon; each is called out where it
# happens.
#
# ⚠ UNVERIFIED: this has never been run against a real Oracle instance.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

# Image SOURCE (repository + clone/sidecar/wsfetch refs) is a per-invocation
# choice, so a caller (`just demo-oracle`) may override it even though .env is
# otherwise file-wins for durable config + secrets. Capture caller-provided
# image refs now; re-apply after the .env load.
_img_repo="${IMAGE_REPOSITORY:-}"
_img_clone="${SCARAB_CLONE_IMAGE:-}"
_img_sidecar="${SCARAB_SIDECAR_IMAGE:-}"
_img_wsfetch="${SCARAB_WSFETCH_IMAGE:-}"

# Load .env. It is authoritative (file-wins) for durable config + secrets: a
# stale ambient value must NOT silently override what the public demo runs.
ENVFILE="${ENVFILE:-$HERE/.env}"
if [ -f "$ENVFILE" ]; then
  # Split on the FIRST '=' only, so base64 values that end in '=' survive
  # (IFS='=' read would truncate them) — the master key and the Cloudflare
  # tunnel token are both base64.
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      ''|\#*) continue ;;   # skip blank + comment lines
      *=*) ;;               # KEY=VALUE — process
      *) continue ;;        # anything else — skip
    esac
    _k=${line%%=*}
    _v=${line#*=}
    # Strip a TRAILING inline comment, and trailing whitespace with it. This
    # file is written to be read by humans — .env.example ships
    #   IMAGE_TAG=edge      # or a published sha-<gitsha>
    # and without this the value is the tag PLUS the comment, which reaches the
    # image preflight as `...:edge   # or a published sha-<gitsha>` and fails as
    # `invalid reference format`. That was the real first-deploy failure.
    #
    # The rule is `source`'s: a '#' only opens a comment when whitespace
    # precedes it, so a '#' inside a value (a password, say) is preserved. It
    # follows that a value which must END in whitespace, or must contain
    # " #" literally, cannot be expressed here — quote it and strip the quotes
    # yourself if that day ever comes. None of the values this deployment uses
    # (hex, base64url, URLs, image refs) contains a space at all.
    case "$_v" in
      *[[:space:]]\#*) _v=${_v%%[[:space:]]\#*} ;;
    esac
    while :; do
      case "$_v" in
        *[[:space:]]) _v=${_v%?} ;;
        *) break ;;
      esac
    done
    export "$_k=$_v"
  done < "$ENVFILE"
  unset _k _v
else
  echo "missing $ENVFILE (cp deploy/demo-oracle/.env.example deploy/demo-oracle/.env and fill it)" >&2
  exit 1
fi

# Caller-provided image source wins over .env (see capture above).
[ -n "$_img_repo" ] && export IMAGE_REPOSITORY="$_img_repo"
[ -n "$_img_clone" ] && export SCARAB_CLONE_IMAGE="$_img_clone"
[ -n "$_img_sidecar" ] && export SCARAB_SIDECAR_IMAGE="$_img_sidecar"
[ -n "$_img_wsfetch" ] && export SCARAB_WSFETCH_IMAGE="$_img_wsfetch"

# HARD GUARD: only ever touch the cluster this .env names. k3s writes `default`;
# the point is the same as local-helm's colima guard — a deploy script must
# never be pointable at a cluster you did not mean.
want_ctx="${KUBE_CONTEXT:-default}"
ctx="$(kubectl config current-context)"
[ "$ctx" = "$want_ctx" ] || {
  echo "refusing: kubectl context is '$ctx', not '$want_ctx' (KUBE_CONTEXT in $ENVFILE)." >&2
  exit 1
}

command -v helm >/dev/null 2>&1 || {
  echo "refusing: helm is not installed. k3s does not ship it:" >&2
  echo "  curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash" >&2
  exit 1
}

NS="${NAMESPACE:-scarab}"
TAG="${1:-${IMAGE_TAG:-edge}}"

# Force a fresh Pod on every deploy. `edge` is a MUTABLE tag — it never changes
# string-wise — so a re-run of `helm upgrade` renders a byte-identical Deployment,
# K8s sees no diff, and the old Pod keeps running the stale image (pullPolicy:
# Always only re-pulls when a container is actually (re)created). A per-deploy
# annotation changes the Pod template each run, so Helm rolls a new Pod that
# pulls `edge` fresh — and the `kubectl rollout status` calls below wait on it.
DEPLOY_NONCE="$(date +%s)"

# ---------------------------------------------------------------------------
# PUBLIC-DEMO REFUSALS. These run before anything else because getting them
# wrong on a box that is on the internet is not a config mistake, it is an
# incident.
# ---------------------------------------------------------------------------

# 1. dev-insecure makes EVERY caller — including every anonymous caller — a
#    synthetic Owner, with permission to dispatch runs and read stored secrets.
#    The chart also refuses to render it alongside scarab.oauth, but the chart's
#    error would arrive after this script had already applied Postgres and
#    cloudflared. Refuse here, where nothing has been touched.
case "${DEV_INSECURE:-}" in
  1|true|TRUE|yes|on)
    cat >&2 <<'EOF'
refusing: DEV_INSECURE is set, and this deployment is PUBLIC.

Dev-insecure downgrades the ADR-0048 security hard-fails to warnings: with no
authenticator, every caller becomes a synthetic Owner — anonymous visitors
included. On a box behind a Cloudflare tunnel with a real hostname, that is an
open control plane, not a relaxed dev setting.

Remove DEV_INSECURE from the .env and configure the OAuth login instead
(SCARAB_OAUTH_CLIENT_ID / _CLIENT_SECRET / SCARAB_OAUTH_OWNERS).
EOF
    exit 1
    ;;
esac

# 2. The two assumptions this whole mode rests on (see .env.example). There is
#    no sign-in allowlist in scarab-server: SCARAB_OAUTH_OWNERS only ELEVATES a
#    login to Owner, and every other authenticated subject is admitted as
#    Viewer — who can read every run's logs and artifacts, which is the repo's
#    source and build output. Letting arbitrary GitHub users in is only safe
#    because that content is already public.
if [ "${DEMO_ASSUME_PUBLIC_REPO:-true}" != "true" ]; then
  cat >&2 <<'EOF'
refusing: DEMO_ASSUME_PUBLIC_REPO is not "true".

This mode admits ANY GitHub user as Viewer, and a Viewer reads every run's logs
and artifacts. That is safe only while the demo repo is public.

scarab-server has no sign-in allowlist today — SCARAB_OAUTH_OWNERS elevates a
login to Owner, it does not restrict who may log in — so there is no value you
can set here that makes a private repo safe. Closing this gap is a server
feature (a subject allowlist at authenticate()), not a deploy knob.
EOF
  exit 1
fi

# ---------------------------------------------------------------------------
# Required config. `:?` so a missing value names itself instead of rendering an
# empty chart value the server then refuses to boot on 300 lines later.
# ---------------------------------------------------------------------------
: "${SCARAB_MASTER_KEY:?set SCARAB_MASTER_KEY in .env (STABLE across redeploys, or stored secrets are lost)}"
: "${SCARAB_DATABASE_URL:?set SCARAB_DATABASE_URL in .env}"
: "${SCARAB_PUBLIC_URL:?set SCARAB_PUBLIC_URL in .env (the tunnel hostname; it is also the OAuth callback base)}"
: "${SCARAB_S3_BUCKET:?set SCARAB_S3_BUCKET in .env (the object store is mandatory — ADR-0067 part 1)}"
: "${SCARAB_S3_ACCESS_KEY:?set SCARAB_S3_ACCESS_KEY in .env}"
: "${SCARAB_S3_SECRET_KEY:?set SCARAB_S3_SECRET_KEY in .env}"
: "${SCARAB_OAUTH_CLIENT_ID:?set SCARAB_OAUTH_CLIENT_ID in .env (real authn is not optional here)}"
: "${SCARAB_OAUTH_CLIENT_SECRET:?set SCARAB_OAUTH_CLIENT_SECRET in .env}"
: "${CLOUDFLARE_TUNNEL_TOKEN:?set CLOUDFLARE_TUNNEL_TOKEN in .env (there is no other ingress)}"

# An empty endpoint means AWS S3 to the client, silently — and these R2 keys are
# not AWS keys, so the first CAS write would fail with an auth error that reads
# like a wrong password. Name it here instead.
S3_ENDPOINT="${SCARAB_S3_ENDPOINT:-}"
[ -n "$S3_ENDPOINT" ] || {
  echo "refusing: SCARAB_S3_ENDPOINT is empty, which means AWS S3 — not R2." >&2
  echo "  Set https://<account_id>.r2.cloudflarestorage.com" >&2
  exit 1
}
S3_REGION="${SCARAB_S3_REGION:-auto}"

# Nobody can administer an install with no owners — including you, and there is
# no second way in: Owner is granted at login from this list only. A warning,
# not a refusal, because a deliberately read-only demo is a legitimate choice.
[ -n "${SCARAB_OAUTH_OWNERS:-}" ] || \
  echo "⚠ SCARAB_OAUTH_OWNERS is empty — EVERY login, including yours, will be a read-only Viewer." >&2

# ---------------------------------------------------------------------------
# The workspace token secret (ADR-0061).
#
# Generated once and kept on disk rather than regenerated per deploy: the
# control plane mints tokens with it and the service verifies them, so a value
# that changed on every deploy would invalidate every in-flight Step's
# credential mid-run and look exactly like the service being down.
# ---------------------------------------------------------------------------
WS_SECRET_FILE="$HERE/.workspace-token-secret"
if [ -z "${SCARAB_WORKSPACE_TOKEN_SECRET:-}" ]; then
  if [ ! -f "$WS_SECRET_FILE" ]; then
    head -c 32 /dev/urandom | base64 | tr -d '\n' > "$WS_SECRET_FILE"
    chmod 600 "$WS_SECRET_FILE"
    echo "==> generated a workspace token secret at deploy/demo-oracle/.workspace-token-secret"
  fi
  SCARAB_WORKSPACE_TOKEN_SECRET="$(cat "$WS_SECRET_FILE")"
fi
if [ "$SCARAB_WORKSPACE_TOKEN_SECRET" = "${SCARAB_RESULTS_TOKEN_SECRET:-}" ]; then
  echo "refusing: SCARAB_WORKSPACE_TOKEN_SECRET must differ from SCARAB_RESULTS_TOKEN_SECRET." >&2
  echo "  Sharing them turns a results-write credential into a content read+write" >&2
  echo "  credential and lets the workspace service forge step results (ADR-0061)." >&2
  exit 1
fi

# GitHub App PEM at BOOT rather than a post-boot PUT /v1/secrets: the PEM is
# mounted from a k8s Secret this script maintains from .env, so a wiped or
# restored DB no longer loses the App credential — only the installation
# registration has to be replayed (and on a publicly-reachable box, reinstalling
# the App on the repo replays it for you). Skipped when SCARAB_APP_PEM is unset.
APP_PEM_SECRET="${APP_PEM_SECRET:-scarab-github-app}"
APP_PEM_KEY=github-app.pem
PEM_VALUES=""
if [ -n "${SCARAB_APP_PEM:-}" ]; then
  [ -f "$SCARAB_APP_PEM" ] || { echo "SCARAB_APP_PEM not found: $SCARAB_APP_PEM" >&2; exit 1; }
  PEM_VALUES="  githubAppPemSecret:
    name: \"${APP_PEM_SECRET}\"
    key: \"${APP_PEM_KEY}\""
fi

OAUTH_SECRET="${OAUTH_SECRET:-scarab-oauth}"
OAUTH_SECRET_KEY=oauth-client-secret

# Render the transient half of the values from .env — secrets, per-install ids,
# and the image source. Deleted on exit: no secrets on the CLI, none left on
# disk. The DURABLE half (sizing, retention, OAuth endpoints) is the committed
# deploy/demo-oracle/values.yaml, passed first so this one wins on any overlap.
VALUES="$(mktemp)"
trap 'rm -f "$VALUES"' EXIT
cat > "$VALUES" <<YAML
image:
  repository: ${IMAGE_REPOSITORY:-ghcr.io/thulasi-ram/scarab-server}
  tag: ${TAG}
  pullPolicy: ${IMAGE_PULL_POLICY:-Always}
# Changes every deploy => Pod template differs => Helm rolls a fresh Pod that
# re-pulls the mutable tag (see DEPLOY_NONCE above).
podAnnotations:
  scarab.dev/deployed-at: "${DEPLOY_NONCE}"
scarab:
  s3:
    bucket: "${SCARAB_S3_BUCKET}"
    endpoint: "${S3_ENDPOINT}"
    region: "${S3_REGION}"
  githubAppId: "${SCARAB_GITHUB_APP_ID:-}"
  publicUrl: "${SCARAB_PUBLIC_URL}"
  cloneImage: "${SCARAB_CLONE_IMAGE:-}"
  sidecarImage: "${SCARAB_SIDECAR_IMAGE:-ghcr.io/thulasi-ram/scarab-results-sidecar:edge}"
  oauth:
    clientId: "${SCARAB_OAUTH_CLIENT_ID}"
    owners: [${SCARAB_OAUTH_OWNERS:+\"$(printf '%s' "$SCARAB_OAUTH_OWNERS" | sed 's/ *, */", "/g')\"}]
workspace:
  fetcherImage: "${SCARAB_WSFETCH_IMAGE:-ghcr.io/thulasi-ram/scarab-wsfetch:edge}"
secrets:
  databaseUrl: "${SCARAB_DATABASE_URL}"
  masterKey: "${SCARAB_MASTER_KEY}"
  githubWebhookSecret: "${SCARAB_GITHUB_WEBHOOK_SECRET:-}"
  resultsTokenSecret: "${SCARAB_RESULTS_TOKEN_SECRET:-}"
  workspaceTokenSecret: "${SCARAB_WORKSPACE_TOKEN_SECRET}"
  s3AccessKey: "${SCARAB_S3_ACCESS_KEY}"
  s3SecretKey: "${SCARAB_S3_SECRET_KEY}"
  # The client secret never passes through helm values — the chart wires it in
  # by reference from a Secret this script maintains.
  oauthClientSecret:
    name: "${OAUTH_SECRET}"
    key: "${OAUTH_SECRET_KEY}"
${PEM_VALUES}
YAML

# ---------------------------------------------------------------------------
# PREFLIGHT — before the Helm release, Postgres or the tunnel is touched.
# (It does create the namespace and one throwaway Pod, because that Pod IS the
# check; nothing durable is applied until it passes.)
#
# Same idea as local-helm's, DIFFERENT MECHANISM: there is no Docker daemon on a
# k3s box, so `docker run --rm <image> --role <nonsense>` is not available. The
# kubelet is asked instead, which is strictly better here — it proves the image
# is pullable BY THE NODE THAT WILL RUN IT (arm64 manifest included), not merely
# by some other machine, and it warms containerd's cache for the real deploy.
#
# Why it matters: the workspace service (ADR-0061) runs the SAME image as the
# server with SCARAB_ROLE=workspace, is deployed unconditionally, and its
# readiness is a hard deploy gate. Against an image whose scarab-server predates
# ADR-0061, clap rejects the role, the container exits 2, the Pod
# CrashLoopBackOffs, and `kubectl rollout status statefulset/scarab-workspace`
# sits for 180s before timing out with nothing naming the cause.
#
# Deliberately NOT a fallback: there is no flag here that disables the workspace
# service. A fast path plus a fallback path is two mental models.
# ---------------------------------------------------------------------------
SERVER_IMAGE="${IMAGE_REPOSITORY:-ghcr.io/thulasi-ram/scarab-server}:${TAG}"
WSFETCH_IMAGE="${SCARAB_WSFETCH_IMAGE:-ghcr.io/thulasi-ram/scarab-wsfetch:edge}"

echo "==> namespace $NS"
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -

# Run `<image> <args…>` to completion in a throwaway Pod and echo everything it
# printed. Never fails the script — the CALLER decides what the output means,
# because "the image printed nothing" and "the image printed the wrong thing"
# want different messages.
# `name` is a caller-supplied suffix, not a nicety: two preflights run in one
# script, and a Pod left behind by a failed first attempt would make the second
# `kubectl run` fail with AlreadyExists instead of with the real problem.
preflight_run() {
  local name="$1" image="$2"; shift 2
  kubectl delete pod -n "$NS" "scarab-preflight-$name" --ignore-not-found --wait >/dev/null 2>&1 || true
  kubectl run "scarab-preflight-$name" -n "$NS" \
    --rm --restart=Never --attach --quiet \
    --pod-running-timeout=300s \
    --image-pull-policy=Always \
    --image="$image" -- "$@" 2>&1 || true
}

echo "==> preflight: $SERVER_IMAGE (pulling on the node; this is the slow step on a cold box)"
# `--role <nonsense>` makes clap print its possible-values list and exit
# non-zero: no configuration, no database and no network needed, and it reports
# the truth for ANY image version rather than us inferring it from a tag name.
SERVER_OUT="$(preflight_run server "$SERVER_IMAGE" --role __scarab_preflight__)"
ROLES="$(printf '%s' "$SERVER_OUT" | sed -n 's/.*\[possible values: \([^]]*\)\].*/\1/p' | tr -d ' ')"
if [ -z "$ROLES" ]; then
  cat >&2 <<EOF
refusing: could not determine which roles $SERVER_IMAGE supports.

Expected \`scarab-server --role <invalid>\` to print a clap possible-values list.
What the preflight Pod actually printed:

$SERVER_OUT

Two common causes: the node could not pull the image (check that the tag exists
and that image.yml published an arm64 manifest for it — it builds arm64 on
native runners, so it should), or IMAGE_REPOSITORY in $ENVFILE does not point at
a scarab-server image at all.
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
deploy would have rolled the control plane fine, created
statefulset/scarab-workspace from the SAME image, watched it CrashLoopBackOff on
  error: invalid value 'workspace' for '--role <ROLE>'
and then blocked for 180s on \`kubectl rollout status\` before failing with a
timeout that names none of the above.

Deploy a published tag that contains ADR-0061 (\`edge\`, or an explicit
\`sha-<gitsha>\`). There is no local-build escape hatch in this mode — see
.env.example.
EOF
    exit 1
    ;;
esac
echo "    server $SERVER_IMAGE (roles: $ROLES)"

echo "==> preflight: $WSFETCH_IMAGE"
# The wsfetch image rides in EVERY workspace Step Pod (ADR-0061 s3-drain): the
# s3-feed fetcher init container on Steps with `needs:`, and the egress
# hold/drain helper on all of them. A missing image does NOT break this deploy —
# it breaks RUNS, later, quietly: every workspace Step sits in
# Init:ImagePullBackOff long after this script printed "Deployed."
#
# An image that EXISTS but predates s3-drain fails differently and is worth
# catching here too: a stale wsfetch has no subcommand parsing, so it ignores the
# argv and tries a real fetch instead of naming the skew.
WSFETCH_OUT="$(preflight_run wsfetch "$WSFETCH_IMAGE" __scarab_preflight__)"
case "$WSFETCH_OUT" in
  *"unknown subcommand"*hold*drain*) echo "    fetcher $WSFETCH_IMAGE (fetch/hold/drain)" ;;
  *)
    cat >&2 <<EOF
refusing: $WSFETCH_IMAGE is missing or predates the ADR-0061 s3-drain helper.

What the preflight Pod printed:

$WSFETCH_OUT

Expected the binary to reject an unknown subcommand by naming \`fetch\`, \`hold\`
and \`drain\`. This would NOT have failed the deploy — it would have failed every
RUN after the fact: either Init:ImagePullBackOff on every workspace Step, or a
drain that exits 0 with NO record and fails the Attempt as a Config error naming
the skew.

Point SCARAB_WSFETCH_IMAGE at a tag that exists and matches the server image.
EOF
    exit 1
    ;;
esac

# ---------------------------------------------------------------------------
# Secrets this script owns (all re-applied every deploy, so rotating a value in
# .env is one deploy.sh away).
# ---------------------------------------------------------------------------
if [ -n "${SCARAB_APP_PEM:-}" ]; then
  echo "==> GitHub App PEM secret ($APP_PEM_SECRET)"
  kubectl create secret generic "$APP_PEM_SECRET" -n "$NS" \
    --from-file="$APP_PEM_KEY=$SCARAB_APP_PEM" \
    --dry-run=client -o yaml | kubectl apply -f -
fi

echo "==> OAuth client secret ($OAUTH_SECRET)"
kubectl create secret generic "$OAUTH_SECRET" -n "$NS" \
  --from-literal="$OAUTH_SECRET_KEY=$SCARAB_OAUTH_CLIENT_SECRET" \
  --dry-run=client -o yaml | kubectl apply -f -

echo "==> Cloudflare tunnel token (scarab-cloudflared)"
kubectl create secret generic scarab-cloudflared -n "$NS" \
  --from-literal="token=$CLOUDFLARE_TUNNEL_TOKEN" \
  --dry-run=client -o yaml | kubectl apply -f -

# The nightly pg_dump CronJob's own credentials. Its OWN Secret rather than the
# chart's: a backup must not depend on chart-internal key names it does not own.
# POSTGRES_PASSWORD is not in .env.example on purpose — the credential is
# hardcoded in postgres.yaml (that Postgres has no route off the cluster and
# nothing on this box listens publicly). The override exists so changing it
# there is one place plus one env var, not a hunt.
echo "==> backup credentials (scarab-backup)"
kubectl create secret generic scarab-backup -n "$NS" \
  --from-literal="PGPASSWORD=${POSTGRES_PASSWORD:-scarab}" \
  --from-literal="S3_ACCESS_KEY=$SCARAB_S3_ACCESS_KEY" \
  --from-literal="S3_SECRET_KEY=$SCARAB_S3_SECRET_KEY" \
  --from-literal="S3_ENDPOINT=$S3_ENDPOINT" \
  --from-literal="S3_BUCKET=$SCARAB_S3_BUCKET" \
  --dry-run=client -o yaml | kubectl apply -f -

# The step namespace must exist BEFORE the Helm release: the chart renders the
# executor's Role/RoleBinding into it (scarab.namespace in values.yaml), and
# applying a Role to a namespace that does not exist fails the whole upgrade.
echo "==> step namespace + LimitRange"
kubectl apply -f "$ROOT/deploy/demo-oracle/steps.yaml"

echo "==> in-cluster Postgres (+ nightly dump to R2)"
kubectl apply -n "$NS" -f "$ROOT/deploy/demo-oracle/postgres.yaml"
kubectl rollout status -n "$NS" deploy/scarab-postgres --timeout=180s

echo "==> scarab-server + workspace service (image tag: $TAG)"
helm upgrade --install scarab "$ROOT/deploy/helm/scarab" -n "$NS" \
  -f "$ROOT/deploy/demo-oracle/values.yaml" -f "$VALUES"
kubectl rollout status -n "$NS" deploy/scarab --timeout=300s
# The workspace service is standard-path, so its readiness is a deploy gate, not
# a footnote: Ready means its PVC bound AND its /readyz passed — warm writable,
# and the R2 bucket reachable and writable. Deploying a control plane whose data
# plane never came up is the "reports success but structurally cannot work"
# shape this repo keeps finding.
echo "==> workspace service (ADR-0061)"

# Break the StatefulSet update deadlock BEFORE waiting on the rollout.
#
# A StatefulSet's RollingUpdate will not replace a Pod that has never become
# Ready. So if a template change fixes the very thing that was crash-looping,
# the controller still refuses to act: the StatefulSet carries the NEW revision,
# `scarab-workspace-0` keeps running the OLD one, and it keeps failing for a
# reason you already fixed. `rollout status` then burns its whole timeout and
# reports nothing about why — the cause is three layers down in a Pod log.
#
# Observed for real (2026-08-27): the chart was not projecting
# SCARAB_OAUTH_CLIENT_SECRET into this Pod, so it refused to boot on a partial
# OAuth provider. Fixing the chart changed nothing until the Pod was deleted by
# hand, after which it was Ready in 13 seconds.
#
# The check is exact rather than heuristic: compare the Pod's
# controller-revision-hash to the StatefulSet's updateRevision. Delete ONLY when
# they differ AND the Pod is not ready — i.e. it is provably running superseded
# spec and is not healthy, so nothing is lost by restarting it. A Pod that is
# merely slow to start is left alone.
_sts_rev=$(kubectl get sts scarab-workspace -n "$NS" \
  -o jsonpath='{.status.updateRevision}' 2>/dev/null || true)
_pod_rev=$(kubectl get pod scarab-workspace-0 -n "$NS" \
  -o jsonpath='{.metadata.labels.controller-revision-hash}' 2>/dev/null || true)
_pod_ready=$(kubectl get pod scarab-workspace-0 -n "$NS" \
  -o jsonpath='{.status.containerStatuses[0].ready}' 2>/dev/null || true)
if [ -n "$_pod_rev" ] && [ -n "$_sts_rev" ] && \
   [ "$_pod_rev" != "$_sts_rev" ] && [ "$_pod_ready" != "true" ]; then
  echo "    scarab-workspace-0 is stuck on a superseded revision and is not"
  echo "    ready ($_pod_rev, want $_sts_rev) — deleting it so the StatefulSet"
  echo "    can recreate it from the current spec."
  kubectl delete pod scarab-workspace-0 -n "$NS" --wait=false
fi
unset _sts_rev _pod_rev _pod_ready

kubectl rollout status -n "$NS" statefulset/scarab-workspace --timeout=300s

# cloudflared LAST, deliberately: it is the front door, and there is no point
# opening it onto a control plane that has not come up. Its readiness probe
# reads cloudflared's own /ready, which means an edge connection is registered —
# so a Ready Pod here is a real statement about the public URL working.
echo "==> cloudflared (ingress)"
kubectl apply -n "$NS" -f "$ROOT/deploy/demo-oracle/cloudflared.yaml"
kubectl rollout status -n "$NS" deploy/scarab-cloudflared --timeout=180s

cat <<EOF

Deployed. ${SCARAB_PUBLIC_URL} should serve the UI within a few seconds.

  kubectl logs -n $NS deploy/scarab -f                 # control plane
  kubectl logs -n $NS statefulset/scarab-workspace -f  # warm CAS / drains
  kubectl logs -n $NS deploy/scarab-cloudflared -f     # the tunnel
  kubectl get pods -n scarab-steps                     # step Pods live HERE

Fresh install only: install the GitHub App on the demo repo (or hit
"Recreate all" on its Advanced webhook page) — the resulting \`installation\`
delivery is what registers it. Unlike local-helm there is no reseed.sh: the box
is publicly reachable, so real GitHub deliveries arrive on their own.
EOF
