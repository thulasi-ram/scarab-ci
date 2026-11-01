#!/usr/bin/env bash
# `just chaos [mid-drain|mid-feed|all] [seed]` and `just chaos replay <file>`:
# ADR-0077 rank 1 on the kind release (Amendment 2026-11-01 §4).
#
#   1. up.sh chaos: refused while step Pods of the current mode run; deletes
#      the Depot's PVCs (a warm cache of another bucket would hide faults);
#      database `scarab_chaos` and bucket `scarab-chaos` on the same Postgres
#      and MinIO, reached through toxiproxy; 2 servers, 2 Depot replicas,
#      plugins off.
#   2. scarab-chaos (crates/scarab-chaos) runs each scenario: three chained
#      Runs, one Depot Pod deleted with grace 0 mid-drain or mid-feed, then
#      the invariant checker. Each run writes a directory under
#      $SCARAB_CHAOS_DIR (default deploy/kind/.chaos): scenario.json (the
#      replayable file), pods.jsonl, verdict.json, memory.log.
#   3. cancels the chaos database's non-terminal Runs through the API (a
#      scenario refuses to start while one is left), then switches the
#      release back to the mode that was active before (dev or
#      dogfood; e2e and anything else go back to dev, because restoring e2e
#      would open a new public Forgejo tunnel) and runs `just demo` there.
#
# Exit code: 0 when every verdict passed; 1 when a verdict failed (a finding);
# 2 when the harness could not run a scenario; 3 when no verdict failed but a
# scenario was INCONCLUSIVE (its fault did not fire, landed outside its window
# or fired after its condition stopped holding), which is not a pass.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
what="${1:-all}"
arg="${2:-1}"
cd "$root"

# shellcheck source=deploy/kind/lib.sh
. "$here/lib.sh"
# shellcheck source=deploy/kind/test-env.sh
. "$here/test-env.sh"

case "$what" in
  mid-drain|mid-feed) kinds=("$what") ;;
  all) kinds=(mid-drain mid-feed) ;;
  replay)
    [ -f "$arg" ] || die "just chaos replay <scenario.json>: '$arg' does not exist"
    replay_file="$(cd "$(dirname "$arg")" && pwd)/$(basename "$arg")"
    ;;
  *) die "unknown scenario '$what' (expected mid-drain, mid-feed, all or replay <file>)" ;;
esac
if [ "$what" != replay ]; then
  case "$arg" in ''|*[!0-9]*) die "seed must be a non-negative integer, got '$arg'" ;; esac
fi

out_root="${SCARAB_CHAOS_DIR:-$here/.chaos}"
mkdir -p "$out_root"
out_root="$(cd "$out_root" && pwd)"

prev="$(cat "$here/.state/mode" 2>/dev/null || echo dev)"
case "$prev" in
  dev|dogfood) back="$prev" ;;
  *) back=dev ;;
esac

stats_pid=""
rc=0
# Set once `up.sh chaos` has switched the release. Until then the release is
# still in the previous mode, and its step Pods are not this recipe's to delete:
# when `up.sh chaos` refuses because they run, or the build fails, the cleanup
# only stops the port-forwards.
switched=0
checker_env=0   # set once the SCARAB_CHAOS_* connections are exported
cleanup() {
  local code=$? mode_now
  [ -z "$stats_pid" ] || kill "$stats_pid" 2>/dev/null || true
  [ "$rc" -ne 0 ] || rc=$code
  mode_now="$(cat "$here/.state/mode" 2>/dev/null || true)"
  if [ "$switched" != 1 ] && [ "$mode_now" != chaos ]; then
    stop_forwards
    echo "==> the release was not switched to chaos mode; leaving it in ${mode_now:-its current mode}"
    exit "$rc"
  fi
  # A Run the quiesce bound gave up on stays non-terminal in scarab_chaos and
  # would resume beside the next session's workload. Cancel it through the
  # API while the chaos release still runs (the next `just chaos` refuses to
  # start while one is left).
  if [ "$checker_env" = 1 ] && [ -x "$bin" ]; then
    echo "==> cancelling non-terminal chaos Runs"
    "$bin" cancel-leftovers || echo "warning: could not cancel every chaos Run; the next 'just chaos' refuses until they end" >&2
  fi
  stop_forwards
  # A Run the bound gave up on can leave its step Pods running; they belong to
  # the chaos mode and would block the switch back. The checker has already
  # recorded them (I8). A deleted Pod still counts in up.sh's guard while it
  # terminates, so wait (bounded) until none is left.
  ( kc delete pods -n "$ns" -l scarab.io/run --field-selector=status.phase!=Succeeded,status.phase!=Failed \
      --ignore-not-found --wait=false >/dev/null 2>&1 ) || true
  local left=""
  for _ in $(seq 1 60); do
    left="$( (active_step_pods) 2>/dev/null || echo '?')"
    [ "$left" = 0 ] && break
    sleep 2
  done
  [ "$left" = 0 ] || echo "warning: $left step Pod(s) still terminating after 120 s; the switch back may refuse" >&2
  echo "==> switching the release back to $back"
  if bash "$here/up.sh" "$back"; then
    scarab_addr
    echo "==> just demo on $back"
    SCARAB_BASE="$base" SCARAB_DEMO_IMAGE="${SCARAB_DEMO_IMAGE:-scarab-clone:dev}" bash "$here/demo.sh" \
      || { echo "error: just demo failed after switching back to $back" >&2; [ "$rc" = 1 ] || rc=2; }
  else
    echo "error: could not switch back to $back" >&2
    [ "$rc" = 1 ] || rc=2
  fi
  exit "$rc"
}
trap cleanup EXIT

