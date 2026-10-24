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
#
# THE DEPLOY RUN LEG (slice b; PLUGIN_VERIFY_DEPLOY_RUN=0 skips it, default on):
# after "v2 live" and before the kill switch, a real Deploy Run requests a
# Deployment and a DeploymentGate waits on what the plugin reports for it.
# A Run carries an Environment only when it is read from a repo through a
# forge, so this leg brings up the REAL Forgejo of deploy/local-forgejo (its
# up.sh, unchanged), seeds $org/web with .scarab/deploy.yaml, and binds it
# DECLARATIVELY (SCARAB_CONNECTIONS, a per-run id: no hook, no KEK). Then:
#   Run A (r3): `bump` writes the version as its `images` result → the result
#     is readable → a Deployment lists Run A as requester → the fake syncs r3
#     → the gate PASSES and Run A succeeds;
#   Run B (r4): same, then the migrate hook fails until retries are exhausted
#     → the gate FAILS naming the hook and Run B fails.
# Where `bump` runs is PLUGIN_VERIFY_DEPLOY_EXECUTOR:
#   local (default) — the server's host-process executor; its results are read
#     back by the executor itself. The plugin still runs in kind.
#   k8s — a kind Pod with the ADR-0042 results sidecar POSTing to the API
#     (SCARAB_RESULTS_API_URL derived by up.sh from its probe). Results arrive
#     through the sidecar and are overlaid at the step's success (f3004a1), so
#     the DeploymentRequest is completed from the ingested rows. Live status:
#     the last k8s-mode run (at 1eb7dc5) saw Run A's `bump` not reach a
#     terminal status within 240 s — this mode is NOT yet proven green.
#     SECURITY: this mode binds the API, which runs with SCARAB_DEV_INSECURE
#     (no authentication), on 0.0.0.0 — every interface — because a Pod must
#     reach it and up.sh discovers only the hostname a Pod uses (e.g.
#     host.docker.internal), not a host interface to bind. It says so loudly.
# The pipeline's gate step is ONE variable (deploy_gate_yaml below, or
# PLUGIN_VERIFY_DEPLOY_GATE_YAML). Forgejo is left running; stop it with
# `bash deploy/local-forgejo/down.sh`.
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

# --- the Deploy Run leg's knobs ----------------------------------------------
deploy_run="${PLUGIN_VERIFY_DEPLOY_RUN:-1}"
deploy_exec="${PLUGIN_VERIFY_DEPLOY_EXECUTOR:-local}"
case "$deploy_exec" in local|k8s) ;; *)
  echo "error: PLUGIN_VERIFY_DEPLOY_EXECUTOR=$deploy_exec (want local or k8s)" >&2; exit 1 ;;
esac
# The Forgejo repo is `web` because the fake Application below names its owner
# project `$org/web` (scarab.io/owner-projects).
deploy_repo="web"
sidecar_image="scarab-results-sidecar:verify"
fj_dir="$root/deploy/local-forgejo"
# The DeploymentGate step, in ONE place (the `gate: deployment` kind — slice b3).
# `deployment.step` names the requesting step; its target is implied because
# `bump` makes exactly one request.
default_gate_yaml() {
  cat <<'YAML'
  - id: wait-deploy
    gate: deployment
    needs: [bump]
    deployment: { step: bump }
YAML
}
deploy_gate_yaml="${PLUGIN_VERIFY_DEPLOY_GATE_YAML:-$(default_gate_yaml)}"
gate_step="${PLUGIN_VERIFY_DEPLOY_GATE_STEP:-wait-deploy}"
# What the reporter reports for each revision (scheme argocd.target-images.v1:
# the target manifests' image refs, hooks excluded, sorted, comma-joined — the
# fake's init_images count, its hooks do not).
v3="ghcr.io/acme/web-init:1,ghcr.io/acme/web:3"
v4="ghcr.io/acme/web-init:1,ghcr.io/acme/web:4"

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

