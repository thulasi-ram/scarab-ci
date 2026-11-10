#!/usr/bin/env bash
# Bring up the kind dev stack (ADR-0077 Amendment, 2026-11-01): one kind
# cluster `scarab-dev`, Postgres and MinIO in it, and one Helm release of the
# real chart whose server and Depot run a binary built on the host.
#
#   deploy/kind/up.sh [dev|dogfood|e2e|chaos]
#
#   dev      (`just stack up`)      dev-insecure, database `scarab`, bucket `scarab-logs`
#   dogfood  (`just stack up dogfood`) GitHub App mode and GitHub login from
#                             ~/.config/scarab/dogfood.env, database `scarab_dogfood`,
#                             bucket `scarab-dogfood`, and a cloudflared tunnel to the
#                             public hostname that serves only /webhooks/
#   e2e      (`just test e2e`)     dev-insecure, database `scarab_e2e`, bucket
#                             `scarab-e2e`, so test data never mixes with dev's;
#                             a cloudflared quick tunnel in front of Forgejo only
#                             when e2e.sh sets SCARAB_E2E_TUNNEL=1
#   chaos    (`just test chaos`)   dev-insecure, database `scarab_chaos`, bucket
#                             `scarab-chaos`, both reached through toxiproxy;
#                             2 server replicas and 2 Depot replicas (ADR-0077)
#
# SCARAB_KIND_EXTRA_VALUES names one more Helm values file, applied last (for
# example the plugin settings `just test e2e` adds). It is not remembered:
# `just stack reload` re-applies the mode without it.
#
# Idempotent: re-running rebuilds the binary and rolls the Pods only when the
# binary changed. `just stack reload` is the short path for that alone.
#
# What it does, in order:
#   1. build scarab-server and scarab-wsfetch for Linux on the host
#      (cargo-zigbuild against bookworm's glibc on macOS; on Linux too when
#      cargo-zigbuild is installed, plain cargo otherwise);
#   2. stage them, and the web UI's dist when there is one, in deploy/kind/.mount;
#   3. create the kind cluster with deploy/kind/.mount mounted on the node and
#      the host port of SCARAB_ADDR mapped to the API's node port, or recreate
#      a `scarab-dev` cluster that lacks them;
#   4. build the runtime image and the step helper images and load them;
#   5. apply Postgres, MinIO and Forgejo, create the mode's database; under
#      `just test e2e` (SCARAB_E2E_TUNNEL=1), start the Forgejo quick tunnel first
#      (Forgejo's ROOT_URL is its URL);
#   6. helm upgrade --install the chart with deploy/kind/values.yaml plus the
#      mode's values;
#   7. seed Forgejo (deploy/kind/forgejo.sh).
#
# Kube safety: every kubectl and helm call goes through kc/hc below, which use
# the cluster's own kubeconfig file (deploy/kind/.kubeconfig) and check that its
# server is this kind cluster's API port before each call. The ambient
# ~/.kube/config is never read.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
# shellcheck source=deploy/kind/lib.sh
. "$here/lib.sh"   # cluster, node, ns, release, node_port, kubeconfig, kc, hc, die
mode="${1:-dev}"
state="$here/.state"           # gitignored: generated config, dev secrets
mount="$here/.mount"           # gitignored: staged binaries + UI, mounted on the node
node_mount=/scarab             # where $mount appears on the kind node
dogfood_env="${SCARAB_DOGFOOD_ENV:-$HOME/.config/scarab/dogfood.env}"

need docker; need kind; need kubectl; need helm; need curl; need python3

# `helm upgrade --force-conflicts` below exists only in Helm 4.
helm_major="$(helm version --template '{{.Version}}' 2>/dev/null | sed -n 's/^v\([0-9][0-9]*\)\..*/\1/p')"
[ "${helm_major:-0}" -ge 4 ] || die "Helm 4 is required (found $(helm version --short 2>/dev/null || echo none)); see https://helm.sh/docs/intro/install/"

case "$mode" in
  dev|dogfood|e2e|chaos) ;;
  *) die "unknown mode '$mode' (expected dev, dogfood, e2e or chaos)" ;;
esac
extra_values="${SCARAB_KIND_EXTRA_VALUES:-}"
if [ -n "$extra_values" ]; then
  [ -f "$extra_values" ] || die "SCARAB_KIND_EXTRA_VALUES=$extra_values does not exist"
fi

mkdir -p "$state" "$mount/bin"
chmod 700 "$state"

# --- mode config ------------------------------------------------------------
# The dogfood env file is read key by key, never sourced and never printed:
# it holds the App key path, the webhook secret and the master key.
if [ "$mode" = dogfood ]; then
  [ -f "$dogfood_env" ] || die "$dogfood_env does not exist (see deploy/kind/README.md)"
  missing=""
  for k in CLOUDFLARE_TUNNEL_TOKEN DOGFOOD_HOSTNAME; do
    [ -n "$(env_get "$dogfood_env" "$k")" ] || missing="$missing $k"
  done
  if [ -n "$missing" ]; then
    die "just stack up dogfood needs the named Cloudflare tunnel: set$missing in $dogfood_env
  CLOUDFLARE_TUNNEL_TOKEN  the tunnel's token (Zero Trust > Networks > Tunnels)
  DOGFOOD_HOSTNAME         its public hostname, routed to http://scarab.scarab.svc.cluster.local:80"
  fi
  for k in SCARAB_GITHUB_APP_ID SCARAB_APP_PEM SCARAB_GITHUB_WEBHOOK_SECRET SCARAB_MASTER_KEY SCARAB_RESULTS_TOKEN_SECRET SCARAB_ORG; do
    [ -n "$(env_get "$dogfood_env" "$k")" ] || die "$k is empty or missing in $dogfood_env"
  done
  # GitHub login (git-bug 2199c2e): the GitHub App's own OAuth client. Its
  # callback URL in the App settings is http://localhost:<port>/v1/auth/callback.
  for k in SCARAB_OAUTH_CLIENT_ID SCARAB_OAUTH_CLIENT_SECRET; do
    [ -n "$(env_get "$dogfood_env" "$k")" ] \
      || die "$k is empty or missing in $dogfood_env: the dogfood stack signs in with GitHub through the GitHub App's OAuth client (its Client ID, and a client secret generated in the App settings; see deploy/kind/README.md)"
  done
  dogfood_principals "$dogfood_env"   # dogfood_owner, dogfood_member (lib.sh)
  # A bare host name: it becomes https://<it> in publicUrl and the probes.
  dogfood_host="$(env_get "$dogfood_env" DOGFOOD_HOSTNAME)"
  dogfood_host="${dogfood_host#https://}"
  dogfood_host="${dogfood_host#http://}"
  case "$dogfood_host" in
    ''|*[!A-Za-z0-9.-]*) die "DOGFOOD_HOSTNAME in $dogfood_env must be a bare host name such as ci.example.com (no spaces, path or slash)" ;;
  esac
  pem="$(env_get "$dogfood_env" SCARAB_APP_PEM)"
  [ -f "$pem" ] || die "SCARAB_APP_PEM in $dogfood_env names a file that does not exist"
  database=scarab_dogfood
  bucket=scarab-dogfood
