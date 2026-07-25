#!/usr/bin/env bash
# Bring up the full Scarab dev stack and start scarab-server against it:
#   - Postgres + MinIO via docker compose
#   - a kind cluster (isolated kubeconfig at deploy/local-proc/.kubeconfig)
#   - scarab-server --role converged --serve in the background
#
# Idempotent: safe to re-run — a server left by an earlier run of THIS script is
# reaped first. It never returns success unless the server it just started is
# the one answering SCARAB_ADDR; anything else already on that port is fatal.
#
# Requires docker, kind, kubectl.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
kubeconfig="$here/.kubeconfig"
cluster="scarab-dev"

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: '$1' not found on PATH" >&2; exit 1; }; }
need docker; need kind; need kubectl

# Liveness helpers. Deliberately not lsof/ss/fuser — these scripts assume only
# docker/kind/kubectl/curl, and bash's own /dev/tcp (already used for the
# Postgres reuse check below) answers the only question we have: "is anything
# listening there?". Works the same on macOS and Linux.
port_listening() { (exec 3<>"/dev/tcp/$1/$2") 2>/dev/null; }
proc_alive() {
  # `kill -0` is not enough for OUR OWN background child: between its death and
  # the shell reaping it, it is a zombie that still answers signals. A server
  # that died on AddrInUse would then read as "still starting". Ask ps instead.
  case "$(ps -o stat= -p "$1" 2>/dev/null)" in
    '' | Z*) return 1 ;;
  esac
  return 0
}
is_scarab_server() {
  # Pid reuse guard: a recycled pid must never be signalled just because it
  # inherited the number sitting in server.pid.
  case "$(ps -o comm= -p "$1" 2>/dev/null)" in
    *scarab-server*) return 0 ;;
  esac
  return 1
}

echo "==> starting Postgres + MinIO (docker compose)"
# Same tolerance as `just test`: something already serving 55432 (an earlier
# stack, or an ad-hoc dev container) is reused — compose then only provides
# MinIO — instead of failing the port bind.
if (exec 3<>/dev/tcp/127.0.0.1/55432) 2>/dev/null; then
  echo "    reusing the Postgres already listening on 127.0.0.1:55432"
  pg_managed=0
  docker compose -f "$here/compose.yaml" up -d minio createbuckets
else
  pg_managed=1
  docker compose -f "$here/compose.yaml" up -d
fi

echo "==> creating kind cluster '$cluster' (kubeconfig: $kubeconfig)"
if ! kind get clusters 2>/dev/null | grep -qx "$cluster"; then
  kind create cluster --name "$cluster" --config "$here/kind.yaml" --kubeconfig "$kubeconfig"
else
  kind export kubeconfig --name "$cluster" --kubeconfig "$kubeconfig"
fi

echo "==> ensuring namespace 'scarab'"
KUBECONFIG="$kubeconfig" kubectl create namespace scarab \
  --dry-run=client -o yaml | KUBECONFIG="$kubeconfig" kubectl apply -f -

if [ "$pg_managed" = 1 ]; then
  echo "==> waiting for Postgres to be healthy"
  for _ in $(seq 1 30); do
    if docker compose -f "$here/compose.yaml" exec -T postgres pg_isready -U scarab -d scarab >/dev/null 2>&1; then
      break
    fi
    sleep 1
  done
fi

echo "==> loading the dev env"
# Env (DB/S3/namespace/addr) comes from the one dev env file — single source of
# truth, shared with `just serve`/`just ui`. Gitignored; seed it from the
# committed template on first run.
[ -f "$here/.env" ] || cp "$here/.env.example" "$here/.env"
set -a; . "$here/.env"; set +a
# A caller-supplied OVERLAY, applied last so it wins over the dev defaults. This
# is how a verification tier reconfigures the stack for its own run without
# editing (or racing on) the shared dev env file — e.g. `just forgejo-verify`
# needs the server bound on 0.0.0.0 with a public URL a container can reach.
if [ -n "${SCARAB_ENV_EXTRA:-}" ]; then
  [ -f "$SCARAB_ENV_EXTRA" ] || { echo "error: SCARAB_ENV_EXTRA=$SCARAB_ENV_EXTRA does not exist" >&2; exit 1; }
  echo "    applying env overlay: $SCARAB_ENV_EXTRA"
  set -a; . "$SCARAB_ENV_EXTRA"; set +a
