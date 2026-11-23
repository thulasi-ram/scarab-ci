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
#   5. The step Pod admission policy (git-bug 07f0de3). It renders when the
#      cluster serves admissionregistration.k8s.io/v1 ValidatingAdmissionPolicy
#      (here: --api-versions) or scarab.admissionPolicy.force is set, and not
#      otherwise, nor when disabled or with the local executor. Its binding
#      selects the step namespace in both modes (`<release namespace>-steps`
#      and scarab.namespace = the release namespace), it matches the server's
#      ServiceAccount, and its allowed ServiceAccounts and Secrets carry what
#      the placement profiles name and scarab.admissionPolicy.allow* lists.
#      `enabled: auto` (default) is off when scarab.createNamespace is false
#      (a namespace-only installer cannot create cluster-scoped objects) and
#      on when set to true. The render fails when the placement config asks
#      for a host field, when step Pods share the release namespace with a
#      server whose ServiceAccount the policy allows, and on an `enabled`
#      that is not auto, true or false.
#   6. The server's startupProbe (/livez, 30 minutes by default), so a long
#      migration before the server listens is not killed by liveness.
#   7. retention.webhookDeliveryDays reaches the ConfigMap, 30 by default,
#      and 0 renders as "0" (not dropped for the default), so the server's
#      start check refuses it.
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

# 5. The step Pod admission policy. `vap_objects` prints "<kind> <name>" for
# the policy and its binding, sorted.
vap_api=(--api-versions admissionregistration.k8s.io/v1/ValidatingAdmissionPolicy)
vap_objects() {
  yq -N 'select(.kind == "ValidatingAdmissionPolicy" or .kind == "ValidatingAdmissionPolicyBinding")
    | .kind + " " + .metadata.name' <<<"$1" | sort
}
vap_want="ValidatingAdmissionPolicy scarab-step-pods.ci.t-scarab
ValidatingAdmissionPolicyBinding scarab-step-pods.ci.t-scarab"
check_vap() { # <case> <want objects> <helm args...>
  local case=$1 want=$2; shift 2
  local got
  got=$(vap_objects "$(helm template t "$chart" "${base[@]}" "$@")")
  [ "$got" = "$want" ] || fail "$case: the admission policy objects are
$got
want
$want"
}
check_vap "no ValidatingAdmissionPolicy API" ""
check_vap "the API served" "$vap_want" "${vap_api[@]}"
check_vap "admissionPolicy.force" "$vap_want" --set scarab.admissionPolicy.force=true
check_vap "admissionPolicy.enabled=false" "" "${vap_api[@]}" --set scarab.admissionPolicy.force=true \
  --set scarab.admissionPolicy.enabled=false
check_vap "scarab.executor=local" "" "${vap_api[@]}" --set scarab.executor=local
check_vap "scarab.namespace=ci" "$vap_want" "${vap_api[@]}" --set scarab.namespace=ci

vap_field() { # <kind> <yq path> <rendered>
  yq -N -o=json -I=0 "select(.kind == \"$1\") | $2" <<<"$3"
}
for mode in "ci-steps" "ci --set scarab.namespace=ci"; do
  # shellcheck disable=SC2086
  set -- $mode
  want_ns=$1; shift
  out=$(helm template t "$chart" "${base[@]}" "${vap_api[@]}" "$@")
  got=$(vap_field ValidatingAdmissionPolicyBinding '.spec.matchResources.namespaceSelector.matchExpressions' "$out")
  want="[{\"key\":\"kubernetes.io/metadata.name\",\"operator\":\"In\",\"values\":[\"$want_ns\"]}]"
  [ "$got" = "$want" ] || fail "step namespace $want_ns: the binding selects '$got', want '$want'"
  got=$(vap_field ValidatingAdmissionPolicy '.spec.matchConditions[0].expression' "$out")
  want='"request.userInfo.username == '"'"'system:serviceaccount:ci:t-scarab'"'"'"'
  [ "$got" = "$want" ] || fail "step namespace $want_ns: the policy matches '$got', want '$want'"
