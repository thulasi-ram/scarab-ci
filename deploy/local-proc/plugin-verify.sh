#!/usr/bin/env bash
# `just plugin-verify` — the first-party Argo CD reporter Plugin on a REAL
# cluster (ADR-0072, ADR-0070 §11), in proc mode:
#
#   build the reporter image → kind load → `up.sh` with plugins on and the dev
#   tag knob → a fake Argo CD on the host (the broker, a host process, reads it
#   on 127.0.0.1) → install the plugin through the API → bind its connection →
#   list its project in an Environment → wait for the isolation probe and the
#   Pod → drive the fake through sync → hook failure → retries exhausted → fix
#   while polling the Environment's Deployments → kill switch: the broker
#   answers 403 and the workload is scaled to zero.
#
# What it proves that the contract test cannot: the image runs under the
# restricted profile on a read-only root, the projected token verifies through
# TokenReview, the NetworkPolicy lets the Pod reach the broker (and the probe
# proves it reaches nothing else), and the kill switch stops a live Pod.
#
# Idempotent: every run installs a NEW installation (names are never reused in
# an org) and re-points the Environment's source at it. The proc stack is left
# running (like `just kube-tests`); stop it with `just down`. The fake Argo CD
# is stopped on exit unless KEEP=1.
#
# Every cluster command goes through the ISOLATED kubeconfig, and the context
# is asserted to be kind-scarab-dev before the first one: production contexts
# sit beside the local one.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
kubeconfig="$here/.kubeconfig"
cluster="scarab-dev"
expected_context="kind-$cluster"

org="${PLUGIN_VERIFY_ORG:-acme}"
env_name="${PLUGIN_VERIFY_ENV:-prod}"
project="${PLUGIN_VERIFY_PROJECT:-prod}"
fake_port="${PLUGIN_VERIFY_FAKE_PORT:-18080}"
repo="scarab.local/scarab-plugin-argocd"
image="$repo:verify"
keep="${KEEP:-0}"

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: '$1' not found on PATH" >&2; exit 1; }; }
need docker; need kind; need kubectl; need curl; need python3; need cargo

assert_context() {
  local ctx
  ctx="$(KUBECONFIG="$kubeconfig" kubectl config current-context 2>/dev/null || true)"
  if [ "$ctx" != "$expected_context" ]; then
    echo "error: refusing to touch a cluster: context is '${ctx:-<none>}', expected '$expected_context' ($kubeconfig)" >&2
    exit 1
  fi
}
# Before up.sh (itself a cluster command) reuses an existing kubeconfig.
if [ -f "$kubeconfig" ]; then
  assert_context
fi

step() { echo "==> $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# JSON helpers: `jq`-free, python3 is already a dev-stack requirement.
json() { python3 -I -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {'d': d}))" "$1"; }

# --- 1. images + host binaries (no cluster yet) ---------------------------
step "building the reporter image ($image)"
docker build -q -f "$root/docker/plugin-argocd/Dockerfile" -t "$image" "$root" >/dev/null
step "building the fake Argo CD (host)"
( cd "$root" && cargo build -q -p scarab-plugin-argocd --features fake --bin fake-argocd )

# --- 2. the proc stack, plugins on ------------------------------------------
overlay="$here/.env.plugin-verify"
cat > "$overlay" <<EOF
# Written by deploy/local-proc/plugin-verify.sh — do not edit, do not commit.
SCARAB_PLUGINS=on
SCARAB_DEV_INSECURE=1
# kind images have no repository digest: the dev-only tag knob (owner default 8).
SCARAB_DEV_PLUGIN_ALLOW_TAG=1
SCARAB_PLUGIN_ARGOCD_REPO=$repo
EOF
SCARAB_ENV_EXTRA="$overlay" bash "$here/up.sh"
set -a
. "$here/.env"
. "$overlay"
set +a
export KUBECONFIG="$kubeconfig"
assert_context

api="http://${SCARAB_ADDR:-127.0.0.1:8080}"
api="${api/0.0.0.0/127.0.0.1}"
broker_addr="${SCARAB_BROKER_ADDR:-0.0.0.0:8090}"
broker="http://127.0.0.1:${broker_addr##*:}"
plugins_ns="${SCARAB_PLUGIN_NAMESPACE:-scarab-plugins}"

step "loading $image into kind"
kind load docker-image "$image" --name "$cluster" >/dev/null

# --- 3. the fake Argo CD on the host -----------------------------------------
fake_pid_file="$here/fake-argocd.pid"
if [ -f "$fake_pid_file" ]; then
  old="$(cat "$fake_pid_file")"
  case "$(ps -o comm= -p "$old" 2>/dev/null)" in *fake-argocd*) kill "$old" 2>/dev/null || true; sleep 1 ;; esac
  rm -f "$fake_pid_file"