cargo build -p scarab-chaos
target="${CARGO_TARGET_DIR:-target}"; case "$target" in /*) ;; *) target="$root/$target" ;; esac
bin="$target/debug/scarab-chaos"

bash "$here/up.sh" chaos
switched=1
scarab_addr

# The checker reads Postgres and MinIO directly, not through toxiproxy: it is
# not under test.
forward scarab-postgres 5432; pg_port="$fwd_port"
forward scarab-minio 9000; s3_port="$fwd_port"
export SCARAB_CHAOS_API="$base"
export SCARAB_CHAOS_KUBECONFIG="$kubeconfig"
export SCARAB_CHAOS_NAMESPACE="$ns"
export SCARAB_CHAOS_RELEASE="$release"
export SCARAB_CHAOS_DATABASE_URL="postgres://scarab:scarab@127.0.0.1:$pg_port/scarab_chaos"
export SCARAB_CHAOS_S3_ENDPOINT="http://127.0.0.1:$s3_port"
export SCARAB_CHAOS_S3_BUCKET=scarab-chaos
export SCARAB_CHAOS_S3_ACCESS_KEY=scarab
export SCARAB_CHAOS_S3_SECRET_KEY=scarabsecret
checker_env=1

server_replicas="$(kc get deploy "$release" -n "$ns" -o jsonpath='{.spec.replicas}')"
depot_replicas="$(kc get statefulset "$release-workspace" -n "$ns" -o jsonpath='{.spec.replicas}')"

# Memory of the kind node (the whole stack plus step Pods) every 5 s, and once
# before any workload runs (the stack alone).
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
mem_log="$out_root/memory-$stamp.log"
docker stats --no-stream --format '{{.MemUsage}}' "$node" | sed 's/^/idle-before-workload /' > "$mem_log"
( while true; do
    docker stats --no-stream --format '{{.MemUsage}}' "$node" 2>/dev/null | sed "s/^/$(date -u +%H:%M:%S) /"
    sleep 5
  done >> "$mem_log" ) &
stats_pid=$!

run_one() {
  local code=0
  "$bin" "$@" || code=$?
  # Worst first: 2 (harness), 1 (a finding), 3 (inconclusive), 0.
  case "$code" in
    0) ;;
    1) [ "$rc" -eq 2 ] || rc=1 ;;
    3) [ "$rc" -ne 0 ] || rc=3 ;;
    *) rc=2 ;;
  esac
}
if [ "$what" = replay ]; then
  echo "==> scarab-chaos replay $replay_file"
  run_one replay "$replay_file" --out-root "$out_root"
else
  for k in "${kinds[@]}"; do
    echo "==> scarab-chaos $k (seed $arg, $server_replicas servers, $depot_replicas Depot replicas)"
    run_one run --kind "$k" --seed "$arg" --out-root "$out_root" \
      --server-replicas "$server_replicas" --depot-replicas "$depot_replicas"
  done
fi

kill "$stats_pid" 2>/dev/null || true; stats_pid=""
peak="$(awk '{v=$2+0; if ($2 ~ /GiB$/) v*=1024; if (v>m) m=v} END {printf "%.0f MiB", m}' "$mem_log")"
echo "==> kind node memory: $(head -n 1 "$mem_log"); peak during the scenarios $peak ($mem_log)"
