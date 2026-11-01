#!/usr/bin/env bash
# `just plugin-verify` — the first-party Argo CD Plugin on a REAL cluster
# (ADR-0072), on the kind release in its e2e mode:
#
#   build the plugin image → kind load → the release with plugins on and the
#   dev tag knob → a fake Argo CD on the host (the broker, in the server Pod,
#   reads it at the host address a Pod reaches) → install the plugin through
#   the API → bind its connection → wait for the isolation probe and the Pod →
#   wait until the Pod has read Argo CD through the broker's read proxy (its
#   standby check) → kill switch: the broker answers 403 and the workload is
#   scaled to zero.
#
# What it proves that the contract test cannot: the image runs under the
# restricted profile on a read-only root, the projected token verifies through
# TokenReview, the NetworkPolicy lets the Pod reach the broker (and the probe
# proves it reaches nothing else), and the kill switch stops a live Pod.
#
# ADR-0074 removed the Deployment record, so this no longer drives report
# ingest, an Environment's Deployments or a DeploymentGate.
#
# Idempotent: every run installs a NEW installation (names are never reused in
# an org). The release stays up in e2e mode with plugins on; `just up` switches
# it back to dev. The fake Argo CD is stopped on exit unless KEEP=1.
#
# Every cluster command goes through kc (deploy/kind/lib.sh): the stack's own
# kubeconfig, checked against the kind cluster before each call.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
# shellcheck source=deploy/kind/lib.sh
. "$here/lib.sh"
# shellcheck source=deploy/kind/test-env.sh
. "$here/test-env.sh"

org="${PLUGIN_VERIFY_ORG:-acme}"
repo="scarab.local/scarab-plugin-argocd"
image="$repo:verify"
keep="${KEEP:-0}"
state="$here/.state"

need() { command -v "$1" >/dev/null 2>&1 || die "'$1' not found on PATH"; }
need docker; need kind; need kubectl; need curl; need python3; need cargo

step() { echo "==> $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# JSON helpers: `jq`-free, python3 is already a dev-stack requirement.
json() { python3 -I -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {'d': d}))" "$1"; }