fi
if (exec 3<>"/dev/tcp/127.0.0.1/$fake_port") 2>/dev/null; then
  fail "127.0.0.1:$fake_port is taken by something else (set PLUGIN_VERIFY_FAKE_PORT)"
fi
argo_token="verify-$(date +%s)-$RANDOM"
FAKE_ARGOCD_TOKEN="$argo_token" nohup "$root/target/debug/fake-argocd" --addr "127.0.0.1:$fake_port" \
  > "$here/fake-argocd.log" 2>&1 &
echo $! > "$fake_pid_file"
cleanup() {
  code=$?
  if [ "$code" -ne 0 ]; then
    echo "---- tail of deploy/local-proc/server.log ----" >&2; tail -n 60 "$here/server.log" >&2 || true
    if [ -n "${id:-}" ]; then
      echo "---- plugin Pod (plugin-$id) ----" >&2
      kubectl -n "$plugins_ns" get pods -l "scarab.io/plugin-installation=$id" -o wide >&2 || true
      kubectl -n "$plugins_ns" logs "deploy/plugin-$id" --all-containers --tail=60 >&2 || true
    fi
  fi
  if [ "$keep" != 1 ] && [ -f "$fake_pid_file" ]; then
    kill "$(cat "$fake_pid_file")" 2>/dev/null || true
    rm -f "$fake_pid_file"
  fi
  exit "$code"
}
trap cleanup EXIT
fake="http://127.0.0.1:$fake_port"
for _ in $(seq 1 30); do curl -sf "$fake/_fake/healthz" >/dev/null && break; sleep 0.5; done
curl -sf "$fake/_fake/healthz" >/dev/null || fail "the fake Argo CD did not start (see deploy/local-proc/fake-argocd.log)"

# One Application: argocd/web in project $project. Each call is a reconcile.
argo() { curl -sf -X PUT "$fake/_fake/apps/argocd/web" -H 'content-type: application/json' -d "$1" >/dev/null; }
web_base='"project":"'"$project"'","init_images":["ghcr.io/acme/web-init:1"],"hooks":[{"name":"migrate"}],"annotations":{"scarab.io/owner-projects":"'"$org"'/web"}'
argo '{'"$web_base"',"revision":"r1","images":["ghcr.io/acme/web:1"],"sync":"OutOfSync"}'

# --- 4. install, bind, route -------------------------------------------------
name="argo-$(date +%s)"
step "installing the plugin as $org/$name"
installed="$(curl -sf -X POST "$api/v1/orgs/$org/plugins" -H 'content-type: application/json' \
  -d '{"name":"'"$name"'","plugin":"argocd","image":"'"$image"'","config":{"ARGOCD_UI_URL":"'"$fake"'"}}')" \
  || fail "install refused"
id="$(echo "$installed" | json 'd["id"]')"
echo "    installation id $id (ServiceAccount plugin-$id)"
curl -sf -X PUT "$api/v1/orgs/$org/plugins/$name/connections/argocd" -H 'content-type: application/json' \
  -d '{"base_url":"'"$fake"'","credential":"'"$argo_token"'"}' >/dev/null || fail "binding the connection"
curl -sf -X PUT "$api/v1/orgs/$org/environments/$env_name" -H 'content-type: application/json' \
  -d '{"deployment_sources":[{"installation":"'"$name"'","vendor_grouping":"'"$project"'"}]}' >/dev/null \
  || fail "listing the source on $env_name"

step "waiting for the isolation probe and the plugin Pod"
for i in $(seq 1 150); do
  st="$(curl -sf "$api/v1/orgs/$org/plugins/$name")"
  status="$(echo "$st" | json 'd["runtime_status"]')"
  probed="$(echo "$st" | json 'd["probe_passed_at"] is not None')"
  if [ "$status" = running ] && [ "$probed" = True ]; then break; fi
  case "$status" in probe_failed|error)
    fail "runtime_status=$status: $(echo "$st" | json 'd["runtime_message"]')" ;;
  esac
  [ "$i" = 150 ] && fail "never running with a passed probe (runtime_status=$status, probe passed: $probed)"
  sleep 2
done
echo "    probe passed; runtime_status=running"

# Poll the target's Deployments (newest first) until the python predicate on
# `d` (the list) holds.
deployments() {
  curl -sf "$api/v1/orgs/$org/environments/$env_name/deployments?target_source=$name&target_id=argocd%2Fweb"
}
until_seen() {
  local what="$1" predicate="$2" timeout="${3:-120}" out=""
  for _ in $(seq 1 "$timeout"); do
    out="$(deployments || echo '[]')"
    if [ "$(echo "$out" | json "bool($predicate)")" = True ]; then
      echo "    ok: $what"
      return 0
    fi
    sleep 1
  done
  echo "$out" | python3 -I -m json.tool >&2 || true
  fail "never saw: $what"
}
last_label='(d[0]["reports"][-1]["label"] if d and d[0]["reports"] else "")'

