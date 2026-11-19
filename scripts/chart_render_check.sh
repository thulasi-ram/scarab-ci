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
#   3. The step namespace. Default values: `<release namespace>-steps`, a
#      Namespace kept on uninstall, the executor's Role and RoleBinding in it
#      bound to the server's ServiceAccount in the release namespace, and
#      SCARAB_NAMESPACE naming it. rbac.create=false, createNamespace=false
#      and executor=local each fall back to the release namespace with no
#      Namespace; an explicit scarab.namespace is used as given with no
#      Namespace; a derived name over 63 characters fails the render.
#   4. scarab.placement.author_overlay reaches the placement ConfigMap.
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

# 3. The step namespace. `ns_objects` prints "<kind> <name> <namespace>" for
# each Namespace, Role and RoleBinding the chart renders, sorted.
base=(--namespace ci --set secrets.workspaceTokenSecret=x)
ns_objects() {
  yq -N 'select(.kind == "Namespace" or .kind == "Role" or .kind == "RoleBinding")
    | .kind + " " + .metadata.name + " " + (.metadata.namespace // "")' <<<"$1" | sort
}
exec_ns() {
  yq -N 'select(.kind == "ConfigMap" and .metadata.name == "t-scarab") | .data.SCARAB_NAMESPACE' <<<"$1"
}
check_ns() { # <case> <want SCARAB_NAMESPACE> <want objects> <helm args...>
  local case=$1 want_ns=$2 want_objs=$3; shift 3
  local out got
  out=$(helm template t "$chart" "${base[@]}" "$@")
  got=$(exec_ns "$out")
  [ "$got" = "$want_ns" ] || fail "$case: SCARAB_NAMESPACE is '$got', want '$want_ns'"
  got=$(ns_objects "$out")
  [ "$got" = "$want_objs" ] || fail "$case: Namespace/Role/RoleBinding are
$got
want
$want_objs"
}

out=$(helm template t "$chart" "${base[@]}")
check_ns default ci-steps "Namespace ci-steps 
Role t-scarab-executor ci-steps
RoleBinding t-scarab-executor ci-steps"
got=$(yq -N 'select(.kind == "Namespace") | .metadata.annotations["helm.sh/resource-policy"]' <<<"$out")
[ "$got" = keep ] || fail "default: the step Namespace's resource-policy is '$got', want keep"
got=$(yq -N 'select(.kind == "RoleBinding") | .subjects[0].kind + " " + .subjects[0].name + " " + .subjects[0].namespace' <<<"$out")
[ "$got" = "ServiceAccount t-scarab ci" ] || fail "default: the executor RoleBinding's subject is '$got', want 'ServiceAccount t-scarab ci'"

check_ns "rbac.create=false" ci "" --set rbac.create=false
check_ns "scarab.createNamespace=false" ci "Role t-scarab-executor ci
RoleBinding t-scarab-executor ci" --set scarab.createNamespace=false
check_ns "scarab.namespace=builds" builds "Role t-scarab-executor builds
RoleBinding t-scarab-executor builds" --set scarab.namespace=builds
check_ns "scarab.executor=local" ci "Role t-scarab-executor ci
RoleBinding t-scarab-executor ci" --set scarab.executor=local

long=$(printf 'n%.0s' $(seq 1 60))
if err=$(helm template t "$chart" --namespace "$long" --set secrets.workspaceTokenSecret=x 2>&1); then
  fail "a 60-character release namespace rendered; want a failure naming the 66-character step namespace"
fi
grep -q "longer than 63 characters" <<<"$err" || fail "a 60-character release namespace failed with: $err"
helm template t "$chart" --namespace "$long" --set secrets.workspaceTokenSecret=x \
  --set scarab.namespace=builds >/dev/null \
  || fail "a 60-character release namespace with scarab.namespace set failed to render"

# 4. The author overlay lists reach the server's placement config.
out=$(helm template t "$chart" "${base[@]}" \
  --set-json 'scarab.placement.author_overlay={"label_keys":["team"],"annotation_keys":["karpenter.sh/do-not-disrupt"],"toleration_keys":["nvidia.com/gpu"]}')
got=$(yq -N 'select(.kind == "ConfigMap" and .metadata.name == "t-scarab-placement") | .data["placement.yaml"]' <<<"$out" \
  | yq -o=json -I=0 '.author_overlay')
want='{"annotation_keys":["karpenter.sh/do-not-disrupt"],"label_keys":["team"],"toleration_keys":["nvidia.com/gpu"]}'
[ "$got" = "$want" ] || fail "the placement ConfigMap's author_overlay is '$got', want '$want'"

echo "chart_render_check: SCARAB_UI_URL, the provider secrets, the step namespace and the placement config render as expected"