elif [ "$mode" = e2e ]; then
  database=scarab_e2e
  bucket=scarab-e2e
  # The forge_token cases install the clone library into their org, and the
  # Argo CD pass installs the Argo CD plugin, as an org admin does: the server
  # fetches libs/clone.yaml and the plugin's manifest from github.com at the
  # commit that last changed each, so both must be committed and pushed. In CI
  # the checked-out commit is the pushed one.
  need git
  for f in libs/clone.yaml crates/scarab-plugin-argocd/manifest.json; do
    c="$(git -C "$root" log -1 --format=%H -- "$f")"
    [ -n "$c" ] || die "$f is not committed: the server fetches it from github.com"
    git -C "$root" diff --quiet "$c" -- "$f" \
      || die "$f has uncommitted changes: the server fetches it from github.com at $c"
    if [ "${GITHUB_ACTIONS:-}" != true ] \
       && [ -z "$(git -C "$root" branch -r --contains "$c" 2>/dev/null)" ]; then
      die "$f at $c is not pushed: the server fetches it from github.com at that commit"
    fi
    case "$f" in
      libs/*) libs_commit="$c" ;;
      *) argocd_manifest_commit="$c" ;;
    esac
  done
elif [ "$mode" = chaos ]; then
  database=scarab_chaos
  bucket=scarab-chaos
else
  database=scarab
  bucket=scarab-logs
fi

# The host port: SCARAB_ADDR from deploy/kind/.env (lib.sh scarab_addr;
# `just ui`'s SCARAB_API_URL must name the same port).
scarab_addr   # addr_file, host_ip, host_port, base
# The dogfood stack's UI is http://localhost:<port> (scarab.uiUrl) and is for
# this machine only. A LAN or 0.0.0.0 address would publish a login and an API
# to the network that the localhost UI URL does not serve.
if [ "$mode" = dogfood ] && [ "$host_ip" != 127.0.0.1 ]; then
  die "the dogfood stack answers on 127.0.0.1 only, and SCARAB_ADDR in $addr_file has host $host_ip.
  Set SCARAB_ADDR=127.0.0.1:$host_port there and rerun 'just stack up dogfood'."
fi

port_listening() { (exec 3<>"/dev/tcp/$1/$2") 2>/dev/null; }

# --- 1. build ---------------------------------------------------------------
docker_arch="$(docker version -f '{{.Server.Arch}}')"
case "$docker_arch" in
  arm64|aarch64) triple_arch=aarch64 ;;
  amd64|x86_64) triple_arch=x86_64 ;;
  *) die "unsupported docker architecture '$docker_arch'" ;;
esac
platform="linux/$docker_arch"
profile="${SCARAB_KIND_PROFILE:-dev}"
case "$profile" in dev) profile_dir=debug ;; *) profile_dir="$profile" ;; esac
target_dir="$(cargo_target_dir)"
bins=(-p scarab-server --bin scarab-server -p scarab-workspace-client --bin scarab-wsfetch)

if [ "${SCARAB_KIND_SKIP_BUILD:-0}" = 1 ]; then
  echo "==> SCARAB_KIND_SKIP_BUILD=1: using the binaries already staged"
else
  if [ "$(uname -s)" = Linux ] && ! command -v cargo-zigbuild >/dev/null 2>&1; then
    # A native build links against the host's glibc. The runtime image is
    # bookworm (2.36); a newer host glibc can produce a binary that does not
    # start there. cargo-zigbuild pins 2.36 and avoids that.
    echo "==> building scarab-server + scarab-wsfetch (cargo, $profile; native glibc)"
    ( cd "$root" && cargo build --profile "$profile" "${bins[@]}" )
    out="$target_dir/$profile_dir"
  else
    need cargo-zigbuild
    triple="$triple_arch-unknown-linux-gnu"
    echo "==> building scarab-server + scarab-wsfetch (cargo zigbuild, $triple, glibc 2.36, $profile)"
    ( cd "$root" && cargo zigbuild --profile "$profile" "${bins[@]}" --target "$triple.2.36" )
    out="$target_dir/$triple/$profile_dir"
  fi
  # Stage by copy + rename, so the mounted directory always holds a whole
  # binary under a new inode: a Pod that restarts mid-build starts the
  # previous binary, never a half-written one. On macOS `cp -c` clones the
  # file (APFS), which costs no copy.
  stage() { # $1 source file, $2 name in $mount/bin
    cp -c "$1" "$mount/bin/.$2.tmp" 2>/dev/null || cp "$1" "$mount/bin/.$2.tmp"
    chmod 0755 "$mount/bin/.$2.tmp"
    mv -f "$mount/bin/.$2.tmp" "$mount/bin/$2"
  }
  stage "$out/scarab-server" scarab-server
  stage "$out/scarab-wsfetch" scarab-wsfetch
fi
[ -x "$mount/bin/scarab-server" ] || die "no staged binary at $mount/bin/scarab-server"
[ -x "$mount/bin/scarab-wsfetch" ] || die "no staged binary at $mount/bin/scarab-wsfetch"

# The web UI: built when it is missing or older than its sources, then staged.
# Without node the stack runs API-only (the server serves the UI only when
# index.html exists), and `just ui` still works against it.
# SCARAB_KIND_SKIP_UI=1 skips the build and stages no UI (CI: no test reads it).
ui_src="$root/ui/scarab-web-ui"
ui_dist="$ui_src/dist"
if [ "${SCARAB_KIND_SKIP_UI:-0}" = 1 ]; then
  echo "    SCARAB_KIND_SKIP_UI=1: the stack runs without the web UI"
  rm -rf "$mount/ui"
elif command -v npm >/dev/null 2>&1; then
  if [ ! -f "$ui_dist/index.html" ] \
     || [ -n "$(find "$ui_src/src" "$ui_src/index.html" "$root/ui/brand" -newer "$ui_dist/index.html" -print -quit 2>/dev/null)" ]; then
    echo "==> building the web UI (npm)"
    [ -d "$ui_src/node_modules" ] || npm --prefix "$ui_src" ci
    npm --prefix "$ui_src" run build >/dev/null
  fi
else
  echo "    npm not found: the stack runs without the web UI (the API is unaffected)"
fi
if [ "${SCARAB_KIND_SKIP_UI:-0}" != 1 ] && [ -f "$ui_dist/index.html" ]; then
  rm -rf "$mount/ui.new"
  cp -R "$ui_dist" "$mount/ui.new"
  rm -rf "$mount/ui.old"
  [ -d "$mount/ui" ] && mv "$mount/ui" "$mount/ui.old"
  mv "$mount/ui.new" "$mount/ui"
  rm -rf "$mount/ui.old"
fi

# --- 2. the cluster ---------------------------------------------------------
# A `scarab-dev` cluster must have this worktree's mount and this host port, both
# fixed at creation. One created by another worktree, or before SCARAB_ADDR
# changed, has neither. Recreating it
# deletes its Postgres and MinIO data, so that needs SCARAB_KIND_RECREATE=1.
misfit=""
cluster_fits() {
  docker inspect "$node" >/dev/null 2>&1 || { misfit="its node container $node is missing"; return 1; }
  local mounts ports
  mounts="$(docker inspect -f '{{range .Mounts}}{{.Source}}={{.Destination}}{{"\n"}}{{end}}' "$node")"
  printf '%s\n' "$mounts" | grep -qxF "$mount=$node_mount" \
    || { misfit="it does not mount this worktree's $mount (another worktree created it)"; return 1; }
  ports="$(docker inspect -f '{{range $p, $b := .HostConfig.PortBindings}}{{range $b}}{{$p}}={{.HostIp}}:{{.HostPort}}{{"\n"}}{{end}}{{end}}' "$node")"
  printf '%s\n' "$ports" | grep -qxF "$node_port/tcp=$host_ip:$host_port" \
    || { misfit="it does not publish the API on $host_ip:$host_port (SCARAB_ADDR in $addr_file changed since it was created)"; return 1; }
}
exists=0
kind get clusters 2>/dev/null | grep -qx "$cluster" && exists=1
if [ "$exists" = 1 ] && cluster_fits; then
  echo "==> reusing kind cluster '$cluster'"
  unpause_node
  if [ "$(docker inspect -f '{{.State.Running}}' "$node")" != true ]; then
    echo "    its node container is stopped; starting it"
    docker start "$node" >/dev/null || die "could not start the stopped node container $node ('just stack down' deletes the cluster)"
  fi
  kind export kubeconfig --name "$cluster" --kubeconfig "$kubeconfig" >/dev/null \
    || die "could not export the kubeconfig of kind cluster '$cluster'"
  for _ in $(seq 1 60); do
    ( kc get --raw /readyz ) >/dev/null 2>&1 && break
    sleep 2
  done
else
  # The port first, before anything is deleted. The existing node holding it
  # is not a conflict: deleting the cluster frees it.
  if port_listening "${host_ip/0.0.0.0/127.0.0.1}" "$host_port" \
     && ! { [ "$exists" = 1 ] && docker port "$node" 2>/dev/null | grep -q ":$host_port\$"; }; then
    die "something already listens on $host_ip:$host_port (SCARAB_ADDR in $addr_file). Find it with:
    lsof -nP -iTCP:$host_port -sTCP:LISTEN"
  fi
  if [ "$exists" = 1 ]; then
    [ "${SCARAB_KIND_RECREATE:-0}" = 1 ] || die "kind cluster '$cluster' does not fit: $misfit.
  Recreating it deletes its Postgres and MinIO data (both modes' databases and buckets).
  Run 'just stack up' from the worktree or with the SCARAB_ADDR that created it, or
  recreate it: SCARAB_KIND_RECREATE=1 just stack up   (or 'just stack down' first)"
    echo "==> SCARAB_KIND_RECREATE=1: deleting kind cluster '$cluster' ($misfit)"
    kind delete cluster --name "$cluster" --kubeconfig "$kubeconfig"
  fi
  cat > "$state/kind.yaml" <<EOF
# Generated by deploy/kind/up.sh; do not edit.
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
name: $cluster
nodes:
  - role: control-plane
    extraMounts:
      - hostPath: $mount
        containerPath: $node_mount
    extraPortMappings:
      - containerPort: $node_port
        hostPort: $host_port
        listenAddress: "$host_ip"
        protocol: TCP
EOF
  echo "==> creating kind cluster '$cluster' (kubeconfig: deploy/kind/.kubeconfig)"
  kind create cluster --name "$cluster" --config "$state/kind.yaml" --kubeconfig "$kubeconfig"
  rm -f "$state/loaded-images"
fi
chmod 600 "$kubeconfig"
verify_kubeconfig
kc get nodes >/dev/null

# The mode-switch guard, before anything is applied or deleted: a refused
# switch must leave the running mode as it was (its toxiproxy, its Forgejo
# tunnel and ROOT_URL, its release). A step Pod of the current mode would
# carry on against the other mode's server, Depot, bucket and token secrets,
# and its Attempt fails on resume. Step Pods carry the executor's
# scarab.io/run label. A cluster created above has none.
last_mode="$(cat "$state/mode" 2>/dev/null || true)"
switching=0
if [ -n "$last_mode" ] && [ "$last_mode" != "$mode" ]; then
  switching=1
  active="$(active_step_pods)"
  if [ "$active" != 0 ] && [ "${SCARAB_KIND_FORCE_SWITCH:-0}" != 1 ]; then
    die "refusing to switch $last_mode -> $mode: $active step Pod(s) of the $last_mode stack are still running.
  Let their Runs finish (or cancel them), then rerun. To switch anyway and fail them:
    SCARAB_KIND_FORCE_SWITCH=1 bash deploy/kind/up.sh $mode"
  fi
fi

# --- 3. images ----------------------------------------------------------------
# lib.sh's load_image (one-platform archive, skipped when unchanged) and
# pull_image.
echo "==> images"
# --provenance=false: the default attestation records the build time, so an
# unchanged image got a new ID on every build and was loaded again every run.
build() { docker build -q --provenance=false "$@" >/dev/null; }
build -t scarab-runtime:dev "$root/docker/runtime"
build -t scarab-wsfetch:dev --build-arg BASE=scarab-runtime:dev \
  -f "$root/docker/runtime/wsfetch.Dockerfile" "$mount/bin"
build -t scarab-results-sidecar:dev "$root/docker/sidecar"
build -t scarab-clone:dev "$root/docker/clone"
for img in scarab-runtime:dev scarab-wsfetch:dev scarab-results-sidecar:dev scarab-clone:dev; do
  load_image "$img"
done
# Third-party images pinned by tag only are loaded from the host's store, so a
# cluster recreate does not pull them again through the node. A digest-pinned
# one (MinIO, mc, cloudflared) is left to the node: a one-platform archive
# drops the index digest the reference names, so the node would pull it anyway.
infra_images="$(sed -n 's/^ *image: *\([^ #]*\).*/\1/p' "$here/postgres.yaml" "$here/minio.yaml" "$here/forgejo.yaml")"
for img in $infra_images; do
  case "$img" in *@*) continue ;; esac
  pull_image "$img"
  load_image "$img"
