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

# NFS on kind, for whoever wires ADR-0062 stage 2b here (probed 2026-08-02):
# the default kindest/node image ALREADY ships /sbin/mount.nfs, mount.nfs4 and
# rpc.statd — no install, no custom node image, no kind.yaml change. The catch is
# that there is no systemd in a node container to RUN rpc.statd, so a mount that
# defaults to v3 dies with "rpc.statd is not running" while `-o vers=4` (and an
# unqualified mount, which negotiates 4.2) both work. Any PV must therefore pin
# `mountOptions: [nfsvers=4.2]`, or `nolock` if it must stay on v3.
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

# ---------------------------------------------------------------------------
# The workspace service (ADR-0061) — in the standard path, not optional.
#
# THE ROUTING PROBLEM, and how it is actually solved. In proc mode
# `scarab-server` is a HOST process while Steps run as Pods inside kind, so the
# workspace service has two audiences with different views of the network. The
# host reaches it over loopback. A Pod cannot — `localhost` inside a Pod is the
# Pod — and `kind.yaml` provides no route, no `extraPortMappings`, no
# `extraMounts`. (`SCARAB_RESULTS_API_URL` has exactly this problem and is simply
# unset here, which is why results egress is off in proc mode.)
#
# The obvious answer is WRONG on this platform, and it fails silently, so it is
# worth writing down: "use the kind docker network's gateway" assumes the gateway
# is a host interface. On Linux it is. On darwin, docker runs inside a VM
# (colima/lima), so the bridge gateway `172.19.0.1` is an interface *of the VM* —
# nothing is listening there, and a Pod gets `connection refused` against a URL
# that looks entirely plausible. Measured, not reasoned about: from a real Pod,
# `172.19.0.1` refuses, `host.docker.internal` answers.
#
# So the URL is DISCOVERED, not derived: candidates are probed from an actual Pod
# and the first one that answers /readyz wins. That is a few seconds once per
# `just up`, it is correct on both Linux and darwin without a platform branch, and
# it cannot report success against an address nothing can reach.
#
# NOTE on the two audiences: SCARAB_WORKSPACE_URL is the POD-facing URL. In proc
# mode it is generally NOT resolvable from the host (`host.docker.internal` means
# nothing on macOS), so anything on the host that needs the service uses
# 127.0.0.1:$ws_port. In Helm mode there is one in-cluster Service and the
# question does not arise.
#
# If the discovery ever finds nothing, the fallback is a Deployment + PVC inside
# kind (as deploy/local-helm does for Postgres and MinIO), which costs a
# `docker build` in every `just up`.
ws_port="${SCARAB_WORKSPACE_PORT:-8081}"
# A dev default so a .env predating ADR-0061 still brings the stack up. It is
# EXPORTED, so the control plane (which mints tokens) and the workspace service
# (which verifies them) necessarily share it — a mismatch would give every Step a
# 401 that looked like the service being down. Production supplies it from the
# chart Secret; there is no default there, and `--role workspace` refuses to boot
# without one.
export SCARAB_WORKSPACE_TOKEN_SECRET="${SCARAB_WORKSPACE_TOKEN_SECRET:-dev-workspace-token-secret}"
export SCARAB_WORKSPACE_DATA_DIR="${SCARAB_WORKSPACE_DATA_DIR:-$root/.scarab/workspace-cas}"
mkdir -p "$SCARAB_WORKSPACE_DATA_DIR"

# Candidate hosts, best-first. `host.docker.internal` is Docker Desktop's and
# colima's name for the host and is what works on darwin; the IPv4 gateway of the
# `kind` network is what works on Linux. RANGE over IPAM.Config rather than
# `index .IPAM.Config 0` — with docker IPv6 enabled, entry 0 is the IPv6 subnet,
# which would also need brackets in a URL.
ws_candidates="host.docker.internal"
for gw in $(docker network inspect kind -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}' 2>/dev/null || true); do
  case "$gw" in
    *:*|'') continue ;;                  # IPv6 / empty — skip
    *) ws_candidates="$ws_candidates $gw" ;;
  esac
done
ws_candidates="$ws_candidates host.lima.internal"

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
reap_pidfile() {
  # $1 = pidfile. Same semantics as the block that used to be inline here: this
  # script OWNS the process the pidfile names, so re-running REPLACES it.
  [ -f "$1" ] || return 0
  local old
  old="$(cat "$1" 2>/dev/null || true)"
  if [ -n "$old" ] && proc_alive "$old" && is_scarab_server "$old"; then
    echo "    reaping the scarab-server left by an earlier up.sh (pid $old, $(basename "$1"))"
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
  rm -f "$1"
}
reap_pidfile "$here/server.pid"
# The workspace service is a second process of the same binary (ADR-0061), so it
# needs the same reaping — an orphan holding :8081 would silently serve the next
# run's Steps from a stale warm tier.
reap_pidfile "$here/workspace.pid"
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

