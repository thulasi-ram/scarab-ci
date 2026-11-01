#!/usr/bin/env bash
# `just test chaos [<scenario>|<rank>|all] [seed]` and `just test chaos replay <file>`:
# ADR-0077 ranks 1, 2 and 4 on the kind release (Amendment 2026-11-01 §4).
#
# Scenarios (each runs three chained Runs, then the invariant checker):
#   mid-drain     rank 1: delete a Depot Pod (grace 0) while a producer drains
#   mid-feed      rank 1: the same while a consumer's fetcher pulls its inputs
#   two-replicas  rank 2: no fault, two server replicas; counts only when both
#                 replicas claimed some of the launches
#   kill-server   rank 2: SIGKILL the server replica holding the producer's
#                 launch claim while its drain is in flight
#   stop-leader   rank 4: SIGSTOP the scheduler leader until a peer takes the
#                 lease over (past the 30 s TTL), then SIGCONT it
#   pause-node    git-bug 070d042: `docker pause` the whole kind node while a
#                 producer's drain is owed, past the sidecar reporting ceiling
#                 (the Pod's grace + 300 s + 30 s: 930 s for a workspace Pod),
#                 then `docker unpause` it; no Attempt may end "workspace
#                 snapshot lost". About 20 minutes, so not in `all`.
# Groups: rank1 (mid-drain mid-feed), rank2 (two-replicas kill-server),
# rank4 (stop-leader), all (the five ranked scenarios, in that order;
# pause-node only by name). The server signals go from the kind node's PID
# namespace (`docker exec <node> kill`), because the server is PID 1 in its
# container; see crates/scarab-chaos/src/node.rs.
#
#   1. up.sh chaos: refused while step Pods of the current mode run; deletes
#      the Depot's PVCs (a warm cache of another bucket would hide faults);
#      database `scarab_chaos` and bucket `scarab-chaos` on the same Postgres
#      and MinIO, reached through toxiproxy; 2 servers, 2 Depot replicas,
#      plugins off.
#   2. scarab-chaos (crates/scarab-chaos) runs each scenario: three chained
#      Runs, the scenario's fault, then the invariant checker. Each run writes
#      a directory under $SCARAB_CHAOS_DIR (default deploy/kind/.chaos):
#      scenario.json (the replayable file), pods.jsonl, verdict.json,
#      release-log.txt, cache-log.txt and, for mid-feed, fetcher-log.txt. The
#      kind node's memory samples go to $SCARAB_CHAOS_DIR/memory-<time>.log.
#      Each scenario also runs the Cache probe (crates/scarab-chaos/src/cache.rs):
#      a cached Step whose second Run restores the first Run's Cache from its
#      home Depot replica. A Cache needs a Project, so the probe's pipeline
#      lives in the in-cluster Forgejo repo scarab-admin/chaos-cache, bound
#      (no webhook) through a connection in the chaos database and dispatched
#      through the API. Its hits, misses and restores from a home replica are
#      printed with the verdict; a miss never fails it.
#   3. unpauses the kind node if it is paused (an interrupted pause-node run
#      must not leave it paused; the harness unpauses on its own paths too),
#      sends SIGCONT to every server container on the node (an interrupted
#      stop-leader run must not leave a server stopped), cancels the chaos
#      database's non-terminal Runs through the API (a scenario refuses to
#      start while one is left), then switches the release back to the mode
#      that was active before (dev or dogfood; e2e and anything else go back
#      to dev, because restoring e2e would open a new public Forgejo tunnel)
#      and runs `just stack demo` there.
#
# Exit code: 0 when every verdict passed; 1 when a verdict failed (a finding);
# 2 when the harness could not run a scenario (including a SIGSTOP that did not
# stop the leader); 3 when no verdict failed but a scenario was INCONCLUSIVE
# (its fault did not fire, landed outside its window, fired after its
# condition stopped holding, or hit a replica other than the one it names),
# which is not a pass.
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
  mid-drain|mid-feed|two-replicas|kill-server|stop-leader|pause-node) kinds=("$what") ;;
  rank1) kinds=(mid-drain mid-feed) ;;
  rank2) kinds=(two-replicas kill-server) ;;
  rank4) kinds=(stop-leader) ;;
  all) kinds=(mid-drain mid-feed two-replicas kill-server stop-leader) ;;
  replay)
    [ -f "$arg" ] || die "just test chaos replay <scenario.json>: '$arg' does not exist"
    replay_file="$(cd "$(dirname "$arg")" && pwd)/$(basename "$arg")"
    ;;
  *) die "unknown scenario '$what' (expected mid-drain, mid-feed, two-replicas, kill-server, stop-leader, pause-node, rank1, rank2, rank4, all or replay <file>)" ;;
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
# SIGCONT to every scarab-server container's process on the node. The harness
# resumes a server it stopped on every path it controls; this covers the ones
# it does not (an interrupt). SIGCONT to a running process does nothing.
resume_servers() {
  docker exec "$node" sh -c '
    for id in $(crictl ps -q --name "^scarab-server$"); do
      pid=$(crictl inspect --output go-template --template "{{.info.pid}}" "$id" || true)
      if [ -n "$pid" ] && [ "$pid" != 0 ]; then kill -CONT "$pid" || true; fi
    done' || echo "warning: could not send SIGCONT to the server containers on $node" >&2
}
# `docker unpause` the kind node when it is paused. pause-node unpauses it on
# every path it controls (a Drop guard, SIGINT and SIGTERM); this covers a
# harness that was SIGKILLed, and runs first on exit, before anything that
# needs the node.
unpause_node() {
  if [ "$(docker inspect --format '{{.State.Paused}}' "$node" 2>/dev/null || true)" = true ]; then
    echo "==> the kind node $node is paused; unpausing it"
    docker unpause "$node" >/dev/null || echo "error: could not unpause $node; run: docker unpause $node" >&2
  fi
}
# Like test-env.sh's `forward`, but kept open: a loop starts the port-forward
# again on the same local port whenever it exits, and writes its current PID
# to a file. A paused node breaks a port-forward; pause-node kills each one
# after the unpause (SCARAB_CHAOS_FORWARD_PIDFILES) and the loop re-opens it.
# stop_forwards ends the loop, which ends its port-forward.
forward_pidfiles=""
forward_kept() { # $1 Service name, $2 its port
  local log pidfile
  fwd_port="$(free_port)"
  log="$kind_dir/.state/forward-$1.log"
  pidfile="$kind_dir/.state/forward-$1.pid"
  : >"$log"
  (
    child=""
    trap '[ -z "$child" ] || kill "$child" 2>/dev/null; exit 0' TERM INT
    while true; do
      kc port-forward -n "$ns" "svc/$1" "$fwd_port:$2" --address 127.0.0.1 >>"$log" 2>&1 &
      child=$!
      echo "$child" >"$pidfile"
      wait "$child" || true
      sleep 1
    done
  ) &
  fwd_pids="$fwd_pids $!"
  forward_pidfiles="${forward_pidfiles:+$forward_pidfiles:}$pidfile"
  for _ in $(seq 1 30); do
    (exec 3<>"/dev/tcp/127.0.0.1/$fwd_port") 2>/dev/null && return 0
    sleep 0.5
  done
  cat "$log" >&2
  die "the port-forward to svc/$1:$2 did not open 127.0.0.1:$fwd_port"
}
cleanup() {
  local code=$? mode_now
  unpause_node
  [ -z "$stats_pid" ] || kill "$stats_pid" 2>/dev/null || true
  [ "$rc" -ne 0 ] || rc=$code
  mode_now="$(cat "$here/.state/mode" 2>/dev/null || true)"
  if [ "$switched" != 1 ] && [ "$mode_now" != chaos ]; then
    stop_forwards
    echo "==> the release was not switched to chaos mode; leaving it in ${mode_now:-its current mode}"
    exit "$rc"
  fi
  resume_servers
  # A Run the quiesce bound gave up on stays non-terminal in scarab_chaos and
  # would resume beside the next session's workload. Cancel it through the
  # API while the chaos release still runs (the next `just test chaos` refuses to
  # start while one is left).
  if [ "$checker_env" = 1 ] && [ -x "$bin" ]; then
    echo "==> cancelling non-terminal chaos Runs"
    "$bin" cancel-leftovers || echo "warning: could not cancel every chaos Run; the next 'just test chaos' refuses until they end" >&2
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
    echo "==> just stack demo on $back"
    SCARAB_BASE="$base" SCARAB_DEMO_IMAGE="${SCARAB_DEMO_IMAGE:-scarab-clone:dev}" bash "$here/demo.sh" \
      || { echo "error: just stack demo failed after switching back to $back" >&2; [ "$rc" = 1 ] || rc=2; }
  else
    echo "error: could not switch back to $back" >&2
    [ "$rc" = 1 ] || rc=2
  fi
  exit "$rc"
}
trap cleanup EXIT
# A node an earlier, killed run left paused would hang everything below.
unpause_node

