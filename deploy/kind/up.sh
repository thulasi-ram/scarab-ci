#!/usr/bin/env bash
# Bring up the kind dev stack (ADR-0077 Amendment, 2026-11-01): one kind
# cluster `scarab-dev`, Postgres and MinIO in it, and one Helm release of the
# real chart whose server and Depot run a binary built on the host.
#
#   deploy/kind/up.sh [dev|dogfood]
#
#   dev      (`just up`)      dev-insecure, database `scarab`, bucket `scarab-logs`
#   dogfood  (`just dogfood`) GitHub App mode from ~/.config/scarab/dogfood.env,
#                             database `scarab_dogfood`, bucket `scarab-dogfood`,
#                             and a cloudflared tunnel to the public hostname
#
# Idempotent: re-running rebuilds the binary and rolls the Pods only when the
# binary changed. `just reload` is the short path for that alone.
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
#   5. apply Postgres and MinIO, create the mode's database;
#   6. helm upgrade --install the chart with deploy/kind/values.yaml plus the
#      mode's values.
#
# Kube safety: every kubectl and helm call goes through kc/hc below, which use
# the cluster's own kubeconfig file (deploy/kind/.kubeconfig) and check that its
# server is this kind cluster's API port before each call. The ambient
# ~/.kube/config is never read.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
# shellcheck source=deploy/kind/lib.sh
. "$here/lib.sh"   # cluster, node, ns, release, kubeconfig, kc, hc, die
mode="${1:-dev}"
state="$here/.state"           # gitignored: generated config, dev secrets
mount="$here/.mount"           # gitignored: staged binaries + UI, mounted on the node
node_mount=/scarab             # where $mount appears on the kind node
node_port=30080                # service.nodePort in values.yaml
dogfood_env="${SCARAB_DOGFOOD_ENV:-$HOME/.config/scarab/dogfood.env}"

need() { command -v "$1" >/dev/null 2>&1 || die "'$1' not found on PATH"; }
need docker; need kind; need kubectl; need helm; need curl; need python3

# `helm upgrade --force-conflicts` below exists only in Helm 4.
helm_major="$(helm version --template '{{.Version}}' 2>/dev/null | sed -n 's/^v\([0-9][0-9]*\)\..*/\1/p')"
[ "${helm_major:-0}" -ge 4 ] || die "Helm 4 is required (found $(helm version --short 2>/dev/null || echo none)); see https://helm.sh/docs/intro/install/"

case "$mode" in
  dev|dogfood) ;;
  *) die "unknown mode '$mode' (expected dev or dogfood)" ;;
esac

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
    die "just dogfood needs the named Cloudflare tunnel: set$missing in $dogfood_env
  CLOUDFLARE_TUNNEL_TOKEN  the tunnel's token (Zero Trust > Networks > Tunnels)
  DOGFOOD_HOSTNAME         its public hostname, routed to http://scarab.scarab.svc.cluster.local:80"
  fi
  for k in SCARAB_GITHUB_APP_ID SCARAB_APP_PEM SCARAB_GITHUB_WEBHOOK_SECRET SCARAB_MASTER_KEY SCARAB_RESULTS_TOKEN_SECRET; do
    [ -n "$(env_get "$dogfood_env" "$k")" ] || die "$k is empty or missing in $dogfood_env"
  done
  pem="$(env_get "$dogfood_env" SCARAB_APP_PEM)"
  [ -f "$pem" ] || die "SCARAB_APP_PEM in $dogfood_env names a file that does not exist"
  database=scarab_dogfood
  bucket=scarab-dogfood
else
  database=scarab
  bucket=scarab-logs
fi

