# Helpers deploy/kind/e2e.sh (`just e2e`) and deploy/kind/forgejo.sh source
# after deploy/kind/lib.sh. The tests run on the
# host and drive the release in the kind cluster; these give them what the
# host cannot reach on its own.
#
#   forward SVC PORT   kubectl port-forward of svc/SVC:PORT in namespace $ns to
#                      a free loopback port, through kc (the stack's own
#                      kubeconfig); sets fwd_port. stop_forwards ends them all.
#   probe_pod_host     the host name or address a Pod reaches the host at, for
#                      a test that is itself a server (a fake forge, a results
#                      listener); sets pod_host.
# shellcheck shell=bash

fwd_pids=""

free_port() {
  python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

forward() { # $1 Service name, $2 its port
  local log
  fwd_port="$(free_port)"
  log="$kind_dir/.state/forward-$1.log"
  kc port-forward -n "$ns" "svc/$1" "$fwd_port:$2" --address 127.0.0.1 >"$log" 2>&1 &
  fwd_pids="$fwd_pids $!"
  for _ in $(seq 1 30); do
    (exec 3<>"/dev/tcp/127.0.0.1/$fwd_port") 2>/dev/null && return 0
    sleep 0.5
  done
  cat "$log" >&2
  die "the port-forward to svc/$1:$2 did not open 127.0.0.1:$fwd_port"
}

stop_forwards() {
  local p
  for p in $fwd_pids; do kill "$p" 2>/dev/null || true; done
  fwd_pids=""
}

# A Pod cannot reach the host at 127.0.0.1. On Linux the kind network's
# gateway is a host address; on macOS docker runs in a VM, the gateway is the
# VM's, and the host is `host.docker.internal`. Rather than guess per
# platform, a one-shot listener on the host is asked for from a real Pod and
# the first candidate that answers wins. The listener binds 0.0.0.0 for the
# few seconds of the probe; the tests' own listeners do the same.
probe_pod_host() {
  local port pid cands gw pod phase
  port="$(free_port)"
  python3 -I -c '
import http.server, sys
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.end_headers(); self.wfile.write(b"ok")
    def log_message(self, *a):
        pass
http.server.HTTPServer(("0.0.0.0", int(sys.argv[1])), H).serve_forever()
' "$port" &
  pid=$!
  cands="host.docker.internal"
  for gw in $(docker network inspect kind -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}' 2>/dev/null || true); do
    case "$gw" in *:*|'') ;; *) cands="$cands $gw" ;; esac
  done
  cands="$cands host.lima.internal"
  pod="pod-host-probe-$$"
  kc delete pod "$pod" -n "$ns" --ignore-not-found >/dev/null 2>&1 || true
  kc run "$pod" -n "$ns" --image=busybox:1.36 --restart=Never --command -- sh -c \
    "for h in $cands; do if wget -q -T 4 -O /dev/null http://\$h:$port/; then echo POD_HOST=\$h; exit 0; fi; done; echo POD_HOST=none" \
    >/dev/null
  for _ in $(seq 1 60); do
    phase="$(kc get pod "$pod" -n "$ns" -o jsonpath='{.status.phase}' 2>/dev/null || true)"
    case "$phase" in Succeeded|Failed) break ;; esac
    sleep 1
  done
  pod_host="$(kc logs "$pod" -n "$ns" 2>/dev/null | sed -n 's/^POD_HOST=//p' | tr -d '\r' | head -n 1 || true)"
  kc delete pod "$pod" -n "$ns" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  kill "$pid" 2>/dev/null || true
  case "$pod_host" in
    ''|none) die "no host address a Pod can reach was found (tried: $cands; probe Pod phase: ${phase:-none})" ;;
  esac
}