done

# --- 3b. NetworkPolicy is enforced ----------------------------------------------
# kindnet enforces NetworkPolicy, and the plugin runtime depends on it: a plugin
# Pod whose isolation self-check reaches the API server refuses to start. On a
# kind cluster 13 h old, after many mode switches, the policies existed and
# kindnet ran, yet a Pod reached the API server and the internet (git-bug
# ba32d49); a fresh cluster enforced them. So every run checks it: a Pod with
# no policy must reach the API server (the probe works), and one in a namespace
# that denies all egress must not (the policy is enforced).
echo "==> checking that the cluster enforces NetworkPolicy"
canary_ns=scarab-netpol-canary
canary_image=busybox:1.36
pull_image "$canary_image"
load_image "$canary_image"
# probe NAME WANT: run a Pod that tries the API server's Service once a second
# for up to 30 s and stops at the first result equal to WANT (REACHED or
# UNREACHED); it prints the last result. Repeating, because kindnet programs a
# new Pod's policy a moment after the Pod starts: a first connection can get
# through before it (the plugin runtime's self-check repeats for the same
# reason, scarab-plugin-k8s probe.rs `until_isolated`).
probe() {
  kc run "$1" -n "$canary_ns" --image="$canary_image" --image-pull-policy=IfNotPresent \
    --restart=Never --quiet --env="WANT=$2" --command -- sh -c '
      i=0
      while :; do
        if nc -z -w 2 "$KUBERNETES_SERVICE_HOST" "$KUBERNETES_SERVICE_PORT"; then
          got=REACHED
        else
          got=UNREACHED
        fi
        if [ "$got" = "$WANT" ] || [ $i -ge 30 ]; then echo "$got"; exit 0; fi
        i=$((i+1)); sleep 1
      done' >/dev/null
  kc wait -n "$canary_ns" --for=jsonpath='{.status.phase}'=Succeeded "pod/$1" --timeout=150s >/dev/null \
    || die "the NetworkPolicy canary Pod $1 did not finish in 150 s (kubectl -n $canary_ns describe pod $1)"
  kc logs -n "$canary_ns" "$1" | tail -1
}
kc delete namespace "$canary_ns" --ignore-not-found --wait=true >/dev/null
kc create namespace "$canary_ns" >/dev/null
open_result="$(probe open REACHED)"
[ "$open_result" = REACHED ] \
  || die "the NetworkPolicy canary cannot reach the API server even with no policy ($open_result); the probe itself is broken"