# The host port: SCARAB_ADDR from deploy/local-proc/.env (lib.sh scarab_addr;
# `just ui`'s SCARAB_API_URL must name the same port). Only that key is read;
# the rest of the file describes proc mode.
scarab_addr   # addr_file, host_ip, host_port, base

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
target_dir="${CARGO_TARGET_DIR:-$root/target}"
case "$target_dir" in /*) ;; *) target_dir="$root/$target_dir" ;; esac
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
ui_src="$root/ui/scarab-web-ui"
ui_dist="$ui_src/dist"
if command -v npm >/dev/null 2>&1; then
  if [ ! -f "$ui_dist/index.html" ] \
     || [ -n "$(find "$ui_src/src" "$ui_src/index.html" "$root/ui/brand" -newer "$ui_dist/index.html" -print -quit 2>/dev/null)" ]; then
    echo "==> building the web UI (npm)"
    [ -d "$ui_src/node_modules" ] || npm --prefix "$ui_src" ci
    npm --prefix "$ui_src" run build >/dev/null
  fi
else
  echo "    npm not found: the stack runs without the web UI (the API is unaffected)"
fi
if [ -f "$ui_dist/index.html" ]; then
  rm -rf "$mount/ui.new"
  cp -R "$ui_dist" "$mount/ui.new"
  rm -rf "$mount/ui.old"
  [ -d "$mount/ui" ] && mv "$mount/ui" "$mount/ui.old"
  mv "$mount/ui.new" "$mount/ui"
  rm -rf "$mount/ui.old"
fi

# --- 2. the cluster ---------------------------------------------------------
# A `scarab-dev` cluster must have this worktree's mount and this host port, both
# fixed at creation. One created by proc mode (deploy/local-proc/kind.yaml), by
# another worktree, or before SCARAB_ADDR changed has neither. Recreating it
# deletes its Postgres and MinIO data, so that needs SCARAB_KIND_RECREATE=1.
misfit=""
cluster_fits() {
  docker inspect "$node" >/dev/null 2>&1 || { misfit="its node container $node is missing"; return 1; }
  local mounts ports
  mounts="$(docker inspect -f '{{range .Mounts}}{{.Source}}={{.Destination}}{{"\n"}}{{end}}' "$node")"
  printf '%s\n' "$mounts" | grep -qxF "$mount=$node_mount" \
    || { misfit="it does not mount this worktree's $mount (another worktree or proc mode created it)"; return 1; }
  ports="$(docker inspect -f '{{range $p, $b := .HostConfig.PortBindings}}{{range $b}}{{$p}}={{.HostIp}}:{{.HostPort}}{{"\n"}}{{end}}{{end}}' "$node")"
  printf '%s\n' "$ports" | grep -qxF "$node_port/tcp=$host_ip:$host_port" \
    || { misfit="it does not publish the API on $host_ip:$host_port (SCARAB_ADDR in $addr_file changed since it was created)"; return 1; }
}
exists=0
kind get clusters 2>/dev/null | grep -qx "$cluster" && exists=1
if [ "$exists" = 1 ] && cluster_fits; then
  echo "==> reusing kind cluster '$cluster'"
  if [ "$(docker inspect -f '{{.State.Running}}' "$node")" != true ]; then
    echo "    its node container is stopped; starting it"
    docker start "$node" >/dev/null || die "could not start the stopped node container $node ('just down' deletes the cluster)"
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
    die "something already listens on $host_ip:$host_port (SCARAB_ADDR in $addr_file).
  A proc-mode server ('deploy/local-proc/down.sh' stops it) or a 'just serve'? Find it with:
    lsof -nP -iTCP:$host_port -sTCP:LISTEN"
  fi
  if [ "$exists" = 1 ]; then
    [ "${SCARAB_KIND_RECREATE:-0}" = 1 ] || die "kind cluster '$cluster' does not fit: $misfit.
  Recreating it deletes its Postgres and MinIO data (both modes' databases and buckets).
  Run 'just up' from the worktree or with the SCARAB_ADDR that created it, or
  recreate it: SCARAB_KIND_RECREATE=1 just up   (or 'just down' first)"
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

# --- 3. images ----------------------------------------------------------------
# Loaded as a ONE-platform archive: `kind load docker-image` imports every
# platform and fails on a multi-arch image under the containerd image store.
# A load is skipped when the node already has the tag and this script last
# loaded the same local image ID.
load_image() {
  local img="$1" id archive
  id="$(docker image inspect -f '{{.Id}}' "$img")"
  if grep -qxF "$img $id" "$state/loaded-images" 2>/dev/null \
     && docker exec "$node" crictl inspecti "$img" >/dev/null 2>&1; then
    return 0
  fi
  echo "    loading $img into $cluster"
  archive="$(mktemp "$state/image.XXXXXX")"
  docker save --platform "$platform" -o "$archive" "$img"
  kind load image-archive "$archive" --name "$cluster" >/dev/null
  rm -f "$archive"
  { grep -v "^$img " "$state/loaded-images" 2>/dev/null || true; echo "$img $id"; } > "$state/loaded-images.new"
  mv "$state/loaded-images.new" "$state/loaded-images"
}
pull_image() { # a third-party image, pulled once for this platform
  docker image inspect "$1" >/dev/null 2>&1 || docker pull -q --platform "$platform" "$1" >/dev/null
}
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
infra_images="$(sed -n 's/^ *image: *\([^ #]*\).*/\1/p' "$here/postgres.yaml" "$here/minio.yaml")"
for img in $infra_images; do
  case "$img" in *@*) continue ;; esac
  pull_image "$img"
  load_image "$img"
