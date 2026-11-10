#!/usr/bin/env bash
# `just test e2e [filter]`: every live suite against the kind release in its
# e2e mode (own database `scarab_e2e`, bucket `scarab-e2e`, plugins on, a quick
# tunnel in front of the in-cluster Forgejo). The test binaries run on the
# host; test-env.sh gives them port-forwards and the address a Pod reaches the
# host at.
#
# Passes, in order (each nextest pass takes the optional nextest filter):
#   1. crates/scarab-e2e, all but crash_resume and the Forgejo cases
#   2. the Forgejo adapter against the real Forgejo (scarab-forge-forgejo live)
#   3. Forgejo onboarding through the server (one test at a time: each one
#      reclaims the Forgejo connection first)
#   4. the forge_token cases through the tunnel
#   5. the executor's and the plugin runtime's live cluster suites, in
#      namespace scarab-kube-tests (the test process stands in for the
#      control plane, so it gets the Depot, its bucket and its token secret)
#   6. the Argo CD plugin against a fake Argo CD on the host
#      (plugin-verify.sh, ADR-0072). Not nextest: it runs when there is no
#      filter or the filter is part of its name, `plugin-argocd`. It is a
#      child process with its own EXIT trap, which stops the fake Argo CD and
#      its port-forward however the pass ends.
#   7. crash_resume, alone and last: it kills the server every case talks to
#
# All passes run; the exit code is non-zero when any failed. Zero
# auto-retries: a red run is a real bug, not flake to mask. The release stays
# in e2e mode afterwards (`just stack up` switches back); the tunnel is
# deleted.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
filter="${1:-}"
cd "$root"

# shellcheck source=deploy/kind/lib.sh
. "$here/lib.sh"
# shellcheck source=deploy/kind/test-env.sh
. "$here/test-env.sh"
tns="${SCARAB_TEST_KUBE_NS:-scarab-kube-tests}"
# Set before up.sh runs, so a failed bring-up still deletes the public tunnel.
# Cluster calls run in subshells: kc exits when the cluster is gone, and that
# must not replace the exit code being reported.
cleanup() {
  local code=$?
  stop_forwards
  if [ "$code" -ne 0 ]; then
    echo "==> scarab-server log (a step or pass failed):"
    ( kc logs -n "$ns" deploy/"$release" --tail 200 ) || true
  fi
  # A failed case can leave its step Pod running, and a running step Pod
  # blocks the next mode switch.
  ( kc delete pods -n "$tns" -l scarab.io/run --ignore-not-found --wait=false >/dev/null 2>&1 ) || true
  echo "==> deleting the Forgejo quick tunnel"
  ( kc delete deploy scarab-forgejo-tunnel -n "$ns" --ignore-not-found --wait=false >/dev/null 2>&1 ) || true
  exit "$code"
}
trap cleanup EXIT
# Ctrl-C or a kill must still reach cleanup, which deletes the public tunnel.
trap 'exit 130' INT
trap 'exit 143' TERM

# Plugins on for the Argo CD pass, with the dev-only tag knob: kind images have
# no repository digest (owner default 8). Set for the whole run, so the pass
# needs no second helm upgrade, which would roll the Pods under the other
# passes' port-forwards.
plugin_repo=scarab.local/scarab-plugin-argocd
mkdir -p "$here/.state"
plugin_values="$here/.state/e2e-plugin-values.json"
cat > "$plugin_values" <<EOF
{"plugins": {"enabled": true},
 "scarab": {"extraEnv": {"SCARAB_DEV_PLUGIN_ALLOW_TAG": "1"}}}
EOF
# Only this recipe asks up.sh for the public Forgejo tunnel.
SCARAB_E2E_TUNNEL=1 SCARAB_KIND_EXTRA_VALUES="$plugin_values" bash "$here/up.sh" e2e
scarab_addr

cargo build -p scarab-cli
target="$(cargo_target_dir)"

# --- the cluster suites' namespace, images and registry -----------------------
kc create namespace "$tns" --dry-run=client -o yaml | kc apply -f - >/dev/null
# A Pod created before the namespace's `default` ServiceAccount exists is refused.
for _ in $(seq 1 30); do kc get serviceaccount default -n "$tns" >/dev/null 2>&1 && break; sleep 1; done
# The service image the uid-pinned-service case runs (git-bug 908f534), loaded
# so the node never pulls it (lib.sh's pull_image and load_image).
pull_image postgres:16
load_image postgres:16
# The plain-HTTP registry the `kind: build` case pushes to and then reads the
# tag list of, from inside the cluster.
echo "==> ensuring an in-cluster registry in namespace $tns"
kc apply -n "$tns" -f - >/dev/null <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: registry
spec:
  replicas: 1
  selector:
    matchLabels: { app: registry }
  template:
    metadata:
      labels: { app: registry }
    spec:
      containers:
        - name: registry
          image: registry:2
          ports:
            - containerPort: 5000
---
apiVersion: v1
kind: Service
metadata:
  name: registry
spec:
  selector: { app: registry }
  ports:
    - port: 5000
      targetPort: 5000
EOF
kc rollout status deployment/registry -n "$tns" --timeout=180s