kc apply -n "$canary_ns" -f - >/dev/null <<'YAML'
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: deny-all-egress
spec:
  podSelector: {}
  policyTypes: [Egress]
YAML
denied_result="$(probe denied UNREACHED)"
kc delete namespace "$canary_ns" --wait=false >/dev/null
[ "$denied_result" = UNREACHED ] || die "kind cluster '$cluster' does not enforce NetworkPolicy:
  a Pod whose namespace denies all egress still reached the API server after 30 s (git-bug ba32d49).
  Plugins will refuse to start, and NetworkPolicy tests will fail.
  Recreate the cluster: 'just stack down', then 'just stack up'
  (this deletes the in-cluster Postgres and MinIO data of every mode)."

# --- 4. Postgres, MinIO, the mode's database ---------------------------------
echo "==> namespace $ns, Postgres, MinIO"
kc create namespace "$ns" --dry-run=client -o yaml | kc apply -f - >/dev/null
kc apply -n "$ns" -f "$here/postgres.yaml" >/dev/null
# A Job's template is immutable; the bucket job is one-shot and idempotent, so
# it is recreated each run instead of applied over.
kc delete job scarab-minio-mkbucket -n "$ns" --ignore-not-found >/dev/null
kc apply -n "$ns" -f "$here/minio.yaml" >/dev/null
kc rollout status -n "$ns" deploy/scarab-postgres --timeout=180s
kc rollout status -n "$ns" deploy/scarab-minio --timeout=180s
kc wait -n "$ns" --for=condition=complete job/scarab-minio-mkbucket --timeout=120s >/dev/null
# The readiness probe can pass while initdb's temporary server is still up, so
# ask until a real connection works.
for _ in $(seq 1 30); do
  kc exec -n "$ns" deploy/scarab-postgres -- psql -U scarab -d scarab -tAc 'select 1' >/dev/null 2>&1 && break
  sleep 2
