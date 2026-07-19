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

# Run scarab-server for local dev. Config comes from the environment — `cd` into
# the repo with direnv set up (see .env.local.example) and this needs no flags.
server:
    cargo run -p scarab-server

# Run the web UI dev server (Vite on http://localhost:5173, proxies /v1 → server).
ui:
    npm --prefix ui/scarab-web-ui install
    npm --prefix ui/scarab-web-ui run dev

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

# Run scarab-server in the FOREGROUND against the dev stack (Ctrl-C to stop).
# Useful when iterating; `just up` runs it in the background instead.
serve:
    SCARAB_DATABASE_URL=postgres://scarab:scarab@127.0.0.1:55432/scarab \
    SCARAB_S3_BUCKET=scarab-logs SCARAB_S3_ENDPOINT=http://127.0.0.1:9000 \
    SCARAB_S3_ACCESS_KEY=scarab SCARAB_S3_SECRET_KEY=scarabsecret SCARAB_S3_REGION=us-east-1 \
    KUBECONFIG=deploy/local-proc/.kubeconfig SCARAB_NAMESPACE=scarab \
    SCARAB_DEV_INSECURE=1 \
    cargo run -p scarab-server -- --role converged --serve --addr 127.0.0.1:8080

# Build + test the workspace. Set SCARAB_TEST_DATABASE_URL to run PG-backed tests.
test:
    cargo test --workspace

# Compile-check everything.
check:
    cargo check --workspace
