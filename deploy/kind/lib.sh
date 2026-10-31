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
