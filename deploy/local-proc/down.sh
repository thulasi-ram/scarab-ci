#!/usr/bin/env bash
# Tear down the Scarab dev stack: stop the server, remove the kind cluster and
# the Postgres/MinIO containers. Best-effort on the containers and safe to run
# repeatedly, but NOT best-effort on the server: it waits for the process named
# by server.pid to actually die (escalating to SIGKILL) and exits non-zero if it
# could not reap it, because a survivor silently poisons the next `just up`.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cluster="scarab-dev"
rc=0

# Same helpers as up.sh, same reasons: /dev/tcp because these scripts assume no
# lsof, and ps because a bare `kill -0` can't tell a live process from a zombie
# or from an unrelated one that recycled the pid.
port_listening() { (exec 3<>"/dev/tcp/$1/$2") 2>/dev/null; }
proc_alive() {
  case "$(ps -o stat= -p "$1" 2>/dev/null)" in
    '' | Z*) return 1 ;;
  esac
  return 0
}
is_scarab_server() {
  case "$(ps -o comm= -p "$1" 2>/dev/null)" in
    *scarab-server*) return 0 ;;
  esac
  return 1
}

# `kill` and hope was not enough: a server that ignores SIGTERM used to keep
# SCARAB_ADDR while the pidfile naming it was already deleted, so nothing could
# ever reap it and the next up.sh inherited an orphan. Wait for the exit,
# escalate, and only forget the pid once the process is really gone.
#
# Two processes now (ADR-0061): the converged control plane and the workspace
# service, same binary, different roles. Both are ours and both must die, or the
# next `just up` refuses on a held port.
stop_pidfile() {
  local label="$1" file="$2" pid
  [[ -f "$file" ]] || return 0
  pid="$(cat "$file" 2>/dev/null || true)"
  if [[ -n "$pid" ]] && proc_alive "$pid" && is_scarab_server "$pid"; then
    echo "==> stopping $label (pid $pid)"
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 10); do
      proc_alive "$pid" || break
      sleep 1
    done
    if proc_alive "$pid"; then
      echo "    it ignored SIGTERM; sending SIGKILL"
      kill -9 "$pid" 2>/dev/null || true
      sleep 1
    fi
  fi
  if [[ -n "$pid" ]] && proc_alive "$pid" && is_scarab_server "$pid"; then
    echo "error: $label (pid $pid) survived SIGKILL; keeping $(basename "$file") so it can still be found" >&2
    rc=1
  else
    rm -f "$file"
  fi
}

stop_pidfile "scarab-server" "$here/server.pid"
stop_pidfile "the workspace service" "$here/workspace.pid"

# An orphan from a run that predates the reaping above (or a stray `just serve`)
# still holding the port is the exact condition that makes the next up.sh verify
# a stale binary. up.sh refuses in that case; say so here too, while the operator
# is already looking, instead of letting them find out one build later. A warning
# and not a failure: that listener is not ours to reap, and this script runs from
# the EXIT traps of `just e2e`/`just forgejo-verify`, where a non-zero teardown
# would turn unrelated noise into a red run.
if [[ -f "$here/.env" ]]; then
  # shellcheck disable=SC1091
  set -a; . "$here/.env"; set +a
  probe="${SCARAB_ADDR:-127.0.0.1:8080}"
  probe="${probe/0.0.0.0/127.0.0.1}"
  if port_listening "${probe%:*}" "${probe##*:}"; then
    echo "warning: something is STILL listening on $probe after teardown —" >&2
    echo "         an orphaned server, or a 'just serve' you meant to keep." >&2
    echo "         Find it with: lsof -nP -iTCP:${probe##*:} -sTCP:LISTEN" >&2
    echo "         The next 'just up' will refuse to start until it is gone." >&2
  fi
fi

echo "==> docker compose down"
docker compose -f "$here/compose.yaml" down -v || true

echo "==> deleting kind cluster '$cluster'"
kind delete cluster --name "$cluster" 2>/dev/null || true

rm -f "$here/.kubeconfig" "$here/server.log" "$here/workspace.log"
# The warm tier is a CACHE with no promise (ADR-0061): cold (the compose MinIO,
# whose volume `docker compose down -v` just removed) is the guarantee, so
# deleting it here loses nothing that survived anyway.
rm -rf "$here/../../.scarab/workspace-cas"
if [[ "$rc" -eq 0 ]]; then
  echo "==> down."
else
  echo "==> down, but a server process outlived the teardown (see above)." >&2
fi
exit "$rc"
