#!/usr/bin/env bash
# Stand up a REAL Forgejo and seed it for the verification tier (git-bug 3863d5e):
#   - the container (compose.yaml), install-locked, webhooks allowed to loopback
#   - an admin user + an API access token
#   - enough repos to force `list_accessible_repos` onto a second page
#   - two working repos with a `.scarab/ci.yaml` on `main`
#
# Writes two gitignored env files the recipe and the tests read:
#   .env.generated  — SCARAB_TEST_FORGEJO_*   (the tests' contract)
#   .env.scarab     — SCARAB_* overlay for deploy/local-proc/up.sh
#
# Idempotent: safe to re-run. Requires docker + curl + python3.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
compose="docker compose -f $here/compose.yaml"

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: '$1' not found on PATH" >&2; exit 1; }; }
need docker; need curl; need python3

# Defaults live in the committed template; a local .env overrides them.
set -a
# shellcheck disable=SC1091
. "$here/.env.example"
[ -f "$here/.env" ] && . "$here/.env"
set +a

base="http://127.0.0.1:${FORGEJO_HTTP_PORT}"
owner="$FORGEJO_ADMIN_USER"
hook_repo="verify-hook"
onboard_repo="verify-onboard"

# Same rule as deploy/local-proc/up.sh: never seed, or grade, an instance this
# script did not start. Here the port bind would normally fail loudly inside
# `compose up -d`, but if a FOREIGN Forgejo already holds the port we would
# happily mint tokens and repos in someone else's instance — and the tier would
# report on it. Refuse instead, unless the listener is our own compose service
# (re-running this script is meant to be idempotent). /dev/tcp, not lsof: no new
# tool dependency beyond docker/curl/python3.
if (exec 3<>"/dev/tcp/127.0.0.1/$FORGEJO_HTTP_PORT") 2>/dev/null \
   && [ -z "$($compose ps -q forgejo 2>/dev/null)" ]; then
  {
    echo "error: something is already listening on 127.0.0.1:$FORGEJO_HTTP_PORT and it is not"
    echo "  this compose project's Forgejo (project: scarab-forgejo-verify)."
    echo "  Find it with:  lsof -nP -iTCP:$FORGEJO_HTTP_PORT -sTCP:LISTEN"
    echo "  Then stop it, or set FORGEJO_HTTP_PORT in deploy/local-forgejo/.env."
  } >&2
  exit 1
fi

echo "==> starting Forgejo ($FORGEJO_IMAGE) on $base"
$compose up -d

echo "==> waiting for the Forgejo API"
for _ in $(seq 1 90); do
  if curl -sf "$base/api/v1/version" >/dev/null 2>&1; then break; fi
  sleep 1
done
curl -sf "$base/api/v1/version" >/dev/null || {
  echo "error: Forgejo did not come up; logs:" >&2
  $compose logs --tail 50 forgejo >&2
  exit 1
}
echo "    $(curl -s "$base/api/v1/version")"

# `forgejo` CLI inside the container. The image exports GITEA_CUSTOM/WORK_DIR, but
# some builds do not, so fall back to the on-disk config path.
fj() {
  if ! $compose exec -T -u git forgejo forgejo "$@" 2>/dev/null; then
    $compose exec -T -u git forgejo forgejo --config /data/gitea/conf/app.ini "$@"
  fi
}

echo "==> ensuring admin user '$owner'"
if ! fj admin user create --admin --username "$owner" --password "$FORGEJO_ADMIN_PASSWORD" \
      --email "$FORGEJO_ADMIN_EMAIL" --must-change-password=false >/dev/null 2>&1; then
  echo "    (already exists)"
fi

echo "==> minting an API access token"
# Token names must be unique per user, so name each run's token for the run. The
# instance is throwaway; stale tokens die with the volume.
token_name="scarab-verify-$(date +%s)-$$"
token="$(fj admin user generate-access-token --username "$owner" \
           --token-name "$token_name" --scopes all --raw | tr -d '[:space:]')"
[ -n "$token" ] || { echo "error: could not mint an access token" >&2; exit 1; }

# Create a repo unless it already exists. $2=auto_init.
ensure_repo() {
  local name="$1" auto_init="$2" code
  code="$(curl -s -o /dev/null -w '%{http_code}' -H "authorization: token $token" \
            "$base/api/v1/repos/$owner/$name")"
  [ "$code" = "200" ] && return 0
  curl -sS -o /dev/null -X POST -H "authorization: token $token" \
    -H "content-type: application/json" \
    -d "{\"name\":\"$name\",\"auto_init\":$auto_init,\"default_branch\":\"main\",\"private\":false}" \
    "$base/api/v1/user/repos"
}

