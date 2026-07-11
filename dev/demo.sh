#!/usr/bin/env bash
# Submit a one-step pipeline to a running scarab-server and watch it complete.
# Requires `just up` (or `just serve`) to be running first.
set -euo pipefail

base="${SCARAB_BASE:-http://127.0.0.1:8080}"

echo "==> POST $base/v1/runs"
id="$(curl -sf -X POST "$base/v1/runs" \
  -H 'content-type: application/json' \
  -d '{
        "pipeline": {
          "ir_version": 1,
          "steps": [
            { "id": "hello", "image": "busybox:latest",
              "command": ["sh", "-c", "echo hello from scarab"] }
          ]
        }
      }' \
  | python3 -c 'import sys, json; print(json.load(sys.stdin)["id"])')"
echo "==> created run $id"

echo "==> polling status"
for _ in $(seq 1 60); do
  status="$(curl -sf "$base/v1/runs/$id" \
    | python3 -c 'import sys, json; print(json.load(sys.stdin)["status"])')"
  echo "    status: $status"
  case "$status" in
    succeeded)
      echo "==> ✅ run $id succeeded"
      echo "==> logs:"
      curl -sf "$base/v1/runs/$id/logs" || true
      exit 0
      ;;
    failed | dead_lettered | cancelled)
      echo "==> ❌ run $id ended: $status" >&2
      exit 1
      ;;
  esac
  sleep 2
done

echo "error: run $id did not complete in time" >&2
exit 1