# --- what the host cannot reach on its own -----------------------------------
forward scarab-postgres 5432; pg_port="$fwd_port"
forward scarab-forgejo 3000; forgejo_port="$fwd_port"
forward "$release-workspace" 80; ws_port="$fwd_port"
forward scarab-minio 9000; s3_port="$fwd_port"
# A fake forge (happy_path), the Forgejo adapter's capture listener and the
# results listener are servers on the host that Pods call.
probe_pod_host

# --- env --------------------------------------------------------------------
set -a
# shellcheck disable=SC1091
. "$kind_dir/.state/forgejo.env"
set +a
export SCARAB_TEST_FORGEJO_URL="http://127.0.0.1:$forgejo_port"
export SCARAB_TEST_FORGEJO_CALLBACK_HOST="$pod_host"
# forgejo.env (above) also carries SCARAB_TEST_CLONE_SOURCE and _HASH: the
# clone library libs.sh installed from the in-cluster Forgejo.

export SCARAB_E2E=1
export SCARAB_E2E_URL="$base"
export SCARAB_E2E_DATABASE_URL="postgres://scarab:scarab@127.0.0.1:$pg_port/scarab_e2e"
export SCARAB_E2E_CLI_BIN="$target/debug/scarab"
export SCARAB_E2E_KUBECONFIG="$kubeconfig"
export SCARAB_E2E_NAMESPACE="$ns"
export SCARAB_E2E_KIND_NODE="$node"
export SCARAB_E2E_POD_HOST="$pod_host"
# The same host as an IPv4 address, as the Pod resolved it ("" if it did not):
# what a NetworkPolicy ipBlock names.
export SCARAB_E2E_POD_HOST_IP="${pod_host_ip:-}"

export KUBECONFIG="$kubeconfig"
export SCARAB_TEST_KUBE=1
export SCARAB_TEST_KUBE_NS="$tns"
export SCARAB_TEST_WORKSPACE_URL="http://127.0.0.1:$ws_port"
export SCARAB_TEST_WORKSPACE_POD_URL="http://$release-workspace.$ns.svc"
SCARAB_TEST_WORKSPACE_SECRET="$(cat "$kind_dir/.state/workspace-token-secret")"
export SCARAB_TEST_WORKSPACE_SECRET
export SCARAB_TEST_WSFETCH_IMAGE=scarab-wsfetch:dev
export SCARAB_TEST_CLONE_IMAGE=scarab-clone:dev
export SCARAB_TEST_SIDECAR_IMAGE=scarab-results-sidecar:dev
export SCARAB_TEST_REGISTRY="registry.$tns.svc.cluster.local:5000"
export SCARAB_TEST_HOST_IP="$pod_host"
# The Depot's cold store: the e2e mode's bucket, through the port-forward.
export SCARAB_S3_BUCKET=scarab-e2e
export SCARAB_S3_ENDPOINT="http://127.0.0.1:$s3_port"
export SCARAB_S3_REGION=us-east-1
export SCARAB_S3_ACCESS_KEY=scarab
export SCARAB_S3_SECRET_KEY=scarabsecret
# The clone cases fetch THIS repo at a SHA that must exist ON THE FORGE, so the
# default is origin/main and not HEAD (which may be unpushed). CI passes the
# commit it checked out.
if [ -z "${SCARAB_TEST_CLONE_SHA:-}" ]; then
  SCARAB_TEST_CLONE_SHA="$(git rev-parse origin/main)" || die "set SCARAB_TEST_CLONE_SHA (no origin/main here)"
fi
export SCARAB_TEST_CLONE_SHA
SCARAB_TEST_CLONE_TOKEN="${SCARAB_TEST_CLONE_TOKEN:-$(gh auth token 2>/dev/null || true)}"
export SCARAB_TEST_CLONE_TOKEN

echo "==> e2e against $base (Pods reach the host at $pod_host; Forgejo via 127.0.0.1:$forgejo_port and $SCARAB_TEST_FORGEJO_HTTPS_URL)"
rc=0
pass() { # $1 name, rest: cargo nextest run args
  local name="$1"; shift
  echo "==> pass: $name"
  cargo nextest run --no-fail-fast --no-tests=pass "$@" ${filter:+"$filter"} || { rc=$?; echo "==> pass FAILED: $name"; }
}
pass e2e -p scarab-e2e -E 'not (binary(crash_resume) | binary(forgejo_onboarding) | binary(forgejo_tunnel))'
pass forgejo-adapter -p scarab-forge-forgejo --test live
pass forgejo-onboarding -p scarab-e2e --test forgejo_onboarding --run-ignored all --test-threads 1
pass forgejo-tunnel -p scarab-e2e --test forgejo_tunnel --run-ignored all --test-threads 1
pass cluster -p scarab-executor-k8s -p scarab-plugin-k8s --test cluster --run-ignored all
case plugin-argocd in
  *"$filter"*)
    echo "==> pass: plugin-argocd"
    PLUGIN_VERIFY_REPO="$plugin_repo" bash "$here/plugin-verify.sh" \
      || { rc=$?; echo "==> pass FAILED: plugin-argocd"; } ;;
  *) echo "==> pass: plugin-argocd skipped (the filter '$filter' is not part of its name)" ;;
esac
pass crash_resume -p scarab-e2e --test crash_resume
exit "$rc"
