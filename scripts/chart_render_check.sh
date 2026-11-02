#!/usr/bin/env bash
# Assert what the chart renders for login, under the values a login-on
# install uses (the dogfood stack's shape: named providers, a UI URL that is
# not the public URL, no dev-insecure).
#
#   1. scarab.uiUrl reaches the ConfigMap as SCARAB_UI_URL, and nothing named
#      SCARAB_UI_URL renders when it is empty (the server then uses the
#      public URL).
#   2. Each scarab.oauth.providers entry's client secret reaches BOTH the
#      server Deployment and the Depot StatefulSet. The Depot reads the shared
#      ConfigMap, whose SCARAB_OAUTH_PROVIDERS names the variable each secret
#      is read from, and its config gate refuses the boot when one is unset.
#      Only the Deployment used to get them, so a login-on install's Depot
#      crash-looped.
#
# Run by `just check all`. Usage: scripts/chart_render_check.sh [chart-dir]
# (needs helm, yq v4).
set -euo pipefail

chart=${1:-deploy/helm/scarab}
fail() { echo "chart_render_check: $*" >&2; exit 1; }

login=(
  --set secrets.workspaceTokenSecret=x
  --set scarab.publicUrl=https://ci.example.com
  --set scarab.uiUrl=http://localhost:8080
  --set-json 'scarab.oauth.owners=["github:alice"]'
  --set-json 'scarab.oauth.providers=[{"id":"github","name":"GitHub","clientId":"Iv1.x","authorizeUrl":"https://github.com/login/oauth/authorize","tokenUrl":"https://github.com/login/oauth/access_token","userinfoUrl":"https://api.github.com/user","forgeUrl":"https://api.github.com","clientSecret":{"name":"scarab-oauth-github","key":"client-secret"}},{"id":"my-idp","clientId":"c","authorizeUrl":"https://idp.example/a","tokenUrl":"https://idp.example/t","userinfoUrl":"https://idp.example/u","clientSecret":{"name":"idp"}}]'
)
out=$(helm template t "$chart" "${login[@]}")

# 1. The UI URL.
got=$(yq 'select(.kind == "ConfigMap" and .metadata.name == "t-scarab") | .data.SCARAB_UI_URL' <<<"$out")
[ "$got" = "http://localhost:8080" ] || fail "SCARAB_UI_URL is '$got', want http://localhost:8080"
got=$(yq 'select(.kind == "ConfigMap" and .metadata.name == "t-scarab") | .data.SCARAB_PUBLIC_URL' <<<"$out")
[ "$got" = "https://ci.example.com" ] || fail "SCARAB_PUBLIC_URL is '$got', want https://ci.example.com"
plain=$(helm template t "$chart" --set secrets.workspaceTokenSecret=x)
[ "$(yq 'select(.kind == "ConfigMap" and .metadata.name == "t-scarab") | .data | has("SCARAB_UI_URL")' <<<"$plain")" = false ] \
  || fail "SCARAB_UI_URL renders with scarab.uiUrl empty"

# 2. Each provider's secret, in both workloads: "<env>=<secret>/<key>".
want="SCARAB_OAUTH_SECRET_GITHUB=scarab-oauth-github/client-secret
SCARAB_OAUTH_SECRET_MY_IDP=idp/client-secret"
for kind in Deployment StatefulSet; do
  got=$(yq "select(.kind == \"$kind\") | .spec.template.spec.containers[0].env[]
    | select(.name | test(\"^SCARAB_OAUTH_SECRET_\"))
    | .name + \"=\" + .valueFrom.secretKeyRef.name + \"/\" + .valueFrom.secretKeyRef.key" <<<"$out")
  [ "$got" = "$want" ] || fail "the $kind's provider secrets are
$got
want
$want"
done

echo "chart_render_check: SCARAB_UI_URL and the provider secrets render as expected"
