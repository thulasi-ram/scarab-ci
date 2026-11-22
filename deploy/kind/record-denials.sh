#!/usr/bin/env bash
# Record the denials Kubernetes' built-in admission answers a Pod create with,
# for the executor's k8s_overlay classifier tests (git-bug 1d983fd):
# `just test record-denials`.
#
# Against the API server of the kind stack's cluster, through `kubectl proxy`
# on the stack's own kubeconfig (kc). The script:
#   - creates the namespace scarab-record-denials (deleted first if a run that
#     did not finish left it);
#   - checks that a dry-run create of one Pod (container `step`, busybox, cpu
#     limit 2, the label scarab.io/record-denials) passes with no policy;
#   - for each case, applies one policy, waits until a dry-run create of the
#     Pod is denied, then sends the create and the dry-run create
#     (`?dryRun=All`) and writes each answer's Status body to
#     crates/scarab-executor-k8s/tests/fixtures/denials/<case>.json and
#     <case>.dry_run.json; removes the policy and waits until the Pod passes
#     again:
#       vap_forbidden   a ValidatingAdmissionPolicy that refuses the label,
#                       with `reason: Forbidden` (403)
#       vap_invalid     the same policy with no reason (422)
#       resource_quota  a ResourceQuota of limits.cpu 1 (403, `exceeded quota`)
#       limit_range     a LimitRange of max cpu 1 per Container (403)
#       pod_security    the namespace labelled for Pod Security `restricted`
#                       (403)
#   - writes the Kubernetes version and the date to recorded_from.json;
#   - deletes the namespace, the policy and its binding.
#
# Kyverno and Gatekeeper are not recorded: each needs its controller's images
# pulled and installed into the cluster. Review the diff before committing it.
# No Scarab server is involved and nothing else in the cluster is changed.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=deploy/kind/lib.sh
. "$here/lib.sh"
# shellcheck source=deploy/kind/test-env.sh
. "$here/test-env.sh"   # free_port
state="$here/.state"

need curl; need python3; need kubectl

rns=scarab-record-denials   # the namespace
policy=scarab-record-denials # the ValidatingAdmissionPolicy and its binding
pod=record-denials
out="$repo_root/crates/scarab-executor-k8s/tests/fixtures/denials"