# --- 5. drive Argo ------------------------------------------------------------
step "OutOfSync → awaiting auto-sync (the Pod's first report; up to 3 min for the Pod)"
until_seen "v1 in_progress, awaiting auto-sync" \
  'd and d[0]["version"]=="ghcr.io/acme/web-init:1,ghcr.io/acme/web:1" and d[0]["status"]=="in_progress"' 180
step "sync running its PreSync hook"
argo '{'"$web_base"',"revision":"r1","images":["ghcr.io/acme/web:1"],"sync":"OutOfSync","health":"Progressing","operation":{"phase":"Running","hook_results":[{"name":"migrate","phase":"Running"}]}}'
until_seen "hook migrate running" "$last_label=='hook migrate running'"
step "synced + healthy"
argo '{'"$web_base"',"revision":"r1","images":["ghcr.io/acme/web:1"],"operation":{"phase":"Succeeded"}}'
until_seen "v1 live" 'd and d[0]["status"]=="live"'

step "commit r2 (web:2): the migration fails, Argo retries"
failing='"revision":"r2","images":["ghcr.io/acme/web:2"],"sync":"OutOfSync","operation":{"phase":"%s","retry_count":%s,"message":"%s","hook_results":[{"name":"migrate","phase":"Failed","message":"Job has reached the specified backoff limit"}]}'
argo '{'"$web_base"','"$(printf "$failing" Running 1 'Retrying attempt #1')"'}'
until_seen "v2 retrying, still in_progress" \
  'd and d[0]["version"].endswith("web:2") and d[0]["status"]=="in_progress" and '"$last_label"'.startswith("retry 1/")'
step "retries exhausted"
argo '{'"$web_base"','"$(printf "$failing" Failed 2 'one or more synchronization tasks completed unsuccessfully (retried 2 times).')"'}'
until_seen "v2 failed, the hook named" 'd and d[0]["status"]=="failed" and "migrate" in '"$last_label"
step "the fix: r2 syncs"
argo '{'"$web_base"',"revision":"r2","images":["ghcr.io/acme/web:2"],"operation":{"phase":"Succeeded"}}'
until_seen "v2 live (failed → live)" 'd and d[0]["version"].endswith("web:2") and d[0]["status"]=="live"'

step "every Argo request came through the broker with the bound credential"
curl -sf "$fake/_fake/requests" | python3 -I -c '
import json, sys
seen = json.load(sys.stdin)
bad = [r for r in seen if not r["authorized"] or "refresh" in r["path_and_query"]]
assert seen, "no request reached the fake Argo CD"
assert not bad, bad
print("    ok: %d requests, all authorized, none asked for a refresh" % len(seen))
' || fail "a request reached Argo without the credential (or asked for a refresh)"

# --- 6. kill switch ------------------------------------------------------------
step "kill switch"
pod="$(kubectl -n "$plugins_ns" get pods -l "scarab.io/plugin-installation=$id" \
  --field-selector=status.phase=Running -o jsonpath='{.items[0].metadata.name}')"
uid="$(kubectl -n "$plugins_ns" get pod "$pod" -o jsonpath='{.metadata.uid}')"
# A token bound to the live Pod, like its projected one: TokenReview accepts it.
token="$(kubectl -n "$plugins_ns" create token "plugin-$id" --audience scarab-broker \
  --bound-object-kind Pod --bound-object-name "$pod" --bound-object-uid "$uid" --duration 10m)"
code="$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $token" "$broker/_scarab/plugin/v1/whoami")"
[ "$code" = 200 ] || fail "whoami before the kill answered $code, expected 200"
curl -sf -X POST "$api/v1/orgs/$org/plugins/$name/kill" -H 'content-type: application/json' -d '{}' >/dev/null \
  || fail "the kill call failed"
# The token's verdict is still cached (≤30 s), so this is the kill switch's own
# per-request check answering — not a token that died with its Pod.
code="$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $token" "$broker/_scarab/plugin/v1/whoami")"
[ "$code" = 403 ] || fail "whoami after the kill answered $code, expected 403"
echo "    ok: the broker refuses the killed installation (403)"
for i in $(seq 1 60); do
  replicas="$(kubectl -n "$plugins_ns" get deployment "plugin-$id" -o jsonpath='{.spec.replicas}' 2>/dev/null || echo gone)"
  [ "$replicas" = 0 ] && break
  [ "$i" = 60 ] && fail "plugin-$id still has replicas=$replicas a minute after the kill"
  sleep 2
done
echo "    ok: plugin-$id scaled to 0"

echo "==> plugin-verify PASSED ($org/$name, installation $id)"
echo "    the proc stack is still up ('just down' stops it); the killed installation stays recorded"