done
out=$(helm template t "$chart" "${base[@]}" "${vap_api[@]}")
got=$(vap_field ValidatingAdmissionPolicyBinding '.spec.validationActions' "$out")
[ "$got" = '["Deny"]' ] || fail "the default validationActions are '$got', want [\"Deny\"]"
got=$(vap_field ValidatingAdmissionPolicy '.spec.failurePolicy' "$out")
[ "$got" = '"Fail"' ] || fail "the default failurePolicy is '$got', want Fail"
got=$(vap_field ValidatingAdmissionPolicy '.spec.variables[] | select(.name == "serviceAccounts") | .expression' "$out")
[ "$got" = '"[\"default\"]"' ] || fail "with no placement the allowed ServiceAccounts are $got, want [\"default\"]"
got=$(vap_field ValidatingAdmissionPolicy '.spec.variables[] | select(.name == "ownSecrets") | .expression' "$out")
[ "$got" = "\"['v-secrets', 'v-token', 'v-registry', 'v-oidc', 'workspace', 'secrets', 'token', 'registry', 'oidc'].map(s, object.metadata.name + '-' + s)\"" ] \
  || fail "the per-Pod Secrets are $got"

out=$(helm template t "$chart" "${base[@]}" "${vap_api[@]}" \
  --set-json 'scarab.admissionPolicy.validationActions=["Audit"]' \
  --set-json 'scarab.placement={"baseline":{"spec":{"volumes":[{"name":"ca","secret":{"secretName":"ca-bundle"}}]}},"profiles":[{"name":"ecr-push","registry_auth_only":true,"k8s":{"spec":{"serviceAccountName":"ecr-pusher","initContainers":[{"name":"login","env":[{"name":"T","valueFrom":{"secretKeyRef":{"name":"login-token","key":"t"}}}]}]}}},{"name":"gpu","k8s":{"spec":{"serviceAccountName":"gpu-runner","volumes":[{"name":"p","projected":{"sources":[{"secret":{"name":"gpu-license"}}]}}]}}},{"name":"big"}]}')
got=$(vap_field ValidatingAdmissionPolicyBinding '.spec.validationActions' "$out")
[ "$got" = '["Audit"]' ] || fail "validationActions=[Audit] renders '$got'"
got=$(vap_field ValidatingAdmissionPolicy '.spec.variables[] | select(.name == "serviceAccounts") | .expression' "$out")
[ "$got" = '"[\"default\",\"ecr-pusher\",\"gpu-runner\"]"' ] \
  || fail "the allowed ServiceAccounts are $got, want default and the profiles' ecr-pusher and gpu-runner"
got=$(vap_field ValidatingAdmissionPolicy '.spec.variables[] | select(.name == "placementSecrets") | .expression' "$out")
[ "$got" = '"[\"ca-bundle\",\"login-token\",\"gpu-license\"]"' ] \
  || fail "the placement Secrets are $got, want ca-bundle, login-token and gpu-license"

check_vap "scarab.createNamespace=false" "" "${vap_api[@]}" --set scarab.createNamespace=false
check_vap "scarab.createNamespace=false, admissionPolicy.enabled=true" "$vap_want" "${vap_api[@]}" \
  --set scarab.createNamespace=false --set scarab.admissionPolicy.enabled=true

out=$(helm template t "$chart" "${base[@]}" "${vap_api[@]}" \
  --set-json 'scarab.admissionPolicy.allowSecrets=["ext-ca"]' \
  --set-json 'scarab.admissionPolicy.allowServiceAccounts=["ext-runner"]')