echo "==> seeding the two working repos"
ensure_repo "$hook_repo" true
ensure_repo "$onboard_repo" true

# The pipeline the onboarding leg's push must trigger. One step, no clone — the
# assertion is that a Run is CREATED from a real delivery, and the kind cluster
# cannot reach a Forgejo published on the docker host anyway.
ci_yaml=$(cat <<'YAML'
on:
  push: {}
steps:
  - id: verify
    image: busybox:latest
    security:
      run_as_root: true
    command: ["sh", "-c", "echo forgejo verification run"]
YAML
)
put_file() {
  local repo="$1" path="$2" content="$3" code
  code="$(curl -s -o /dev/null -w '%{http_code}' -H "authorization: token $token" \
            "$base/api/v1/repos/$owner/$repo/contents/$path?ref=main")"
  [ "$code" = "200" ] && return 0
  local b64; b64="$(printf '%s' "$content" | python3 -c 'import base64,sys;sys.stdout.write(base64.b64encode(sys.stdin.buffer.read()).decode())')"
  curl -sS -o /dev/null -X POST -H "authorization: token $token" \
    -H "content-type: application/json" \
    -d "{\"content\":\"$b64\",\"message\":\"seed $path\",\"branch\":\"main\"}" \
    "$base/api/v1/repos/$owner/$repo/contents/$path"
}
for r in "$hook_repo" "$onboard_repo"; do
  put_file "$r" ".scarab/ci.yaml" "$ci_yaml"
done

echo "==> seeding $FORGEJO_PAD_REPOS filler repos (forces a second /user/repos page)"
for i in $(seq -f '%03g' 1 "$FORGEJO_PAD_REPOS"); do
  ensure_repo "verify-pad-$i" false
done
total=$((FORGEJO_PAD_REPOS + 2))
echo "    $total repos reachable by the token"

echo "==> writing $here/.env.generated"
cat > "$here/.env.generated" <<EOF
# GENERATED by deploy/local-forgejo/up.sh — do not edit, do not commit.
SCARAB_TEST_FORGEJO=1
SCARAB_TEST_FORGEJO_URL=$base
SCARAB_TEST_FORGEJO_TOKEN=$token
SCARAB_TEST_FORGEJO_OWNER=$owner
SCARAB_TEST_FORGEJO_HOOK_REPO=$hook_repo
SCARAB_TEST_FORGEJO_ONBOARD_REPO=$onboard_repo
SCARAB_TEST_FORGEJO_REPO_TOTAL=$total
SCARAB_TEST_FORGEJO_CALLBACK_HOST=$FORGEJO_CALLBACK_HOST
SCARAB_TEST_FORGEJO_WEBHOOK_SECRET=$FORGEJO_WEBHOOK_SECRET
# The existing shared-contract run (crates/scarab-forge-forgejo/tests/contract_live.rs)
# reads owner/name from one variable.
SCARAB_TEST_FORGEJO_REPO=$owner/$hook_repo
EOF

echo "==> writing $here/.env.scarab (overlay for the local-proc server)"
cat > "$here/.env.scarab" <<EOF
# GENERATED by deploy/local-forgejo/up.sh — sourced by deploy/local-proc/up.sh
# AFTER its own .env, via SCARAB_ENV_EXTRA.
#
# 0.0.0.0, not 127.0.0.1: the Forgejo container delivers to the docker host, and
# a loopback-bound server is unreachable from it.
SCARAB_ADDR=0.0.0.0:$SCARAB_VERIFY_PORT
# The callback URL bind stamps onto the hook it registers — must be the address
# FORGEJO can reach, not the one a human types.
SCARAB_PUBLIC_URL=http://$FORGEJO_CALLBACK_HOST:$SCARAB_VERIFY_PORT
# The secret /webhooks/forgejo verifies with; the same one the adapter stamps
# into the hook it creates. Mismatch here = every delivery 401s.
SCARAB_FORGEJO_WEBHOOK_SECRET=$FORGEJO_WEBHOOK_SECRET
EOF

echo "==> Forgejo ready at $base (admin: $owner)"
