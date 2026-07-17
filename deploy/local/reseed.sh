#!/usr/bin/env bash
# Seed a FRESH scarab DB with the GitHub App credential + connection (no browser):
#   1. store the App PEM at the reserved connection scope (_forge/github-app)
#   2. re-register the installation by POSTing a signed synthetic
#      `installation:created` (same path a real GitHub delivery takes)
#
# Config comes from deploy/local/.env (gitignored — see .env.example); a real
# environment variable already set in your shell overrides the file. Nothing
# sensitive is baked in — the webhook secret defaults to whatever the deployed
# k8s Secret already holds.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"

ENVFILE="${ENVFILE:-$HERE/.env}"
if [ -f "$ENVFILE" ]; then
  # File-wins (see deploy.sh). Split on the FIRST '=' so base64 values survive.
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      ''|\#*) continue ;;
      *=*) ;;
      *) continue ;;
    esac
    export "${line%%=*}=${line#*=}"
  done < "$ENVFILE"
fi

BASE="${BASE:-http://localhost:8899}"
NS="${NAMESPACE:-scarab}"
: "${SCARAB_APP_PEM:?set SCARAB_APP_PEM in deploy/local/.env}"
: "${SCARAB_INSTALL_ID:?set SCARAB_INSTALL_ID in deploy/local/.env}"
: "${SCARAB_ORG:?set SCARAB_ORG in deploy/local/.env}"
: "${SCARAB_REPO:?set SCARAB_REPO in deploy/local/.env}"
[ -f "$SCARAB_APP_PEM" ] || { echo "PEM not found: $SCARAB_APP_PEM" >&2; exit 1; }

# Default the webhook secret to whatever the running release already uses, so it
# is never hardcoded in a tracked file.
SECRET="${SCARAB_GITHUB_WEBHOOK_SECRET:-$(kubectl get secret -n "$NS" scarab -o jsonpath='{.data.SCARAB_GITHUB_WEBHOOK_SECRET}' 2>/dev/null | base64 -d || true)}"
[ -n "$SECRET" ] || { echo "no webhook secret (set SCARAB_GITHUB_WEBHOOK_SECRET or ensure the release Secret has it)" >&2; exit 1; }

echo "==> storing App PEM at _forge/github-app"
jq -Rs --arg org _forge --arg name github-app '{org:$org,name:$name,value:.}' < "$SCARAB_APP_PEM" \
  | curl -sf -o /dev/null -w 'PUT /v1/secrets -> HTTP %{http_code}\n' \
      -X POST "$BASE/v1/secrets" -H 'content-type: application/json' -d @-

echo "==> re-registering installation $SCARAB_INSTALL_ID via synthetic webhook"
BODY=$(printf '{"action":"created","installation":{"id":%s,"account":{"login":"%s"}},"repositories":[{"full_name":"%s/%s"}]}' \
  "$SCARAB_INSTALL_ID" "$SCARAB_ORG" "$SCARAB_ORG" "$SCARAB_REPO")
SIG="sha256=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$SECRET" | awk '{print $2}')"
curl -sf -o /dev/null -w 'POST /webhooks/github -> HTTP %{http_code}\n' \
  -X POST "$BASE/webhooks/github" -H 'content-type: application/json' \
  -H 'x-github-event: installation' -H "x-github-delivery: reseed-$$" \
  -H "x-hub-signature-256: $SIG" -d "$BODY"

echo "==> /v1/repos"; curl -s "$BASE/v1/repos"; echo