fi
# KUBECONFIG is dynamic (this run's kind cluster), so it's set here, not in .env.
export KUBECONFIG="$kubeconfig"

# Probe the address the server actually binds (from .env — e.g. a dev machine
# whose :8080 is taken sets SCARAB_ADDR to another port); 0.0.0.0 via loopback.
addr="${SCARAB_ADDR:-127.0.0.1:8080}"
probe="${addr/0.0.0.0/127.0.0.1}"
base="http://$probe"
probe_host="${probe%:*}"
probe_port="${probe##*:}"

echo "==> claiming $probe for this run's server"
# The two ways this script used to report success against a server it did not
# start. (1) A scarab-server from an EARLIER up.sh in this worktree is still
# running; we overwrite its pidfile, orphaning it where down.sh can never reap
# it. (2) Anything else holds SCARAB_ADDR, so the process we launch dies with
# `AddrInUse` — visible only in server.log — and the whole stack, including a
# verification tier, then talks to a stale binary. That can as easily fake a
# PASS as a FAIL, which is the end of the tier's value as evidence.
#
# Semantics: this script OWNS the process named by server.pid, so it reaps that
# one itself — up.sh is documented idempotent, and re-running must REPLACE our
# server rather than refuse. A listener it does NOT own is fatal: we cannot
# tell a colleague's `just serve` from a stale orphan, and guessing wrong is how
# you end up grading the wrong binary. The human is told how to clear it.
if [ -f "$here/server.pid" ]; then
  old="$(cat "$here/server.pid" 2>/dev/null || true)"
  if [ -n "$old" ] && proc_alive "$old" && is_scarab_server "$old"; then
    echo "    reaping the scarab-server left by an earlier up.sh (pid $old)"
    kill "$old" 2>/dev/null || true
    for _ in $(seq 1 10); do
      proc_alive "$old" || break
      sleep 1
    done
    if proc_alive "$old"; then
      echo "    it ignored SIGTERM; sending SIGKILL"
      kill -9 "$old" 2>/dev/null || true
      sleep 1
    fi
  fi
  rm -f "$here/server.pid"
fi
if port_listening "$probe_host" "$probe_port"; then
  {
    echo "error: something is already listening on $probe — refusing to start."
    echo "  It is not a server this script owns (server.pid named none, or that one"
    echo "  was just reaped), so the stack would be driven against the WRONG process."
    echo "  Find it with:  lsof -nP -iTCP:$probe_port -sTCP:LISTEN"
    echo "  Then: stop it, or run 'just down', or point this stack elsewhere with"
    echo "  SCARAB_ADDR in deploy/local-proc/.env."
  } >&2
  exit 1
fi

echo "==> building scarab-server"
( cd "$root" && cargo build -p scarab-server )

echo "==> starting scarab-server (converged) in the background"
nohup "$root/target/debug/scarab-server" --role converged --serve \
  > "$here/server.log" 2>&1 &
server_pid=$!
echo "$server_pid" > "$here/server.pid"

fail_with_log() {
  echo "error: $1" >&2
  echo "---- tail of deploy/local-proc/server.log ----" >&2
  tail -n 40 "$here/server.log" >&2 || true
  exit 1
}

echo "==> waiting for the API (/healthz) from pid $server_pid"
# The port was proven free above and this child is proven alive below, so a
# /healthz that answers can only be answered by the server WE started. Both
# halves are load-bearing: a dead nohup'd child must never read as a healthy
# stack, and neither must someone else's healthy stack.
for _ in $(seq 1 30); do
  if ! proc_alive "$server_pid"; then
    rm -f "$here/server.pid"
    fail_with_log "scarab-server (pid $server_pid) exited during startup; it never served $base"
  fi
  if curl -sf "$base/healthz" >/dev/null 2>&1; then
    echo "==> scarab-server is up on $base (pid $server_pid, logs: deploy/local-proc/server.log)"
    echo "    run 'just demo' to submit a pipeline and watch it complete."
    exit 0
  fi
  sleep 1
done
fail_with_log "scarab-server (pid $server_pid) is alive but did not answer $base/healthz within 30s"