done

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
if ! kc exec -n "$ns" deploy/scarab-postgres -- psql -U scarab -d scarab -tAc \
     "select 1 from pg_database where datname = '$database'" | grep -q 1; then
  echo "    creating database $database"
  kc exec -n "$ns" deploy/scarab-postgres -- createdb -U scarab "$database"
fi
database_url="postgres://scarab:scarab@scarab-postgres:5432/$database"

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
last_mode="$(cat "$state/mode" 2>/dev/null || true)"
if [ -n "$last_mode" ] && [ "$last_mode" != "$mode" ]; then
  # A step Pod of the current mode would carry on against the other mode's
  # server, Depot, bucket and token secrets, and its Attempt fails on resume.
  # Step Pods carry the executor's scarab.io/run label.
  active="$(kc get pods -A -l scarab.io/run \
    --field-selector=status.phase!=Succeeded,status.phase!=Failed -o name | wc -l | tr -d ' ')"
  if [ "$active" != 0 ] && [ "${SCARAB_KIND_FORCE_SWITCH:-0}" != 1 ]; then
    die "refusing to switch $last_mode -> $mode: $active step Pod(s) of the $last_mode stack are still running.
  Let their Runs finish (or cancel them), then rerun. To switch anyway and fail them:
    SCARAB_KIND_FORCE_SWITCH=1 just $([ "$mode" = dev ] && echo up || echo dogfood)"
  fi
  echo "==> mode $last_mode -> $mode: deleting the Depot's warm PVC"
  kc delete statefulset -n "$ns" "$release-workspace" --ignore-not-found --wait=true >/dev/null
  kc delete pvc -n "$ns" -l app.kubernetes.io/component=workspace --ignore-not-found --wait=true >/dev/null
fi

values="$(mktemp "$state/values.XXXXXX")"
libs_dir="$(mktemp -d "$state/libs.XXXXXX")"
trap 'rm -rf "$values" "$libs_dir"' EXIT
chmod 600 "$values"

# The clone library (ADR-0076 §9): checkouts run `lib:thulasi-ram/clone` with
# the image it pins, so the release gets a manifest listing a copy of
# libs/clone.yaml that pins the scarab-clone:dev image built above (the same
# approach `just local-helm local` used). `just dogfood` installs that copy
# into the org after the roll-out.
python3 -I - "$root/libs/clone.yaml" scarab-clone:dev > "$libs_dir/clone.yaml" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
new, n = re.subn(r"(?m)^(\s*image:\s*)ghcr\.io/thulasi-ram/scarab-clone\S*$", r"\g<1>" + sys.argv[2], text)
if n != 1:
    sys.exit("libs/clone.yaml: expected one scarab-clone image line")
sys.stdout.write(new)
PY
python3 -I "$root/scripts/libs_manifest.py" --libs "$libs_dir" \
  --repository github.com/thulasi-ram/scarab-ci \
  --commit "$(git -C "$root" rev-parse HEAD)" --release v0.0.0-kind > "$libs_dir/manifest.json"

# Roll the Pods when the binary or the staged UI changes, and only then: helm
# sees a new Pod template annotation. The server checks for ui/index.html at
# start, so a UI staged after the Pod started needs a roll too.
binary_sum="$({ cat "$mount/bin/scarab-server"; cat "$mount/ui/index.html" 2>/dev/null || true; } | shasum -a 256 | cut -c1-16)"
{
  echo "podAnnotations:"
  echo "  scarab.dev/binary-sha256: \"$binary_sum\""
  echo "scarab:"
  echo "  s3:"
  echo "    bucket: \"$bucket\""
  echo "  libsManifest: |"
  sed 's/^/    /' "$libs_dir/manifest.json"
  if [ "$mode" = dogfood ]; then
    # Dev-insecure, as the colima dogfood ran (deploy/local-helm/.env): no
    # login, every caller is an Owner. The tunnel makes that public; put a
    # Cloudflare Access policy on the hostname, or set
    # SCARAB_DOGFOOD_DEV_INSECURE=false once OAuth values are added here.
    echo "  devInsecure: ${SCARAB_DOGFOOD_DEV_INSECURE:-true}"
    echo "  githubAppId: \"$(env_get "$dogfood_env" SCARAB_GITHUB_APP_ID)\""
    echo "  publicUrl: \"https://$(env_get "$dogfood_env" DOGFOOD_HOSTNAME)\""
    timeout="$(env_get "$dogfood_env" SCARAB_STEP_TIMEOUT_SECS)"
    echo "  stepTimeoutSecs: \"${timeout:-3600}\""
  else
    echo "  devInsecure: true"
  fi
  echo "secrets:"
  echo "  databaseUrl: \"$database_url\""
  echo "  masterKey: \"$master_key\""
  echo "  resultsTokenSecret: \"$results_secret\""
  echo "  workspaceTokenSecret: \"$workspace_secret\""
  if [ "$mode" = dogfood ]; then
    echo "  githubWebhookSecret: \"$webhook_secret\""
    echo "  githubAppPemSecret:"
    echo "    name: scarab-github-app"
    echo "    key: github-app.pem"
  fi
} > "$values"

