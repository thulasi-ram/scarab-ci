# Shared by deploy/kind/*.sh: the cluster's names and the kube safety check.
#
# Every kubectl and helm call goes through kc / hc, which use the cluster's own
# kubeconfig file (deploy/kind/.kubeconfig) and check, before each call, that
# its server is the API port docker publishes for this kind cluster's node. The
# ambient ~/.kube/config, where production contexts live, is never read.
# shellcheck shell=bash

kind_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cluster=scarab-dev
node="$cluster-control-plane"
ns=scarab
release=scarab
node_port=30080            # service.nodePort in values.yaml: the API on the node
kubeconfig="$kind_dir/.kubeconfig"

repo_root="$(cd "$kind_dir/../.." && pwd)"

die() { echo "error: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' not found on PATH"; }

# cargo_target_dir: where cargo puts its output, as an absolute path.
# CARGO_TARGET_DIR when set (a relative one is taken from the repo root, where
# the recipes run cargo), else <repo>/target.
cargo_target_dir() {
  local t="${CARGO_TARGET_DIR:-$repo_root/target}"
  case "$t" in /*) ;; *) t="$repo_root/$t" ;; esac
  printf '%s\n' "$t"
}

# Images for the kind node. $platform (linux/<docker arch>) is computed on
# first use unless the caller set it.
image_platform() {
  [ -n "${platform:-}" ] || platform="linux/$(docker version -f '{{.Server.Arch}}')"
}
pull_image() { # a third-party image, pulled once for this platform
  image_platform
  docker image inspect "$1" >/dev/null 2>&1 || docker pull -q --platform "$platform" "$1" >/dev/null
}
# load_image IMG: load a local image into the kind node as a ONE-platform
# archive (`kind load docker-image` imports every platform and fails on a
# multi-arch image under the containerd image store). Skipped when the node
# has the tag and this stack last loaded the same local image ID
# (.state/loaded-images).
load_image() {
  local img="$1" id archive loaded="$kind_dir/.state/loaded-images"
  image_platform
  id="$(docker image inspect -f '{{.Id}}' "$img")"
  if grep -qxF "$img $id" "$loaded" 2>/dev/null \
     && { docker exec "$node" crictl inspecti "$img" >/dev/null 2>&1 \
          || docker exec "$node" crictl inspecti "docker.io/library/$img" >/dev/null 2>&1; }; then
    return 0
  fi
  echo "    loading $img into $cluster"
  archive="$(mktemp "$kind_dir/.state/image.XXXXXX")"
  docker save --platform "$platform" -o "$archive" "$img"
  kind load image-archive "$archive" --name "$cluster" >/dev/null
  rm -f "$archive"
  { grep -v "^$img " "$loaded" 2>/dev/null || true; echo "$img $id"; } > "$loaded.new"
  mv "$loaded.new" "$loaded"
}

# env_get FILE KEY: the value of KEY's last line in an env file, read without
# sourcing it (values are never printed). The value is everything after the
# FIRST '=' (base64 keeps its '='). A leading `export `, a trailing CR and one
# pair of surrounding quotes are dropped.
env_get() {
  local v
  v="$(sed -n -E "s/^[[:space:]]*(export[[:space:]]+)?$2=//p" "$1" | tail -n 1 | tr -d '\r')"
  case "$v" in
    \"*\") v="${v#\"}"; v="${v%\"}" ;;
    \'*\') v="${v#\'}"; v="${v%\'}" ;;
  esac
  printf '%s\n' "$v"
}

# scarab_addr: the host address the stack answers on. SCARAB_ADDR from
# deploy/kind/.env (.env.example when there is none), the one dev env file
# `just ui` also reads. Normalised here for up.sh and the Justfile's demo
# recipes alike: a trailing comment, quotes and blanks are dropped; `localhost`
# and an empty host mean 127.0.0.1; a missing port means 8080, and a bare
# number is a port. Sets addr_file, host_ip, host_port and base (the URL to
# call; 0.0.0.0 is called through 127.0.0.1).
scarab_addr() {
  local raw
  addr_file="$kind_dir/.env"
  [ -f "$addr_file" ] || addr_file="$kind_dir/.env.example"
  raw="$(env_get "$addr_file" SCARAB_ADDR)"
  raw="${raw%%#*}"
  raw="$(printf '%s' "$raw" | tr -d "[:space:]\"'")"
  case "$raw" in
    '') host_ip=127.0.0.1; host_port=8080 ;;
    *:*) host_ip="${raw%:*}"; host_port="${raw##*:}" ;;
    *[!0-9]*) host_ip="$raw"; host_port=8080 ;;
    *) host_ip=127.0.0.1; host_port="$raw" ;;
  esac
  case "$host_ip" in ''|localhost) host_ip=127.0.0.1 ;; esac
  case "$host_port" in ''|*[!0-9]*) die "SCARAB_ADDR in $addr_file ('$raw') has no numeric port" ;; esac
  [[ "$host_ip" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || die "SCARAB_ADDR in $addr_file ('$raw'): the host must be an IPv4 address such as 127.0.0.1"
  base="http://${host_ip/0.0.0.0/127.0.0.1}:$host_port"
}

api_url() { # the API server URL docker publishes for this cluster's node
  local hp
  hp="$(docker port "$node" 6443/tcp 2>/dev/null | head -n 1)" || return 1
  [ -n "$hp" ] || return 1
  echo "https://${hp/0.0.0.0/127.0.0.1}"
}
verify_kubeconfig() {
  local want got
  [ -f "$kubeconfig" ] || die "no kubeconfig at $kubeconfig (is the stack up? 'just up')"
  want="$(api_url)" || die "kind cluster '$cluster' is not running ('just up')"
  got="$(KUBECONFIG="$kubeconfig" kubectl config view --minify -o jsonpath='{.clusters[0].cluster.server}')"
  [ "$got" = "$want" ] || die "refusing: $kubeconfig points at '$got', not this kind cluster ($want)"
}
# active_step_pods: how many step Pods (the executor's scarab.io/run label) in
# any namespace are neither Succeeded nor Failed. A Pod being deleted still
# counts until it is gone. up.sh refuses a mode switch while this is not 0.
active_step_pods() {
  kc get pods -A -l scarab.io/run \
    --field-selector=status.phase!=Succeeded,status.phase!=Failed -o name | wc -l | tr -d ' '
}

kc() { verify_kubeconfig; KUBECONFIG="$kubeconfig" kubectl --context "kind-$cluster" "$@"; }
hc() { verify_kubeconfig; KUBECONFIG="$kubeconfig" helm --kube-context "kind-$cluster" "$@"; }