# The workspace service (ADR-0061) comes up FIRST: the control plane's startup
# report names it, and once the fetcher lands a Step cannot be provisioned without
# it. Same binary, different role — one image, no server↔service skew.
echo "==> starting the workspace service (--role workspace) on 0.0.0.0:$ws_port"
if port_listening "127.0.0.1" "$ws_port"; then
  {
    echo "error: something is already listening on 127.0.0.1:$ws_port — refusing to start"
    echo "  the workspace service. Find it with:  lsof -nP -iTCP:$ws_port -sTCP:LISTEN"
    echo "  Then stop it, run 'just down', or set SCARAB_WORKSPACE_PORT in"
    echo "  deploy/local-proc/.env."
  } >&2
  exit 1
fi
# `env -u`/explicit overrides rather than editing .env: the workspace role must
# bind 0.0.0.0 on its own port while the control plane keeps SCARAB_ADDR from the
# env file. SCARAB_DATABASE_URL flows through from the same env — since
# ADR-0067 part 2 the Depot keeps its fence rows (drain records, write
# ledgers) in the control plane's Postgres: connects yes, migrates never.
SCARAB_ADDR="0.0.0.0:$ws_port" \
nohup "$root/target/debug/scarab-server" --role workspace \
  > "$here/workspace.log" 2>&1 &
workspace_pid=$!
echo "$workspace_pid" > "$here/workspace.pid"

ws_ok=0
for _ in $(seq 1 30); do
  if ! proc_alive "$workspace_pid"; then break; fi
  if curl -sf "http://127.0.0.1:$ws_port/readyz" >/dev/null 2>&1; then ws_ok=1; break; fi
  sleep 1
done
if [ "$ws_ok" != 1 ]; then
  echo "error: the workspace service never answered /readyz on 127.0.0.1:$ws_port" >&2
  echo "---- tail of deploy/local-proc/workspace.log ----" >&2
  tail -n 40 "$here/workspace.log" >&2 || true
  rm -f "$here/workspace.pid"
  exit 1
fi
echo "    workspace service ready (warm tier: $SCARAB_WORKSPACE_DATA_DIR, logs: deploy/local-proc/workspace.log)"

# Discover the Pod-facing URL by ASKING A POD (see the long note above). One
# throwaway busybox Pod tries every candidate and prints the winner; busybox is
# used because the kind node image ships neither wget nor curl, which is how an
# earlier version of this check managed to report FAIL for addresses that in fact
# worked.
echo "==> discovering how a Pod reaches the workspace service"
ws_probe_script="for h in $ws_candidates; do if wget -q -T 4 -O /dev/null \"http://\$h:$ws_port/readyz\"; then echo \"SCARAB_WS_HOST=\$h\"; exit 0; fi; done; echo SCARAB_WS_HOST=none"
ws_probe_pod="ws-discover-$$"
# Create → wait → `kubectl logs` → delete, rather than `kubectl run --rm --attach`.
# The `--attach` form is what this used to do and it is NOT reliable here: it
# returned the winning host once and then returned NOTHING on every subsequent
# invocation on the same cluster, while a plain `kubectl logs` on the very same
# probe script printed `SCARAB_WS_HOST=host.docker.internal` every time. That
# mattered the moment this check became a hard gate (below): a probe that silently
# produces no output would fail `just up` on a stack whose network is perfectly
# fine, which is the kind of false red that gets a gate deleted.
kubectl delete pod "$ws_probe_pod" -n scarab --ignore-not-found --wait=false >/dev/null 2>&1 || true
kubectl run "$ws_probe_pod" -n scarab \
  --image=busybox:1.36 --restart=Never \
  --command -- sh -c "$ws_probe_script" >/dev/null 2>&1 || true
ws_host=""
for _ in $(seq 1 30); do
  phase="$(kubectl get pod "$ws_probe_pod" -n scarab -o jsonpath='{.status.phase}' 2>/dev/null || true)"
  case "$phase" in
    Succeeded|Failed) break ;;
  esac
  sleep 2
done
ws_host="$(kubectl logs "$ws_probe_pod" -n scarab 2>/dev/null \
           | sed -n 's/^SCARAB_WS_HOST=//p' | tr -d '\r' | head -n 1)"