mkdir -p "$state" "$out"
work="$(mktemp -d "$state/record-denials.XXXXXX")"
proxy_pid=""
cleanup() {
  [ -z "$proxy_pid" ] || kill "$proxy_pid" 2>/dev/null || true
  kc delete validatingadmissionpolicybinding "$policy" --ignore-not-found >/dev/null 2>&1 || true
  kc delete validatingadmissionpolicy "$policy" --ignore-not-found >/dev/null 2>&1 || true
  kc delete namespace "$rns" --ignore-not-found --timeout=120s >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

port="$(free_port)"
kc proxy --address 127.0.0.1 --port "$port" >"$work/proxy.log" 2>&1 &
proxy_pid=$!
for _ in $(seq 1 30); do
  (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && break
  sleep 0.5
done
api="http://127.0.0.1:$port"
curl -sf "$api/version" >"$work/version.json" || { cat "$work/proxy.log" >&2; die "kubectl proxy did not answer"; }
version="$(python3 -I -c 'import json,sys;print(json.load(open(sys.argv[1]))["gitVersion"])' "$work/version.json")"
echo "==> Kubernetes $version"

echo "==> namespace $rns"
kc delete namespace "$rns" --ignore-not-found --timeout=120s >/dev/null
kc delete validatingadmissionpolicybinding "$policy" --ignore-not-found >/dev/null
kc delete validatingadmissionpolicy "$policy" --ignore-not-found >/dev/null
kc create namespace "$rns" >/dev/null

cat >"$work/pod.json" <<JSON
{
  "apiVersion": "v1",
  "kind": "Pod",
  "metadata": {"name": "$pod", "labels": {"scarab.io/record-denials": "deny"}},
  "spec": {
    "restartPolicy": "Never",
    "containers": [{
      "name": "step",
      "image": "busybox:1.36",
      "command": ["true"],
      "resources": {
        "requests": {"cpu": "2", "memory": "16Mi"},
        "limits": {"cpu": "2", "memory": "16Mi"}
      }
    }]
  }
}
JSON

post() { # $1 query ('' or '?dryRun=All'), $2 the file the answer goes to; prints the HTTP code
  curl -s -o "$2" -w '%{http_code}' -X POST -H 'content-type: application/json' \
    --data-binary @"$work/pod.json" "$api/api/v1/namespaces/$rns/pods$1"
}

# A policy just applied, or just removed, reaches admission a moment later.
await_dry_run() { # $1 the HTTP code wanted
  local got=""
  for _ in $(seq 1 60); do
    got="$(post '?dryRun=All' "$work/probe.json")"
    [ "$got" = "$1" ] && return 0
    sleep 1
  done
  cat "$work/probe.json" >&2
  die "a dry-run create of the Pod answered $got, not $1, for 60 s"
}

pretty() { # $1 in, $2 out
  python3 -I -c 'import json,sys;json.dump(json.load(open(sys.argv[1])),sys.stdout,indent=2);print()' \
    "$1" >"$2"
}

record() { # $1 case, $2 the HTTP code the denial has
  local code
  await_dry_run "$2"
  code="$(post '' "$work/create.json")"
  if [ "$code" = 201 ]; then
    kc delete pod -n "$rns" "$pod" --ignore-not-found >/dev/null
    die "$1: the API server admitted the Pod"
  fi
  [ "$code" = "$2" ] || { cat "$work/create.json" >&2; die "$1: the create answered $code, not $2"; }
  pretty "$work/create.json" "$out/$1.json"
  code="$(post '?dryRun=All' "$work/dry_run.json")"
  [ "$code" = "$2" ] || { cat "$work/dry_run.json" >&2; die "$1: the dry run answered $code, not $2"; }
  pretty "$work/dry_run.json" "$out/$1.dry_run.json"
  echo "    wrote $out/$1.json and $1.dry_run.json"
}

vap() { # $1 the validation's reason, or '' for none
  local reason=""
  [ -z "$1" ] || reason="    reason: $1"
  kc apply -f - >/dev/null <<YAML
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingAdmissionPolicy
metadata:
  name: $policy
spec:
  failurePolicy: Fail
  matchConstraints:
    resourceRules:
      - apiGroups: [""]
        apiVersions: ["v1"]
        operations: ["CREATE"]
        resources: ["pods"]
  validations:
  - expression: "!has(object.metadata.labels) || !('scarab.io/record-denials' in object.metadata.labels)"
    message: "the Pod has the label scarab.io/record-denials"
$reason
---
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingAdmissionPolicyBinding
metadata:
  name: $policy
spec:
  policyName: $policy
  validationActions: [Deny]
  matchResources:
    namespaceSelector:
      matchLabels:
        kubernetes.io/metadata.name: $rns
YAML
}
remove_vap() {
  kc delete validatingadmissionpolicybinding "$policy" >/dev/null
  kc delete validatingadmissionpolicy "$policy" >/dev/null
}

echo "==> the Pod passes a dry run with no policy"
await_dry_run 201

echo "==> vap_forbidden"
vap Forbidden
record vap_forbidden 403
remove_vap
await_dry_run 201

echo "==> vap_invalid"
vap ""
record vap_invalid 422
remove_vap
await_dry_run 201

echo "==> resource_quota"
kc apply -n "$rns" -f - >/dev/null <<YAML
apiVersion: v1
kind: ResourceQuota
metadata:
  name: record-denials
spec:
  hard:
    limits.cpu: "1"
YAML
# Until the quota controller fills in status.hard, admission answers
# "status unknown for quota", which is not the denial recorded here.
for _ in $(seq 1 60); do
  [ -n "$(kc get resourcequota record-denials -n "$rns" -o jsonpath='{.status.hard}')" ] && break
  sleep 1
done
record resource_quota 403
kc delete resourcequota record-denials -n "$rns" >/dev/null
await_dry_run 201

echo "==> limit_range"
kc apply -n "$rns" -f - >/dev/null <<YAML
apiVersion: v1
kind: LimitRange
metadata:
  name: record-denials
spec:
  limits:
    - type: Container
      max:
        cpu: "1"
YAML
record limit_range 403
kc delete limitrange record-denials -n "$rns" >/dev/null
await_dry_run 201

echo "==> pod_security"
kc label namespace "$rns" --overwrite \
  pod-security.kubernetes.io/enforce=restricted \
  pod-security.kubernetes.io/enforce-version=latest >/dev/null
record pod_security 403
kc label namespace "$rns" pod-security.kubernetes.io/enforce- \
  pod-security.kubernetes.io/enforce-version- >/dev/null

python3 -I - "$out/recorded_from.json" "$version" "$(date -u +%Y-%m-%d)" <<'PY'
import json, sys
path, version, day = sys.argv[1:]
with open(path, "w") as f:
    json.dump({"recorded_from": f"Kubernetes {version}, deploy/kind/record-denials.sh, {day}"}, f, indent=2)
    f.write("\n")
PY
echo "    wrote $out/recorded_from.json"
echo "==> done: review \`git diff $out\` and commit the recording"