# --- 1. images + host binaries ------------------------------------------------
step "building the plugin image ($image)"
docker build -q -f "$root/docker/plugin-argocd/Dockerfile" -t "$image" "$root" >/dev/null
step "building the fake Argo CD (host)"
( cd "$root" && cargo build -q -p scarab-plugin-argocd --features fake --bin fake-argocd )
target_dir="${CARGO_TARGET_DIR:-$root/target}"
case "$target_dir" in /*) ;; *) target_dir="$root/$target_dir" ;; esac

# --- 2. the release, plugins on -----------------------------------------------
mkdir -p "$state"
values="$state/plugin-verify-values.json"
# kind images have no repository digest: the dev-only tag knob (owner default 8).
cat > "$values" <<EOF
{"plugins": {"enabled": true, "argocdRepo": "$repo"},
 "scarab": {"extraEnv": {"SCARAB_DEV_PLUGIN_ALLOW_TAG": "1"}}}
EOF
SCARAB_KIND_EXTRA_VALUES="$values" bash "$here/up.sh" e2e
scarab_addr
api="$base"
plugins_ns=scarab-plugins

step "loading $image into kind"
platform="linux/$(docker version -f '{{.Server.Arch}}')"
archive="$(mktemp "$state/image.XXXXXX")"
docker save --platform "$platform" -o "$archive" "$image"
kind load image-archive "$archive" --name "$cluster" >/dev/null
rm -f "$archive"

# --- 3. the fake Argo CD on the host -----------------------------------------
# The broker runs in the server Pod, so it reaches the fake at the host
# address a Pod uses; the fake binds 0.0.0.0 for that.
probe_pod_host
fake_port="${PLUGIN_VERIFY_FAKE_PORT:-$(free_port)}"
fake_pid_file="$state/fake-argocd.pid"
if [ -f "$fake_pid_file" ]; then
  old="$(cat "$fake_pid_file")"
  case "$(ps -o comm= -p "$old" 2>/dev/null)" in *fake-argocd*) kill "$old" 2>/dev/null || true; sleep 1 ;; esac
  rm -f "$fake_pid_file"
fi
if (exec 3<>"/dev/tcp/127.0.0.1/$fake_port") 2>/dev/null; then
  fail "127.0.0.1:$fake_port is taken by something else (set PLUGIN_VERIFY_FAKE_PORT)"
fi
argo_token="verify-$(date +%s)-$RANDOM"
FAKE_ARGOCD_TOKEN="$argo_token" nohup "$target_dir/debug/fake-argocd" --addr "0.0.0.0:$fake_port" \
  > "$state/fake-argocd.log" 2>&1 &
echo $! > "$fake_pid_file"
# The broker's port, for the whoami checks from the host.
forward "$release-broker" 8090
broker="http://127.0.0.1:$fwd_port"
cleanup() {
  code=$?
  stop_forwards
  if [ "$code" -ne 0 ]; then
    echo "---- tail of the server log ----" >&2; kc logs -n "$ns" deploy/"$release" --tail=60 >&2 || true
    if [ -n "${id:-}" ]; then
      echo "---- plugin Pod (plugin-$id) ----" >&2
      kc -n "$plugins_ns" get pods -l "scarab.io/plugin-installation=$id" -o wide >&2 || true
      kc -n "$plugins_ns" logs "deploy/plugin-$id" --all-containers --tail=60 >&2 || true
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
curl -sf "$fake/_fake/healthz" >/dev/null || fail "the fake Argo CD did not start (see $state/fake-argocd.log)"
fake_for_server="http://$pod_host:$fake_port"

# --- 4. install and bind -------------------------------------------------------
name="argo-$(date +%s)"
step "installing the plugin as $org/$name"
installed="$(curl -sf -X POST "$api/v1/orgs/$org/plugins" -H 'content-type: application/json' \
  -d '{"name":"'"$name"'","plugin":"argocd","image":"'"$image"'"}')" \
  || fail "install refused"
id="$(echo "$installed" | json 'd["id"]')"
echo "    installation id $id (ServiceAccount plugin-$id)"
curl -sf -X PUT "$api/v1/orgs/$org/plugins/$name/connections/argocd" -H 'content-type: application/json' \
  -d '{"base_url":"'"$fake_for_server"'","credential":"'"$argo_token"'"}' >/dev/null || fail "binding the connection"

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

# --- 5. the Pod reads Argo CD through the broker --------------------------------
# The plugin's standby check reads `GET /api/version` through the read proxy at
# start and then every minute; the fake records every request it serves.
step "waiting for the Pod's read of Argo CD through the broker (up to 3 min)"
for i in $(seq 1 180); do
  seen="$(curl -sf "$fake/_fake/requests" | json 'any(r["path_and_query"].startswith("/api/version") for r in d)' || echo False)"
  [ "$seen" = True ] && break
  [ "$i" = 180 ] && fail "the plugin Pod never read /api/version through the broker"
  sleep 1
done
echo "    ok: the Pod read Argo CD through the broker's read proxy"

step "every Argo request came through the broker with the bound credential"
curl -sf "$fake/_fake/requests" | python3 -I -c '
import json, sys
seen = json.load(sys.stdin)
bad = [r for r in seen if not r["authorized"]]
assert seen, "no request reached the fake Argo CD"
assert not bad, bad
print("    ok: %d requests, all authorized" % len(seen))
' || fail "a request reached Argo without the credential"

# --- 6. kill switch ------------------------------------------------------------
step "kill switch"
pod="$(kc -n "$plugins_ns" get pods -l "scarab.io/plugin-installation=$id" \
  --field-selector=status.phase=Running -o jsonpath='{.items[0].metadata.name}')"
uid="$(kc -n "$plugins_ns" get pod "$pod" -o jsonpath='{.metadata.uid}')"
# A token bound to the live Pod, like its projected one: TokenReview accepts it.
token="$(kc -n "$plugins_ns" create token "plugin-$id" --audience scarab-broker \
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
  replicas="$(kc -n "$plugins_ns" get deployment "plugin-$id" -o jsonpath='{.spec.replicas}' 2>/dev/null || echo gone)"
  [ "$replicas" = 0 ] && break
  [ "$i" = 60 ] && fail "plugin-$id still has replicas=$replicas a minute after the kill"
  sleep 2
done
echo "    ok: plugin-$id scaled to 0"

echo "==> plugin-verify PASSED ($org/$name, installation $id)"
echo "    the release stays in e2e mode with plugins on ('just up' switches back); the killed installation stays recorded"