kubectl delete pod "$ws_probe_pod" -n scarab --ignore-not-found --wait=false >/dev/null 2>&1 || true
if [ -z "$ws_host" ] || [ "$ws_host" = none ]; then
  # A HARD GATE, not a warning — and this is the promotion the previous version's
  # own comment promised "once the fetcher lands" (ADR-0061 D2.3 guard #3).
  #
  # It landed. Every Step with `needs:` is now provisioned by an init container
  # that dials this URL, and there is no control-plane feed to fall back on. A
  # stack that comes up "usable" without a Pod-reachable workspace service is a
  # stack where every chained pipeline fails at its second step — which is a
  # confusing red run twenty minutes later instead of one clear error here.
  {
    echo "error: no Pod-reachable address for the workspace service was found."
    echo "  Tried: $ws_candidates (port $ws_port)"
    echo "  Since ADR-0061 s3-feed this is FATAL: a Step that inherits a workspace"
    echo "  is provisioned by an init container that dials this URL, and the old"
    echo "  control-plane 'kubectl exec' feed is deleted, not kept as a fallback."
    echo "  So a stack without it cannot run any pipeline with more than one step."
    echo "  Next option: run the workspace service as a Deployment + PVC inside"
    echo "  kind, as deploy/local-helm does for Postgres and MinIO."
  } >&2
  rm -f "$here/workspace.pid"
  kill "$workspace_pid" 2>/dev/null || true
  exit 1
fi
echo "    a Pod reached the workspace service at http://$ws_host:$ws_port"
export SCARAB_WORKSPACE_URL="http://$ws_host:$ws_port"

# The workspace helper image (ADR-0061): the s3-feed fetcher init container AND
# the egress hold/drain sidecar. kind cannot pull from the host's docker daemon,
# so proc mode builds it and `kind load`s it — the same reason the clone image
# has to be dealt with explicitly here.
#
# ALWAYS rebuilt (same as `just kube-tests` does for the clone/sidecar images):
# docker's layer cache makes an unchanged rebuild a few seconds, and the old
# "skip when the image exists" shortcut served a STALE helper whenever
# crates/scarab-workspace-client changed — an old binary has no subcommands, so
# every drain "succeeded" (exit 0) with no record and the Attempt failed as a
# Config skew error the dev loop itself had manufactured.
#
# (The old DELETE-ME note is gone with git-bug 0628369, closed as superseded:
# ADR-0062 replaces only the eager-fetch role; the egress role survives it.)
export SCARAB_WSFETCH_IMAGE="${SCARAB_WSFETCH_IMAGE:-scarab-wsfetch:dev}"
if [ "${SCARAB_WSFETCH_SKIP_BUILD:-0}" != 1 ]; then
  echo "==> building + loading the workspace fetcher image ($SCARAB_WSFETCH_IMAGE)"
  docker build -q -t "$SCARAB_WSFETCH_IMAGE" -f "$root/docker/wsfetch/Dockerfile" "$root" >/dev/null
  kind load docker-image "$SCARAB_WSFETCH_IMAGE" --name "$cluster" >/dev/null
fi

# ---------------------------------------------------------------------------
# What this run DISCOVERED, written down for other processes.
#
# Everything above is knowledge that exists only inside this shell: which port the
# workspace service bound, which host name a Pod can actually reach it at (probed,
# not derived), which fetcher image is in the node. A later `cargo nextest` is a
# different process and cannot inherit exported vars, so the live k8s tier
# (`crates/scarab-executor-k8s/tests/cluster.rs`) had no way to be configured — and
# it responded by skipping seven cases while reporting PASS.
#
# Same contract deploy/local-forgejo/up.sh already uses: a `.env.generated` the
# consumer sources. `.env.*` is gitignored. `just kube-tests` is the consumer.
generated="$here/.env.generated"
cat > "$generated" <<EOF
# Generated by deploy/local-proc/up.sh — do not edit, do not commit.
# The live k8s executor tier's contract (see \`just kube-tests\`).
#
# HOST-facing: this stack's control plane is a host process, and so is a test
# process standing in for one.
SCARAB_TEST_WORKSPACE_URL=http://127.0.0.1:$ws_port
# POD-facing: what a Step Pod's fetcher dials. NOT the same address, and on darwin
# not even resolvable from the host — it was probed from a real Pod above.
SCARAB_TEST_WORKSPACE_POD_URL=$SCARAB_WORKSPACE_URL
SCARAB_TEST_WORKSPACE_SECRET=$SCARAB_WORKSPACE_TOKEN_SECRET
SCARAB_TEST_WSFETCH_IMAGE=$SCARAB_WSFETCH_IMAGE
# A host address a Pod can reach — the results-sidecar case POSTs to a listener
# in the test process. It is the same discovered host as the workspace URL, so it
# is reused rather than guessed a second time.
SCARAB_TEST_HOST_IP=$ws_host
SCARAB_TEST_KUBE_NS=${SCARAB_NAMESPACE:-scarab}
EOF
echo "    live-tier env written to deploy/local-proc/.env.generated ('just kube-tests' reads it)"

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
    echo "    workspace service: $SCARAB_WORKSPACE_URL from a Pod, http://127.0.0.1:$ws_port from the host"
    echo "    run 'just demo' to submit a pipeline and watch it complete."
    exit 0
  fi
  sleep 1
done
fail_with_log "scarab-server (pid $server_pid) is alive but did not answer $base/healthz within 30s"