done
fresh_database=0
if ! kc exec -n "$ns" deploy/scarab-postgres -- psql -U scarab -d scarab -tAc \
     "select 1 from pg_database where datname = '$database'" | grep -q 1; then
  echo "    creating database $database"
  kc exec -n "$ns" deploy/scarab-postgres -- createdb -U scarab "$database"
  fresh_database=1
fi
database_url="postgres://scarab:scarab@scarab-postgres:5432/$database"
# Chaos mode reaches the same Postgres and MinIO through toxiproxy, so a
# scenario can fault those connections (ADR-0077 §3). Other modes do not run it.
s3_endpoint="http://scarab-minio:9000"
if [ "$mode" = chaos ]; then
  echo "==> toxiproxy (chaos mode: Postgres and MinIO through it)"
  kc apply -n "$ns" -f "$here/toxiproxy.yaml" >/dev/null
  # The proxies are read from the ConfigMap at start; restart on a change.
  kc rollout restart -n "$ns" deploy/scarab-toxiproxy >/dev/null
  kc rollout status -n "$ns" deploy/scarab-toxiproxy --timeout=180s
  database_url="postgres://scarab:scarab@scarab-toxiproxy:15432/$database"
  s3_endpoint="http://scarab-toxiproxy:19000"
else
  kc delete -n "$ns" -f "$here/toxiproxy.yaml" --ignore-not-found >/dev/null
fi