# The port the dev env binds the API on (the overlay may rebind it on 0.0.0.0).
api_port="$( set -a; if [ -f "$here/.env" ]; then . "$here/.env"; else . "$here/.env.example"; fi; a="${SCARAB_ADDR:-127.0.0.1:8080}"; echo "${a##*:}" )"
# The server starts first, so a fake on the same port would only fail later as
# "taken" after a full stack bring-up.
if [ "$api_port" = "$fake_port" ]; then
  echo "error: the API (deploy/local-proc/.env SCARAB_ADDR) and the fake Argo CD both want :$fake_port — set PLUGIN_VERIFY_FAKE_PORT" >&2
  exit 1
fi

# --- 1. images + host binaries (no cluster yet) ---------------------------
step "building the reporter image ($image)"
docker build -q -f "$root/docker/plugin-argocd/Dockerfile" -t "$image" "$root" >/dev/null
step "building the fake Argo CD (host)"
( cd "$root" && cargo build -q -p scarab-plugin-argocd --features fake --bin fake-argocd )

# --- 1b. the Deploy Run leg's forge (before the server boots: the declarative
# connection is provisioned, and its repo bound, at boot) ---------------------
deploy_pipeline() {
  # Quoted heredoc: `${{ inputs.* }}` is Scarab's, `$SCARAB_*` the step's shell's.
  cat <<'YAML' | sed "s|@ENV@|$env_name|"
# Written by deploy/local-proc/plugin-verify.sh (the Deploy Run leg).
on:
  manual: {}
environment: @ENV@
interface:
  inputs:
    - { name: version, type: string, required: true }
    - { name: argo_source, type: string, required: true }
steps:
  - id: bump
    # k8s mode: kind-loaded, non-root (USER 65532), has sh. Local mode: ignored.
    image: scarab-results-sidecar:verify
    command:
      - sh
      - -c
      - |
        printf '"%s"' "$SCARAB_PARAM_VERSION" > "$SCARAB_RESULTS/images.json"
        echo "bump: requested $SCARAB_PARAM_VERSION"
    results: [images]
    deployment_requests:
      - target: { source: "${{ inputs.argo_source }}", id: argocd/web }
        version: images
YAML
  printf '%s\n' "$deploy_gate_yaml"
}

fj_url=""
fj_token=""
conn_id=""
if [ "$deploy_run" = 1 ]; then
  step "the Deploy Run leg: a real Forgejo (deploy/local-forgejo/up.sh)"
  bash "$fj_dir/up.sh"
  fj_url="$(sed -n 's/^SCARAB_TEST_FORGEJO_URL=//p' "$fj_dir/.env.generated")"
  fj_token="$(sed -n 's/^SCARAB_TEST_FORGEJO_TOKEN=//p' "$fj_dir/.env.generated")"
  [ -n "$fj_url" ] && [ -n "$fj_token" ] || fail "deploy/local-forgejo/.env.generated has no URL/token"
  step "seeding Forgejo $org/$deploy_repo with .scarab/deploy.yaml"
  FJ_URL="$fj_url" FJ_TOKEN="$fj_token" FJ_ORG="$org" FJ_REPO="$deploy_repo" \
  FJ_PATH=".scarab/deploy.yaml" FJ_CONTENT="$(deploy_pipeline)" python3 -I - <<'PY' || fail "seeding Forgejo"
import base64, json, os, sys, urllib.error, urllib.request

base = os.environ["FJ_URL"] + "/api/v1"
token = os.environ["FJ_TOKEN"]
org, repo, path = os.environ["FJ_ORG"], os.environ["FJ_REPO"], os.environ["FJ_PATH"]
content = os.environ["FJ_CONTENT"] + "\n"

def call(method, p, body=None):
    req = urllib.request.Request(
        base + p, method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={"authorization": "token " + token, "content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read()
            return r.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")

def need(ok, what, s, b):
    if not ok:
        sys.exit(f"{what}: HTTP {s}: {b}")

s, b = call("GET", f"/orgs/{org}")
if s == 404:
    s, b = call("POST", "/orgs", {"username": org})
    need(s == 201, f"creating org {org}", s, b)
else:
    need(s == 200, f"reading org {org}", s, b)
s, b = call("GET", f"/repos/{org}/{repo}")
if s == 404:
    s, b = call("POST", f"/orgs/{org}/repos",
                {"name": repo, "auto_init": True, "default_branch": "main", "private": False})
    need(s == 201, f"creating {org}/{repo}", s, b)
else:
    need(s == 200, f"reading {org}/{repo}", s, b)
# Upsert, not create-if-absent: an edited pipeline (the gate YAML) must reach the
# repo on the next run, or the run grades the OLD file.
want = base64.b64encode(content.encode()).decode()
s, b = call("GET", f"/repos/{org}/{repo}/contents/{path}?ref=main")
if s == 404:
    s, b = call("POST", f"/repos/{org}/{repo}/contents/{path}",
                {"content": want, "message": f"seed {path}", "branch": "main"})
    need(s == 201, f"creating {path}", s, b)
    print(f"    created {path}")
else:
    need(s == 200, f"reading {path}", s, b)
    if base64.b64decode(b["content"]).decode() != content:
        s, b = call("PUT", f"/repos/{org}/{repo}/contents/{path}",
                    {"content": want, "sha": b["sha"], "message": f"update {path}", "branch": "main"})
        need(s == 200, f"updating {path}", s, b)
        print(f"    updated {path}")
    else:
        print(f"    {path} unchanged")
PY
  # A NEW connection id per run: an id a previous run declared is handed back to
  # the database the first time the server boots without it (any plain `just
  # up`), and re-declaring a DB-owned id refuses the boot. A rebind of the same
  # repo just moves it to the new connection.
  conn_id="plugin-verify-forgejo-$(date +%s)"
fi

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
if [ "$deploy_run" = 1 ]; then
  cat >> "$overlay" <<EOF
# The Deploy Run leg: a config-owned Forgejo connection binding $org/$deploy_repo.
PLUGIN_VERIFY_FORGEJO_TOKEN=$fj_token
SCARAB_CONNECTIONS='{"connections":[{"id":"$conn_id","kind":"forgejo","base_url":"$fj_url","credential":{"env":"PLUGIN_VERIFY_FORGEJO_TOKEN"},"repos":["$org/$deploy_repo"]}]}'
EOF
  if [ "$deploy_exec" = local ]; then
    cat >> "$overlay" <<EOF
# Deploy Run steps run as host processes (their results are read back by the
# executor). Plugins still run in kind: the plugin runtime has its own client.
SCARAB_EXECUTOR=local
EOF
  else
    echo "WARNING: unauthenticated API (SCARAB_DEV_INSECURE) on all interfaces (0.0.0.0:$api_port) for k8s mode — anyone who can reach this host can drive it" >&2
    cat >> "$overlay" <<EOF
# Results egress (ADR-0042) from a step Pod: the sidecar POSTs to the API, so it
# must be Pod-reachable; up.sh derives SCARAB_RESULTS_API_URL from its probe.
# UNAUTHENTICATED (SCARAB_DEV_INSECURE) and on ALL interfaces: up.sh discovers
# only the hostname a Pod uses, not a host interface to bind.
SCARAB_ADDR=0.0.0.0:$api_port
SCARAB_RESULTS_TOKEN_SECRET=plugin-verify-results-$(date +%s)
SCARAB_SIDECAR_IMAGE=$sidecar_image
EOF
  fi
fi
SCARAB_ENV_EXTRA="$overlay" bash "$here/up.sh"
set -a
. "$here/.env"
. "$overlay"
set +a
export KUBECONFIG="$kubeconfig"
assert_context
if [ "$deploy_run" = 1 ] && [ "$deploy_exec" = k8s ]; then
  echo "WARNING: unauthenticated API (SCARAB_DEV_INSECURE) is listening on all interfaces ($SCARAB_ADDR) until \`just down\`" >&2
fi

api="http://${SCARAB_ADDR:-127.0.0.1:8080}"
api="${api/0.0.0.0/127.0.0.1}"
broker_addr="${SCARAB_BROKER_ADDR:-0.0.0.0:8090}"
broker="http://127.0.0.1:${broker_addr##*:}"
plugins_ns="${SCARAB_PLUGIN_NAMESPACE:-scarab-plugins}"

step "loading $image into kind"
kind load docker-image "$image" --name "$cluster" >/dev/null
if [ "$deploy_run" = 1 ] && [ "$deploy_exec" = k8s ]; then
  results_url="$(sed -n 's/^SCARAB_RESULTS_API_URL=//p' "$here/.env.generated")"
  [ -n "$results_url" ] || fail "up.sh derived no SCARAB_RESULTS_API_URL (results egress would be off)"
  echo "    step sidecars POST results to $results_url"
  step "building + loading the results sidecar / step image ($sidecar_image)"
  docker build -q -t "$sidecar_image" "$root/docker/sidecar" >/dev/null
  assert_context
  kind load docker-image "$sidecar_image" --name "$cluster" >/dev/null
fi

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
    for r in "${run_a:-}" "${run_b:-}"; do
      [ -n "$r" ] || continue
      echo "---- Deploy Run $r ----" >&2
      curl -s "$api/v1/runs/$r" | python3 -I -m json.tool >&2 || true
      if [ "$deploy_exec" = k8s ]; then
        echo "---- its Pods (the results sidecar is scarab-results-egress) ----" >&2
        kubectl -n "${SCARAB_NAMESPACE:-scarab}" logs -l "scarab.io/run=$r" --all-containers \
          --prefix --tail=40 >&2 || true
      fi
    done
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
if [ "$deploy_run" = 1 ]; then
  # A create takes the closed default (ADR-0071 §2): without this a dispatch of
  # a pipeline into $env_name answers 403.
  curl -sf -X PUT "$api/v1/orgs/$org/environments/$env_name/eligibility" -H 'content-type: application/json' \
    -d '{"all_projects":true}' >/dev/null || fail "making every project eligible for $env_name"
fi

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

# --- 5b. the Deploy Run leg -----------------------------------------------------
# A run's state as python: `d` the run, `st` its steps by id, `detail(id)` every
# attempt's failure class + detail. Prints True, False, or `terminal` (the run
# ended without the predicate holding — fail now, not after the timeout).
run_eval='
import json, sys
d = json.load(sys.stdin)
st = {s["id"]: s for s in d.get("steps", [])}
def detail(i):
    return " | ".join("%s: %s" % (a.get("failure"), a.get("failure_detail"))
                      for a in st.get(i, {}).get("attempt_list", []))
try:
    ok = bool(eval(sys.argv[1], {"d": d, "st": st, "detail": detail}))
except (KeyError, IndexError, TypeError):
    ok = False
done = d.get("status") in ("succeeded", "failed", "cancelled", "dead_lettered")
print("True" if ok else ("terminal" if done else "False"))
'
until_run() {
  local what="$1" r="$2" predicate="$3" timeout="${4:-180}" out="" verdict=""
  for _ in $(seq 1 "$timeout"); do
    out="$(curl -sf "$api/v1/runs/$r" || echo '{}')"
    verdict="$(echo "$out" | python3 -I -c "$run_eval" "$predicate")"
    case "$verdict" in
      True) echo "    ok: $what"; return 0 ;;
      terminal) break ;;
    esac
    sleep 1
  done
  echo "$out" | python3 -I -m json.tool >&2 || true
  if [ "$verdict" = terminal ]; then
    fail "run $r ended without: $what"
  fi
  fail "never saw (${timeout}s): $what (run $r)"
}
# Dispatch `deploy` at main with the version as a launch parameter; echoes the id.
dispatch_deploy() {
  local version="$1" label="$2" body resp code
  body='{"ref":"main","pipeline":"deploy","params":{"version":"'"$version"'","argo_source":"'"$name"'"},"reason":"plugin-verify '"$label"'"}'
  resp="$(curl -s -w '\n%{http_code}' -X POST "$api/v1/repos/$org/$deploy_repo/dispatch" \
    -H 'content-type: application/json' -d "$body")"
  code="${resp##*$'\n'}"
  resp="${resp%$'\n'*}"
  if [ "$code" != 201 ]; then
    echo "$resp" >&2
    fail "dispatching $label answered HTTP $code — a compile error naming \`gate\`/\`deployment\` means this build has no \`gate: deployment\` kind yet (slice b3)"
  fi
  echo "$resp" | json 'd["id"]'
}
# `bump` succeeded, its `images` result is readable, and a Deployment of the
# version lists this run as a requester that is NOT a claim.
assert_requested() {
  local r="$1" label="$2" version="$3" out bstatus bdetail res has
  until_run "$label: bump finished" "$r" 'st["bump"]["status"] in ("succeeded", "failed")' 240
  out="$(curl -sf "$api/v1/runs/$r")"
  bstatus="$(echo "$out" | json '[s for s in d["steps"] if s["id"] == "bump"][0]["status"]')"
  res="$(curl -sf "$api/v1/runs/$r/steps/bump/results" || echo '[]')"
  has="$(echo "$res" | json 'any(x["name"] == "images" and x["value"] == "'"$version"'" for x in d)')"
  if [ "$bstatus" != succeeded ]; then
    bdetail="$(echo "$out" | json '" | ".join("%s: %s" % (a.get("failure"), a.get("failure_detail")) for a in [s for s in d["steps"] if s["id"] == "bump"][0]["attempt_list"])')"
    if [ "$deploy_exec" = k8s ] && [ "$has" = True ]; then
      echo "---- attempt_results rows for bump (GET …/steps/bump/results) ----" >&2
      echo "$res" >&2
      fail "$label (k8s): the results sidecar delivered \`images\`=$version, and bump still failed: $bdetail"
    fi
    fail "$label: bump $bstatus ($bdetail); results: $res"
  fi
  [ "$has" = True ] || { echo "$res" >&2; fail "$label: GET …/steps/bump/results has no images=$version"; }
  echo "    ok: $label: bump succeeded, result images=$version"
  until_seen "$label: a Deployment of the new version, requested by run $r (not a claim)" \
    'any(x["version"] == "'"$version"'" and any(q["run"] == "'"$r"'" and not q["claim"] for q in x["requesters"]) for x in d)' 60
  until_run "$label: $gate_step waits (the Deployment is only requested)" "$r" \
    'st["'"$gate_step"'"]["status"] not in ("succeeded", "failed", "skipped", "cancelled")' 30
}
has_status() { echo 'any(x["version"] == "'"$1"'" and x["status"] == "'"$2"'" for x in d)'; }

run_a=""
run_b=""
if [ "$deploy_run" = 1 ]; then
  step "Deploy Run A (r3, executor: $deploy_exec): bump requests $v3"
  run_a="$(dispatch_deploy "$v3" "run-a-r3")"
  echo "    run $run_a"
  assert_requested "$run_a" "run A" "$v3"
  step "Argo picks up r3 (web:3) and syncs it"
  argo '{'"$web_base"',"revision":"r3","images":["ghcr.io/acme/web:3"],"sync":"OutOfSync"}'
  until_seen "v3 in_progress" "$(has_status "$v3" in_progress)"
  argo '{'"$web_base"',"revision":"r3","images":["ghcr.io/acme/web:3"],"operation":{"phase":"Succeeded"}}'
  until_seen "v3 live" "$(has_status "$v3" live)"
  until_run "run A: $gate_step PASSED and the run succeeded" "$run_a" \
    'st["'"$gate_step"'"]["status"] == "succeeded" and d["status"] == "succeeded"' 180

  # Dispatched only now: governed deploys into one Environment serialize.
  step "Deploy Run B (r4): bump requests $v4"
  run_b="$(dispatch_deploy "$v4" "run-b-r4")"
  echo "    run $run_b"
  assert_requested "$run_b" "run B" "$v4"
  step "r4's migrate hook fails, Argo retries, then gives up"
  failing4='"revision":"r4","images":["ghcr.io/acme/web:4"],"sync":"OutOfSync","operation":{"phase":"%s","retry_count":%s,"message":"%s","hook_results":[{"name":"migrate","phase":"Failed","message":"Job has reached the specified backoff limit"}]}'
  argo '{'"$web_base"','"$(printf "$failing4" Running 1 'Retrying attempt #1')"'}'
  until_seen "v4 retrying, still in_progress" "$(has_status "$v4" in_progress)"
  argo '{'"$web_base"','"$(printf "$failing4" Failed 2 'one or more synchronization tasks completed unsuccessfully (retried 2 times).')"'}'
  until_seen "v4 failed, the hook named" \
    'any(x["version"] == "'"$v4"'" and x["status"] == "failed" and x["reports"] and "migrate" in x["reports"][-1]["label"] for x in d)'
  until_run "run B: $gate_step FAILED naming the hook, and the run failed" "$run_b" \
    'st["'"$gate_step"'"]["status"] == "failed" and "migrate" in detail("'"$gate_step"'") and d["status"] == "failed"' 180
  echo "    gate reason: $(curl -sf "$api/v1/runs/$run_b" | json '" | ".join(str(a.get("failure_detail")) for a in [s for s in d["steps"] if s["id"] == "'"$gate_step"'"][0]["attempt_list"])')"
else
  step "Deploy Run leg skipped (PLUGIN_VERIFY_DEPLOY_RUN=$deploy_run)"
fi

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
if [ "$deploy_run" = 1 ]; then
  echo "    Deploy Run leg ($deploy_exec): run A $run_a passed its gate, run B $run_b failed it"
  echo "    Forgejo is still up (bash deploy/local-forgejo/down.sh stops it)"
fi
echo "    the proc stack is still up ('just down' stops it); the killed installation stays recorded"