if [ "$mode" = dogfood ]; then
  echo "==> GitHub App PEM secret, cloudflared"
  kc create secret generic scarab-github-app -n "$ns" \
    --from-file="github-app.pem=$pem" --dry-run=client -o yaml | kc apply -f - >/dev/null
  # The token goes through a file, not argv, so it never shows in `ps`.
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
  -f "$here/values.yaml" -f "$values" --force-conflicts >/dev/null
echo "$mode" > "$state/mode"
kc rollout status -n "$ns" deploy/"$release" --timeout=180s
kc rollout status -n "$ns" statefulset/"$release-workspace" --timeout=180s

if [ "$mode" = dogfood ]; then
  kc apply -n "$ns" -f "$here/cloudflared.yaml" >/dev/null
  kc rollout status -n "$ns" deploy/scarab-cloudflared --timeout=120s
fi

# --- 7. the host can reach it ---------------------------------------------------
ok=0
for _ in $(seq 1 30); do
  if curl -sf "$base/readyz" >/dev/null 2>&1; then ok=1; break; fi
  sleep 1
done
[ "$ok" = 1 ] || die "the release is rolled out but $base/readyz does not answer from the host"

# ADR-0071 (c-full c2): a Deploy Run is admitted only into an org Environment
# that exists and lists its Project as eligible. The dogfood pipeline deploys to
# `production`, so seed it, eligible for every Project, when it is missing (the
# PUTs replace, so an existing one is left as configured). Needs dev-insecure.
org="${SCARAB_DEV_ORG:-thulasi-ram}"
[ "$mode" = dogfood ] && org="$(env_get "$dogfood_env" SCARAB_ORG)"
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

if [ "$mode" = dogfood ]; then
  # Install the clone library copy into the org, replacing a copy that pinned
  # another image (the hash is checked against the manifest above).
  libs="$base/v1/orgs/$org/libraries"
  want="$(shasum -a 256 "$libs_dir/clone.yaml" | awk '{print $1}')"
  have="$(curl -sf "$libs" | python3 -I -c '
import json, sys
for i in json.load(sys.stdin).get("installations", []):
    if (i["publisher"], i["name"]) == ("thulasi-ram", "clone"):
        print(i["content_hash"])
' || true)"
  if [ "$have" != "$want" ]; then
    [ -z "$have" ] || curl -sf -o /dev/null -X DELETE "$libs/thulasi-ram/clone" || true
    python3 -I -c 'import json, sys; print(json.dumps({"files": {"thulasi-ram/clone": open(sys.argv[1]).read()}}))' \
        "$libs_dir/clone.yaml" \
      | curl -sf -o /dev/null -X POST "$base/v1/orgs/$org/library-defaults/retry" \
          -H 'content-type: application/json' -d @- \
      && echo "==> installed the clone library (pins scarab-clone:dev) into $org" \
      || echo "warning: could not install the clone library into $org; install it from Settings > Libraries" >&2
  fi

  # A fresh database has no GitHub installation until GitHub delivers one. The
  # same signed synthetic `installation:created` deploy/local-helm/reseed.sh
  # sent registers it now. Idempotent.
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

echo "==> up: $base (mode: $mode)"
echo "    server log:  just logs          Depot: just workspace-status"
echo "    pipelines:   just demo, just demo-chain"
if [ "$mode" = dogfood ]; then
  # Checked, not printed: values from dogfood.env stay out of terminal logs.
  public="https://$(env_get "$dogfood_env" DOGFOOD_HOSTNAME)"
  code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 20 "$public/readyz" || true)"
  echo "    public URL:  https://\$DOGFOOD_HOSTNAME from $dogfood_env (/readyz -> HTTP $code)"
  [ "$code" = 200 ] || echo "    (not 200: check the tunnel's Public Hostname route in the Cloudflare dashboard;" \
    "a resolver that cached the name before its DNS record existed answers nothing until the cache expires)"
fi
