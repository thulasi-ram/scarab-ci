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
kubeconfig="$kind_dir/.kubeconfig"

die() { echo "error: $*" >&2; exit 1; }

# env_get FILE KEY: the value of KEY's last line in an env file, read without
# sourcing it. The value is everything after the FIRST '=' (base64 keeps its
# '='), with a trailing CR removed.
env_get() {
  sed -n "s/^$2=//p" "$1" | tail -n 1 | tr -d '\r'
}

# scarab_addr: the host address the stack answers on. SCARAB_ADDR from
# deploy/local-proc/.env (.env.example when there is none), the one dev env
# file `just ui` also reads. Normalised here for up.sh and the Justfile's demo
# recipes alike: a trailing comment, quotes and blanks are dropped; `localhost`
# and an empty host mean 127.0.0.1; a missing port means 8080, and a bare
# number is a port. Sets addr_file, host_ip, host_port and base (the URL to
# call; 0.0.0.0 is called through 127.0.0.1).
scarab_addr() {
  local raw
  addr_file="$kind_dir/../local-proc/.env"
  [ -f "$addr_file" ] || addr_file="$kind_dir/../local-proc/.env.example"
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
kc() { verify_kubeconfig; KUBECONFIG="$kubeconfig" kubectl --context "kind-$cluster" "$@"; }
hc() { verify_kubeconfig; KUBECONFIG="$kubeconfig" helm --kube-context "kind-$cluster" "$@"; }
