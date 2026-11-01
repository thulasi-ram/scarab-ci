#!/usr/bin/env bash
# kubectl against the kind dev stack, through its own kubeconfig file and the
# same server check deploy/kind/up.sh makes (deploy/kind/lib.sh). `just stack
# logs [depot]` and `just stack status` use it; so can you:
#
#   deploy/kind/kubectl.sh get pods -n scarab
set -euo pipefail
# shellcheck source=deploy/kind/lib.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
kc "$@"