# Forgejo, in every mode. `just test e2e` (e2e.sh, which sets SCARAB_E2E_TUNNEL=1
# and deletes the tunnel on exit) also puts a cloudflared quick tunnel in
# front of it: Forgejo builds clone URLs and payload links from ROOT_URL, so
# ROOT_URL must be the tunnel's https URL, which is known only once
# cloudflared has registered. Each tunnel Pod gets a new URL, so every e2e run
# starts a new one. Nothing else opens it: e2e mode without the flag (by hand)
# runs without a tunnel, and every run deletes one an earlier run left.
# A random admin password and hook secret per stack, kept in .state (mode 600).
for f in forgejo-admin-password forgejo-webhook-secret; do
  [ -s "$state/$f" ] || ( umask 077; head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$state/$f" )
done
forgejo_webhook_secret="$(cat "$state/forgejo-webhook-secret")"
render_forgejo() { # $1 ROOT_URL, $2 REQUIRE_SIGNIN_VIEW
  echo "==> Forgejo (ROOT_URL $1, sign-in required to view: $2)"
  V_ROOT="$1" V_SIGNIN="$2" python3 -I -c '
import os, sys
t = open(sys.argv[1]).read()
sys.stdout.write(t.replace("__ROOT_URL__", os.environ["V_ROOT"]).replace("__SIGNIN_VIEW__", os.environ["V_SIGNIN"]))
' "$here/forgejo.yaml" | kc apply -n "$ns" -f - >/dev/null
  kc rollout status -n "$ns" deploy/scarab-forgejo --timeout=180s
}
in_cluster_root="http://scarab-forgejo.$ns.svc.cluster.local:3000/"
tunnel_url=""
kc delete deploy scarab-forgejo-tunnel -n "$ns" --ignore-not-found --wait=true >/dev/null
if [ "$mode" = e2e ] && [ "${SCARAB_E2E_TUNNEL:-0}" = 1 ]; then
  # Sign-in required BEFORE the tunnel makes Forgejo public.
  render_forgejo "$in_cluster_root" true
  echo "==> Forgejo quick tunnel (network calls; Forgejo is public while it runs)"
  kc apply -n "$ns" -f "$here/forgejo-tunnel.yaml" >/dev/null
  kc rollout status -n "$ns" deploy/scarab-forgejo-tunnel --timeout=120s
  for _ in $(seq 1 60); do
    tunnel_log="$(kc logs -n "$ns" deploy/scarab-forgejo-tunnel 2>&1 || true)"
    tunnel_url="$(printf '%s\n' "$tunnel_log" | grep -ao 'https://[a-z0-9-]*\.trycloudflare\.com' | head -n 1 || true)"
    if [ -n "$tunnel_url" ] && printf '%s\n' "$tunnel_log" | grep -aq "Registered tunnel connection"; then break; fi
    tunnel_url=""
    sleep 2
  done
  [ -n "$tunnel_url" ] || { printf '%s\n' "$tunnel_log" | tail -n 20 >&2; die "cloudflared did not register a quick tunnel"; }
  echo "    $tunnel_url"
  render_forgejo "$tunnel_url/" true
else
  render_forgejo "$in_cluster_root" false
fi

# --- 5. secrets ---------------------------------------------------------------
# Dev secrets are generated once and kept in deploy/kind/.state (gitignored),
# not per run: the control plane mints workspace and results tokens with them,
# and the master key encrypts stored secrets, so a value that changed per run
# would invalidate every in-flight Step's token and every stored secret.
dev_secret() { # $1 name: a base64 32-byte value, created on first use
  local f="$state/$1"
  if [ ! -s "$f" ]; then
    ( umask 077; head -c 32 /dev/urandom | base64 | tr -d '\n' > "$f" )
  fi
  cat "$f"
}
if [ "$mode" = dogfood ]; then
  master_key="$(env_get "$dogfood_env" SCARAB_MASTER_KEY)"
  results_secret="$(env_get "$dogfood_env" SCARAB_RESULTS_TOKEN_SECRET)"
  workspace_secret="$(env_get "$dogfood_env" SCARAB_WORKSPACE_TOKEN_SECRET)"
  [ -n "$workspace_secret" ] || workspace_secret="$(dev_secret dogfood-workspace-token-secret)"
  webhook_secret="$(env_get "$dogfood_env" SCARAB_GITHUB_WEBHOOK_SECRET)"
else
  master_key="$(dev_secret master-key)"
  results_secret="$(dev_secret results-token-secret)"
  workspace_secret="$(dev_secret workspace-token-secret)"
  webhook_secret=""
fi
[ "$workspace_secret" != "$results_secret" ] \
  || die "the workspace token secret must differ from the results token secret (ADR-0061)"

# --- 6. the release -----------------------------------------------------------
# The Depot's warm PVC caches blobs of ONE bucket. Switching modes switches the
# bucket, and a warm tier holding the other mode's blobs would serve reads the
# new bucket cannot back (ADR-0077 Amendment §4). The Depot is a cache
# (ADR-0066), so its PVC is deleted on a mode switch and refills from the bucket.
if [ "$switching" = 1 ]; then
  echo "==> mode $last_mode -> $mode: deleting the Depot's warm PVC"
  kc delete statefulset -n "$ns" "$release-workspace" --ignore-not-found --wait=true >/dev/null
  kc delete pvc -n "$ns" -l app.kubernetes.io/component=workspace --ignore-not-found --wait=true >/dev/null
fi

values="$(mktemp "$state/values.XXXXXX")"
# tunnel_unchecked: dogfood's cloudflared was applied and the outside check
# below has not passed yet. Any exit in that window (a rollout timeout, a
# failed command, Ctrl-C) scales it to 0, so the release is never left
# public on a route nobody checked.
tunnel_unchecked=0
on_exit() {
  rm -rf "$values" "${secret_file:-}" "${token_file:-}"
  if [ "$tunnel_unchecked" = 1 ]; then
    echo "==> stopped before the tunnel check passed: scaling cloudflared to 0" >&2
    ( kc scale deploy scarab-cloudflared -n "$ns" --replicas=0 >/dev/null 2>&1 ) || true
  fi
}
trap on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
chmod 600 "$values"

if [ "$mode" = e2e ]; then
  # The sources the e2e cases install from (checked as pushed above), and the
  # clone library's content hash, which the forge_token cases compare with
  # the installed version (SHA-256 of the file, as Scarab hashes a library).
  printf '%s\n' "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@$libs_commit" \
    > "$state/e2e-clone-source"
  shasum -a 256 "$root/libs/clone.yaml" | awk '{print $1}' > "$state/e2e-clone-hash"
  printf '%s\n' \
    "github.com/thulasi-ram/scarab-ci//crates/scarab-plugin-argocd/manifest.json@$argocd_manifest_commit" \
    > "$state/e2e-argocd-manifest-source"
fi

# Roll the Pods when the binary or the staged UI changes, and only then: helm
# sees a new Pod template annotation. The server checks for ui/index.html at
# start, so a UI staged after the Pod started needs a roll too.
binary_sum="$({ cat "$mount/bin/scarab-server"; cat "$mount/ui/index.html" 2>/dev/null || true; } | shasum -a 256 | cut -c1-16)"
if [ "$mode" = dogfood ]; then
  # Real GitHub login, as a real install runs (git-bug 2199c2e): an
  # identity store, role bindings, API tokens and passkey step-up. The UI
  # URL is localhost, not $base: browsers refuse passkeys on an IP address,
  # and the login callback is registered for it.
  dev_insecure=false
  app_id="$(env_get "$dogfood_env" SCARAB_GITHUB_APP_ID)"
  timeout="$(env_get "$dogfood_env" SCARAB_STEP_TIMEOUT_SECS)"
  oauth_client_id="$(env_get "$dogfood_env" SCARAB_OAUTH_CLIENT_ID)"
  ui_url="http://localhost:$host_port"
else
  dev_insecure=true; app_id=""; timeout=""; oauth_client_id=""; ui_url=""
fi
# Written as JSON (valid YAML) by python, never spliced into YAML text: a
# secret with a quote, a backslash or a '#' stays the same bytes. The values
# reach python through its environment, not argv, so they never show in `ps`.
V_MODE="$mode" V_SUM="$binary_sum" V_BUCKET="$bucket" V_S3_ENDPOINT="$s3_endpoint" V_DEV_INSECURE="$dev_insecure" \
V_APP_ID="$app_id" V_PUBLIC_URL="https://${dogfood_host:-}" V_TIMEOUT="${timeout:-3600}" \
V_UI_URL="$ui_url" V_OAUTH_CLIENT_ID="$oauth_client_id" V_OWNER="${dogfood_owner:-}" \
V_DATABASE_URL="$database_url" V_MASTER_KEY="$master_key" V_RESULTS="$results_secret" \
V_WORKSPACE="$workspace_secret" V_WEBHOOK="$webhook_secret" V_FORGEJO_WEBHOOK="$forgejo_webhook_secret" \
python3 -I - > "$values" <<'PY2'
import json, os, sys
e = os.environ
v = {
    "podAnnotations": {"scarab.dev/binary-sha256": e["V_SUM"]},
    "scarab": {
        "s3": {"bucket": e["V_BUCKET"], "endpoint": e["V_S3_ENDPOINT"]},
        "devInsecure": e["V_DEV_INSECURE"] == "true",
    },
    "secrets": {
        "databaseUrl": e["V_DATABASE_URL"],
        "masterKey": e["V_MASTER_KEY"],
        "resultsTokenSecret": e["V_RESULTS"],
        "workspaceTokenSecret": e["V_WORKSPACE"],
    },
}
if e["V_MODE"] != "dogfood":
    # The in-cluster Forgejo's hooks are stamped with this secret.
    v["secrets"]["forgejoWebhookSecret"] = e["V_FORGEJO_WEBHOOK"]
if e["V_MODE"] == "e2e":
    # The address bind stamps onto a hook it registers: Forgejo delivers to
    # the server's Service.
    v["scarab"]["publicUrl"] = "http://scarab.scarab.svc.cluster.local"
if e["V_MODE"] == "chaos":
    # ADR-0077 rank 1 and 2: two servers and two Depot replicas. Plugins off:
    # the plugin runtime would delete the Deployments of plugins the chaos
    # database does not know.
    v["replicaCount"] = 2
    v["workspace"] = {"replicaCount": 2}
    v["plugins"] = {"enabled": False}
    # ADR-0077 rank 4 SIGSTOPs the scheduler leader for a little over the
    # 30 s lease TTL. At the chart's 3 failures x 15 s the kubelet restarts
    # the stopped server in that window and the scenario is INCONCLUSIVE;
    # 8 x 15 s leaves it stopped until the scenario resumes it.
    v["livenessProbe"] = {"failureThreshold": 8}
if e["V_MODE"] == "dogfood":
    v["scarab"].update(githubAppId=e["V_APP_ID"], publicUrl=e["V_PUBLIC_URL"], stepTimeoutSecs=e["V_TIMEOUT"],
                       uiUrl=e["V_UI_URL"])
    # A named provider `github`, so Principal ids are github:<login>. The
    # client secret is the Secret scarab-oauth-github, made below.
    v["scarab"]["oauth"] = {
        "owners": [e["V_OWNER"]],
        "providers": [{
            "id": "github",
            "name": "GitHub",
            "clientId": e["V_OAUTH_CLIENT_ID"],
            "authorizeUrl": "https://github.com/login/oauth/authorize",
            "tokenUrl": "https://github.com/login/oauth/access_token",
            "userinfoUrl": "https://api.github.com/user",
            "scopes": "read:user",
            "forgeUrl": "https://api.github.com",
            "clientSecret": {"name": "scarab-oauth-github", "key": "client-secret"},
        }],
    }
    v["secrets"].update(githubWebhookSecret=e["V_WEBHOOK"],
                        githubAppPemSecret={"name": "scarab-github-app", "key": "github-app.pem"})
json.dump(v, sys.stdout, indent=1)
PY2

if [ "$mode" = dogfood ]; then
  echo "==> GitHub App PEM secret, GitHub login client secret, cloudflared"
  kc create secret generic scarab-github-app -n "$ns" \
    --from-file="github-app.pem=$pem" --dry-run=client -o yaml | kc apply -f - >/dev/null
  # The secrets go through a file, not argv, so they never show in `ps`.
  secret_file="$(mktemp "$state/oauth.XXXXXX")"
  env_get "$dogfood_env" SCARAB_OAUTH_CLIENT_SECRET | tr -d '\n' > "$secret_file"
  kc create secret generic scarab-oauth-github -n "$ns" \
    --from-file="client-secret=$secret_file" --dry-run=client -o yaml | kc apply -f - >/dev/null
  rm -f "$secret_file"
  token_file="$(mktemp "$state/cf.XXXXXX")"
  env_get "$dogfood_env" CLOUDFLARE_TUNNEL_TOKEN | tr -d '\n' > "$token_file"
  kc create secret generic scarab-cloudflared -n "$ns" \
    --from-file="token=$token_file" --dry-run=client -o yaml | kc apply -f - >/dev/null
  rm -f "$token_file"
else
  # A dev stack has no tunnel; remove one a dogfood run left.
  kc delete deploy scarab-cloudflared -n "$ns" --ignore-not-found >/dev/null
fi

echo "==> helm upgrade --install $release (mode: $mode, database: $database, bucket: $bucket)"
# --force-conflicts (Helm 4, checked at the top): Helm 4 applies server-side; a field touched with kubectl
# would otherwise abort the whole upgrade. The values are the authority.
hc upgrade --install "$release" "$root/deploy/helm/scarab" -n "$ns" \
  -f "$here/values.yaml" -f "$values" ${extra_values:+-f "$extra_values"} --force-conflicts >/dev/null
echo "$mode" > "$state/mode"
kc rollout status -n "$ns" deploy/"$release" --timeout=180s
kc rollout status -n "$ns" statefulset/"$release-workspace" --timeout=180s

FORGEJO_TUNNEL_URL="$tunnel_url" bash "$here/forgejo.sh"

if [ "$mode" = dogfood ]; then
  tunnel_unchecked=1
  kc apply -n "$ns" -f "$here/cloudflared.yaml" >/dev/null
  kc rollout status -n "$ns" deploy/scarab-cloudflared --timeout=120s

  # The release signs people in with GitHub, but it holds the GitHub App's
  # key and is a staging stack: the UI and API are for this machine only.
  # The tunnel's public hostname must therefore route only `^/webhooks/`
  # (path rule in the Cloudflare dashboard; the catch-all answers 404). The
  # login callback is on localhost, so nothing else needs to be public.
  # Check that from outside, through Cloudflare, and take the tunnel down if
  # anything else reaches Scarab, or if it cannot be checked.
  resolve=()
  if ! python3 -I -c 'import socket, sys; socket.gethostbyname(sys.argv[1])' "$dogfood_host" 2>/dev/null; then
    # A resolver that cached "no such name" before the DNS record existed
    # keeps answering that until the cache expires; ask 1.1.1.1 instead.
    ip="$(dig +short @1.1.1.1 "$dogfood_host" A 2>/dev/null | grep -E '^[0-9.]+$' | head -n 1 || true)"
    [ -z "$ip" ] || resolve=(--resolve "$dogfood_host:443:$ip")
  fi
  public_code() { # $1 method, $2 path: the status the public hostname answers
    curl -s -o /dev/null -w '%{http_code}' --max-time 15 ${resolve[@]+"${resolve[@]}"} \
      -X "$1" "https://$dogfood_host$2" 2>/dev/null || true
  }
  echo "==> checking from outside that the tunnel serves only /webhooks/"
  root_code=000; api_code=000
  for _ in $(seq 1 30); do
    root_code="$(public_code GET /)"; api_code="$(public_code GET /v1/repos)"
    # 000: no answer; 5xx: Cloudflare has no live connection to the tunnel yet.
    case "$root_code$api_code" in *000*|5*|???5*) sleep 2 ;; *) break ;; esac
  done
  if [ "$root_code" != 404 ] || [ "$api_code" != 404 ]; then
    kc scale deploy scarab-cloudflared -n "$ns" --replicas=0 >/dev/null
    die "the tunnel is down again (cloudflared scaled to 0): through https://\$DOGFOOD_HOSTNAME,
  / answered HTTP $root_code and /v1/repos HTTP $api_code; both must be Cloudflare's 404.
  The UI and API of this stack are for this machine only: it holds the GitHub App's key.
  In the Cloudflare dashboard, set the tunnel's public hostname Path to ^/webhooks/
  (the catch-all rule then answers 404), and rerun 'just stack up dogfood'.
  The UI and API stay on ${ui_url:-$base}. (000 or 5xx means the check could not reach the tunnel.)"
  fi
  tunnel_unchecked=0
  webhook_code="$(public_code POST /webhooks/github)"
  echo "    / and /v1/repos -> 404 (not routed); unsigned POST /webhooks/github -> HTTP $webhook_code"
  case "$webhook_code" in
    400|401) ;;
    *) echo "warning: an unsigned webhook should reach Scarab and get 400/401; GitHub deliveries may not arrive" >&2 ;;
  esac
