# Scarab dev harness. One-command local environment for the slice-1 skeleton.
#
#   just up      # Postgres + MinIO + kind + scarab-server (background)
#   just demo    # submit a pipeline and watch it complete
#   just down    # tear it all down
#
# Requires: docker, kind, kubectl, cargo (and python3 for the demo).

# List recipes by default.
default:
    @just --list

# Run the web UI dev server (Vite on http://localhost:5173, proxies /v1 → server).
# Env (incl. SCARAB_API_URL, which vite refuses to guess) comes from the one dev
# env file — not wired inline here. Point elsewhere by editing that file.
ui:
    #!/usr/bin/env bash
    set -euo pipefail
    npm --prefix ui/scarab-web-ui install
    [ -f deploy/local-proc/.env ] || { echo "==> creating deploy/local-proc/.env from .env.example"; cp deploy/local-proc/.env.example deploy/local-proc/.env; }
    set -a && source deploy/local-proc/.env && set +a
    npm --prefix ui/scarab-web-ui run dev

# Run the web UI against a built-in fixture (no server, no DB) — the fastest way
# to eyeball UI changes. Serves a fixed "acme" org (dashboard/run-detail/env/
# secrets) with the dark theme forced. See ui/scarab-web-ui/src/mock.ts.
ui-mock:
    npm --prefix ui/scarab-web-ui install
    VITE_SCARAB_MOCK=1 npm --prefix ui/scarab-web-ui run dev

# Run the docs site dev server (Astro Starlight; ADR-0040).
docs:
    npm --prefix ui/scarab-docs-ui install
    npm --prefix ui/scarab-docs-ui run dev

# Bring up the full dev stack and start scarab-server against it.
up:
    bash deploy/local-proc/up.sh

# Submit a one-step pipeline to the running server and watch it complete.
demo:
    bash deploy/local-proc/demo.sh

# Tear down the whole stack.
down:
    bash deploy/local-proc/down.sh

# Tail the background server log.
logs:
    tail -f deploy/local-proc/server.log

# Requires deploy/local-helm/.env and kube context `colima`. Usage:
#   just local-helm             # pull + deploy the latest ghcr `edge`
#   just local-helm sha-abc123  # pull + deploy a specific published SHA
#   just local-helm local       # build server+clone+sidecar locally, then deploy

# Deploy the Helm dogfood stack on colima; pulls ghcr by default, `local` builds.
local-helm ref="edge":
    #!/usr/bin/env bash
    set -euo pipefail
    owner=ghcr.io/thulasi-ram
    if [ "{{ref}}" = "local" ]; then
      echo "==> building server + clone + sidecar from the working tree"
      docker build -t scarab-server:dogfood-local -f docker/server/Dockerfile .
      docker build -t scarab-clone:dogfood docker/clone
      docker build -t scarab-results-sidecar:dogfood docker/sidecar
      IMAGE_REPOSITORY=scarab-server \
      SCARAB_CLONE_IMAGE=scarab-clone:dogfood \
      SCARAB_SIDECAR_IMAGE=scarab-results-sidecar:dogfood \
        bash deploy/local-helm/deploy.sh dogfood-local
    else
      echo "==> pulling + deploying published images @ {{ref}} (ghcr, pullPolicy Always)"
      IMAGE_REPOSITORY="$owner/scarab-server" \
      IMAGE_PULL_POLICY=Always \
      SCARAB_CLONE_IMAGE="$owner/scarab-clone:{{ref}}" \
      SCARAB_SIDECAR_IMAGE="$owner/scarab-results-sidecar:{{ref}}" \
        bash deploy/local-helm/deploy.sh {{ref}}
    fi

# Persistent port-forward to the in-cluster server (UI + API) on colima. A plain
# `kubectl port-forward` binds to ONE Pod and dies when it rolls — and every
# `just local-helm` now rolls the server Pod — so this loops against the Service
# and auto-reconnects. Leave it running; Ctrl-C to stop. Usage: `just local-helm-ui [port]`.
local-helm-ui port="8080":
    #!/usr/bin/env bash
    set -euo pipefail
    [ "$(kubectl config current-context)" = "colima" ] || { echo "refusing: context is not 'colima'." >&2; exit 1; }
    echo "==> http://127.0.0.1:{{port}}  (auto-reconnects across deploys; Ctrl-C to stop)"
    until kubectl port-forward -n scarab svc/scarab {{port}}:80; do
      echo "   port-forward dropped (Pod roll?) — reconnecting…" >&2
      sleep 1
    done

# Run scarab-server in the FOREGROUND against the dev stack (Ctrl-C to stop).
# Useful when iterating; `just up` runs it in the background instead. Needs the
# stack up first (`just up`). Env comes from the one dev env file, not inline.
serve:
    #!/usr/bin/env bash
    set -euo pipefail
    [ -f deploy/local-proc/.env ] || { echo "==> creating deploy/local-proc/.env from .env.example"; cp deploy/local-proc/.env.example deploy/local-proc/.env; }
    set -a && source deploy/local-proc/.env && set +a
    export KUBECONFIG=deploy/local-proc/.kubeconfig
    cargo run -p scarab-server -- --role converged --serve

# Run the workspace suite against the compose Postgres (brought up on demand),
# so the PG-backed tests actually run — mirroring the CI `test` job. Uses
# nextest when installed, plain `cargo test` otherwise.
test:
    #!/usr/bin/env bash
    set -euo pipefail
    # Reuse whatever already serves 55432 (a `just up` stack, or an ad-hoc dev
    # container); otherwise bring up just the compose Postgres.
    if (exec 3<>/dev/tcp/127.0.0.1/55432) 2>/dev/null; then
      echo "==> reusing the Postgres already listening on 127.0.0.1:55432"
    else
      docker compose -f deploy/local-proc/compose.yaml up -d --wait postgres
    fi
    export SCARAB_TEST_DATABASE_URL=postgres://scarab:scarab@127.0.0.1:55432/scarab
    if command -v cargo-nextest >/dev/null 2>&1; then
      cargo nextest run --workspace
    else
      echo "warning: cargo-nextest not installed (https://nexte.st) — falling back to cargo test" >&2
      cargo test --workspace
    fi

# Coverage run (mirrors the CI `coverage` job): suite under cargo-llvm-cov
# against the compose Postgres, per-crate summary, and REGENERATES
# docs/audits/coverage-baseline.toml — review + commit the baseline deliberately.
# Requires cargo-nextest + cargo-llvm-cov.
coverage:
    #!/usr/bin/env bash
    set -euo pipefail
    if (exec 3<>/dev/tcp/127.0.0.1/55432) 2>/dev/null; then
      echo "==> reusing the Postgres already listening on 127.0.0.1:55432"
    else
      docker compose -f deploy/local-proc/compose.yaml up -d --wait postgres
    fi
    export SCARAB_TEST_DATABASE_URL=postgres://scarab:scarab@127.0.0.1:55432/scarab
    cargo llvm-cov nextest --workspace \
      --ignore-filename-regex 'crates/scarab-testkit/|/tests/' \
      --lcov --output-path target/lcov.info
    python3 scripts/coverage_ratchet.py target/lcov.info docs/audits/coverage-baseline.toml --write

# Compile-check everything.
check:
    cargo check --workspace