got=$(vap_field ValidatingAdmissionPolicy '.spec.variables[] | select(.name == "serviceAccounts") | .expression' "$out")
[ "$got" = '"[\"default\",\"ext-runner\"]"' ] || fail "allowServiceAccounts: the allowed ServiceAccounts are $got"
got=$(vap_field ValidatingAdmissionPolicy '.spec.variables[] | select(.name == "placementSecrets") | .expression' "$out")
[ "$got" = '"[\"ext-ca\"]"' ] || fail "allowSecrets: the allowed Secrets are $got"

refuses() { # <case> <want in the error> <helm args...>
  local case=$1 want=$2; shift 2
  local err
  if err=$(helm template t "$chart" "${base[@]}" "${vap_api[@]}" "$@" 2>&1); then
    fail "$case: rendered; want a failure naming '$want'"
  fi
  grep -qF -- "$want" <<<"$err" || fail "$case: failed with: $err"
}
host_placement='scarab.placement={"baseline":{"spec":{"volumes":[{"name":"cache","hostPath":{"path":"/c"}}]}},"profiles":[{"name":"net","k8s":{"spec":{"hostNetwork":true,"hostPID":true}}},{"name":"ipc","k8s":{"spec":{"hostIPC":true}}}]}'
refuses "a placement host field" \
  'the baseline (hostPath volume "cache"); profile "net" (hostNetwork, hostPID); profile "ipc" (hostIPC)' \
  --set-json "$host_placement"
refuses "a placement host field" "scarab.admissionPolicy.enabled=false" --set-json "$host_placement"
check_vap "a placement host field, admissionPolicy.enabled=false" "" "${vap_api[@]}" \
  --set-json "$host_placement" --set scarab.admissionPolicy.enabled=false
refuses "a shared namespace, the server on default" \
  "where the server's ServiceAccount is \"default\"" --set scarab.namespace=ci --set serviceAccount.create=false
refuses "a shared namespace, the server's ServiceAccount allowed" \
  "where the server's ServiceAccount is \"t-scarab\"" --set scarab.namespace=ci \
  --set-json 'scarab.admissionPolicy.allowServiceAccounts=["t-scarab"]'
check_vap "a shared namespace, serviceAccount.name=builder" "$vap_want" "${vap_api[@]}" \
  --set scarab.namespace=ci --set serviceAccount.create=false --set serviceAccount.name=builder
check_vap "the step namespace, the server on default" "$vap_want" "${vap_api[@]}" --set serviceAccount.create=false
refuses "admissionPolicy.enabled=maybe" "it takes auto, true or false" --set scarab.admissionPolicy.enabled=maybe

# 6. The startupProbe: /livez, and at least 30 minutes before liveness counts.
probe=$(yq 'select(.kind == "Deployment") | .spec.template.spec.containers[0].startupProbe' <<<"$plain")
[ "$(yq '.httpGet.path' <<<"$probe")" = /livez ] || fail "the server's startupProbe is
$probe"
budget=$(yq '.periodSeconds * .failureThreshold' <<<"$probe")
[ "$budget" -ge 1800 ] || fail "the server's startupProbe allows ${budget}s, want at least 1800s"

# 7. The webhook delivery retention: the default, and 0 kept as set.
delivery_days() {
  yq 'select(.kind == "ConfigMap" and .metadata.name == "t-scarab") | .data.SCARAB_RETENTION_WEBHOOK_DELIVERY_DAYS' \
    <<<"$(helm template t "$chart" "$@")"
}
got=$(delivery_days)
[ "$got" = "30" ] || fail "SCARAB_RETENTION_WEBHOOK_DELIVERY_DAYS is '$got' by default, want 30"
got=$(delivery_days --set retention.webhookDeliveryDays=0)
[ "$got" = "0" ] || fail "retention.webhookDeliveryDays=0 renders '$got', want 0 (the server refuses it)"

echo "chart_render_check: SCARAB_UI_URL, the provider secrets, the step namespace, the placement config, the step Pod admission policy, the server's startupProbe and the webhook delivery retention render as expected"