fi

# --- 7. the host can reach it ---------------------------------------------------
ok=0
for _ in $(seq 1 30); do
  if curl -sf "$base/readyz" >/dev/null 2>&1; then ok=1; break; fi
  sleep 1
done
[ "$ok" = 1 ] || die "the release is rolled out but $base/readyz does not answer from the host"

org="${SCARAB_DEV_ORG:-thulasi-ram}"
[ "$mode" = dogfood ] && org="$(env_get "$dogfood_env" SCARAB_ORG)"
if [ "$mode" != dogfood ]; then
  # ADR-0071 (c-full c2): a Deploy Run is admitted only into an org Environment
  # that exists and lists its Project as eligible. The dogfood pipeline deploys
  # to `production`, so seed it, eligible for every Project, when it is missing
  # (the PUTs replace, so an existing one is left as configured). Needs
  # dev-insecure; the dogfood stack has login on, so seed.sh's checklist below
  # lists it as a step for its Owner.
  code="$(curl -s -o /dev/null -w '%{http_code}' "$base/v1/orgs/$org/environments/production")"
  case "$code" in
    200) ;;
    404)
      curl -sf -X PUT "$base/v1/orgs/$org/environments/production" -H 'content-type: application/json' \
        -d '{"description":"seeded by deploy/kind/up.sh for the dogfood pipeline"}' >/dev/null \
      && curl -sf -X PUT "$base/v1/orgs/$org/environments/production/eligibility" \
        -H 'content-type: application/json' -d '{"all_projects":true}' >/dev/null \
      && echo "==> seeded org Environment $org/production (eligible: all projects)" \
      || echo "warning: could not seed org Environment $org/production" >&2
      ;;
    *) echo "warning: could not read org Environment $org/production (HTTP $code); not seeding it" >&2 ;;
  esac
