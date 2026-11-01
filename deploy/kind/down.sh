#!/usr/bin/env bash
# Tear down the kind dev stack.
#
#   deploy/kind/down.sh          delete the kind cluster: the release, Postgres,
#                                MinIO and all their data
#   deploy/kind/down.sh keep     uninstall the release and delete both
#                                tunnels (dogfood's and e2e's); the cluster,
#                                Postgres, MinIO and their PVCs stay, so the
#                                next `just up` starts on the same data
#
# The generated dev secrets in deploy/kind/.state are kept either way, so a
# kept database still decrypts with the same master key.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=deploy/kind/lib.sh
. "$here/lib.sh"   # cluster, kubeconfig, kc, hc
what="${1:-all}"

case "$what" in
  all)
    echo "==> deleting kind cluster '$cluster'"
    kind delete cluster --name "$cluster" --kubeconfig "$kubeconfig" 2>/dev/null || true
    rm -f "$kubeconfig" "$here/.state/loaded-images" "$here/.state/mode"
    echo "==> down."
    ;;
  keep)
    if ! kind get clusters 2>/dev/null | grep -qx "$cluster"; then
      echo "==> no kind cluster '$cluster'; nothing to do"
      exit 0
    fi
    echo "==> uninstalling release $release (Postgres, MinIO and their data stay)"
    hc uninstall "$release" -n "$ns" --ignore-not-found
    kc delete deploy scarab-cloudflared -n "$ns" --ignore-not-found
    # The e2e Forgejo quick tunnel, if a run left it: Forgejo is public while
    # it runs.
    kc delete deploy scarab-forgejo-tunnel -n "$ns" --ignore-not-found
    echo "==> down (kept the cluster and its data)."
    ;;
  *)
    echo "usage: deploy/kind/down.sh [keep]" >&2
    exit 2
    ;;
esac