cargo build -p scarab-chaos
bin="$(cargo_target_dir)/debug/scarab-chaos"

bash "$here/up.sh" chaos
switched=1
scarab_addr

# The checker reads Postgres and MinIO directly, not through toxiproxy: it is
# not under test.
forward_kept scarab-postgres 5432; pg_port="$fwd_port"
forward_kept scarab-minio 9000; s3_port="$fwd_port"
export SCARAB_CHAOS_API="$base"
export SCARAB_CHAOS_KUBECONFIG="$kubeconfig"
export SCARAB_CHAOS_NAMESPACE="$ns"
export SCARAB_CHAOS_RELEASE="$release"
export SCARAB_CHAOS_DATABASE_URL="postgres://scarab:scarab@127.0.0.1:$pg_port/scarab_chaos"
export SCARAB_CHAOS_S3_ENDPOINT="http://127.0.0.1:$s3_port"
export SCARAB_CHAOS_S3_BUCKET=scarab-chaos
export SCARAB_CHAOS_S3_ACCESS_KEY=scarab
export SCARAB_CHAOS_S3_SECRET_KEY=scarabsecret
export SCARAB_CHAOS_NODE="$node"
# The Cache probe's Forgejo (up.sh seeded it and wrote forgejo.env, with a
# token minted for this run). Without it the probe is skipped with a note.
forgejo_env="$here/.state/forgejo.env"
if [ -f "$forgejo_env" ]; then
  forward_kept scarab-forgejo 3000
  export SCARAB_CHAOS_FORGEJO_URL="http://127.0.0.1:$fwd_port"
  SCARAB_CHAOS_FORGEJO_SERVER_URL="$(env_get "$forgejo_env" SCARAB_TEST_FORGEJO_SERVER_URL)"
  SCARAB_CHAOS_FORGEJO_TOKEN="$(env_get "$forgejo_env" SCARAB_TEST_FORGEJO_TOKEN)"
  SCARAB_CHAOS_FORGEJO_OWNER="$(env_get "$forgejo_env" SCARAB_TEST_FORGEJO_OWNER)"
  export SCARAB_CHAOS_FORGEJO_SERVER_URL SCARAB_CHAOS_FORGEJO_TOKEN SCARAB_CHAOS_FORGEJO_OWNER
else
  echo "warning: $forgejo_env is missing; the Cache probe is skipped" >&2
fi
export SCARAB_CHAOS_FORWARD_PIDFILES="$forward_pidfiles"
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