fi

if [ "$mode" = dogfood ]; then
  # The Owner and member role bindings (`scarab-server admin grant
  # --if-absent` in the server Pod). On a fresh database also the one-time
  # steps that need a passkey: the `production` Environment and the clone
  # library, installed from its source like any library. On an existing one,
  # the step that installs it when the installed version is not
  # libs/clone.yaml. Without
  # login up.sh did those with plain curl.
  if [ "$fresh_database" = 1 ]; then
    bash "$here/seed.sh" --checklist
  else
    bash "$here/seed.sh"
  fi

  # A fresh database has no GitHub installation until GitHub delivers one. The
  # signed synthetic `installation:created` registers it now. Idempotent.
  install_id="$(env_get "$dogfood_env" SCARAB_INSTALL_ID)"
  repo="$(env_get "$dogfood_env" SCARAB_REPO)"
  if [ -n "$install_id" ] && [ -n "$repo" ]; then
    body="$(printf '{"action":"created","installation":{"id":%s,"account":{"login":"%s"}},"repositories":[{"full_name":"%s/%s"}]}' \
      "$install_id" "$org" "$org" "$repo")"
    sig="sha256=$(printf '%s' "$body" | openssl dgst -sha256 -hmac "$webhook_secret" | awk '{print $2}')"
    code="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$base/webhooks/github" \
      -H 'content-type: application/json' -H 'x-github-event: installation' \
      -H "x-github-delivery: kind-up-$$" -H "x-hub-signature-256: $sig" -d "$body")"
    echo "==> registered GitHub installation for $org/$repo (HTTP $code)"
  fi
fi

echo "==> up: ${ui_url:-$base} (mode: $mode)"
echo "    server log:  just stack logs     Depot: just stack logs depot, just stack status"
echo "    pipelines:   just stack demo, just stack demo chain"
if [ "$mode" = dogfood ]; then
  echo "    sign in:     $ui_url with GitHub (localhost, not 127.0.0.1: passkeys refuse an IP address)"
  echo "    seed again:  just stack seed dogfood (grants; add the checklist with: just stack seed dogfood checklist)"
  echo "    GitHub webhooks: https://\$DOGFOOD_HOSTNAME/webhooks/github (only /webhooks/ is public)"
fi
