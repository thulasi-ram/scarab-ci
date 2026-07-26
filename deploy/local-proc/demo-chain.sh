#!/usr/bin/env bash
# Submit a CHAINED pipeline (produce → consume) and watch it complete.
#
# Why this exists next to demo.sh: `just demo` submits ONE step with no `needs:`,
# which never crosses a Step boundary — so it exercises the workspace **drain**
# and nothing at all of the **feed**. Since ADR-0061 s3-feed the feed is a
# Scarab-owned init container (`scarab-wsfetch`) that dials the workspace service
# and materialises the upstream snapshot itself, and the only way to exercise that
# from the outside is a step that inherits a workspace.
#
# `consume` asserts on the CONTENT it inherited, not merely on its own exit code:
# a step that ran against a silently empty /workspace must fail this, because an
# empty workspace is not an error — it is a wrong answer.
#
# Requires `just up` (or `just serve`) to be running first.
set -euo pipefail

base="${SCARAB_BASE:-http://127.0.0.1:8080}"

# The image MUST default to a non-root USER — see the long note in demo.sh: the
# ADR-0039 hardened baseline sets `runAsNonRoot: true`, so a root-default image
# never starts. `scarab-clone` is ours, is USER 65532, and has a shell.
image="${SCARAB_DEMO_IMAGE:-ghcr.io/thulasi-ram/scarab-clone:edge}"

echo "==> POST $base/v1/runs (chained: produce → consume; image: $image)"
id="$(curl -sf -X POST "$base/v1/runs" \
  -H 'content-type: application/json' \
  -d "{
        \"pipeline\": {
          \"ir_version\": 1,
          \"steps\": [
            { \"id\": \"produce\", \"image\": \"$image\",
              \"command\": [\"sh\", \"-c\",
                \"mkdir -p /workspace/dist && echo scarab-crossed-the-boundary > /workspace/dist/evidence.txt && ls -la /workspace/dist\"] },
            { \"id\": \"consume\", \"image\": \"$image\", \"needs\": [\"produce\"],
              \"command\": [\"sh\", \"-c\",
                \"echo '--- what the fetcher provisioned:'; ls -la /workspace /workspace/dist; grep -q scarab-crossed-the-boundary /workspace/dist/evidence.txt && echo FEED-LEG-OK\"] }
          ]
        }
      }" \
  | python3 -c 'import sys, json; print(json.load(sys.stdin)["id"])')"
echo "==> created run $id"

echo "==> polling status"
for _ in $(seq 1 90); do
  status="$(curl -sf "$base/v1/runs/$id" \
    | python3 -c 'import sys, json; print(json.load(sys.stdin)["status"])')"
  echo "    status: $status"
  case "$status" in
    succeeded)
      echo "==> ✅ run $id succeeded — the workspace crossed a Step boundary"
      echo "==> logs:"
      curl -sf "$base/v1/runs/$id/logs" || true
      echo
      echo "==> the fetcher's own line (ADR-0061 s3-feed stepping stone):"
      curl -sf "$base/v1/runs/$id/logs" | grep -i "mode=eager" || \
        echo "    (not in the step log stream — the init container's log is separate; \
try: kubectl logs -n scarab <pod> -c scarab-workspace-init)"
      exit 0
      ;;
    failed | dead_lettered | cancelled)
      echo "==> ❌ run $id ended: $status" >&2
      echo "==> logs:" >&2
      curl -sf "$base/v1/runs/$id/logs" >&2 || true
      exit 1
      ;;
  esac
  sleep 2
done

echo "error: run $id did not complete in time" >&2
exit 1
