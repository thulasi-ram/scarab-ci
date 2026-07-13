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

# Run the UI dev server (Vite on http://localhost:5173, proxies /v1 → server).
ui:
    npm --prefix ui install
    npm --prefix ui run dev

# Bring up the full dev stack and start scarab-server against it.
up:
    bash dev/up.sh

# Submit a one-step pipeline to the running server and watch it complete.
demo:
    bash dev/demo.sh

# Tear down the whole stack.
down:
    bash dev/down.sh

# Tail the background server log.
logs:
    tail -f dev/server.log

# Run scarab-server in the FOREGROUND against the dev stack (Ctrl-C to stop).
# Useful when iterating; `just up` runs it in the background instead.
serve:
    SCARAB_DATABASE_URL=postgres://scarab:scarab@127.0.0.1:55432/scarab \
    SCARAB_S3_BUCKET=scarab-logs SCARAB_S3_ENDPOINT=http://127.0.0.1:9000 \
    SCARAB_S3_ACCESS_KEY=scarab SCARAB_S3_SECRET_KEY=scarabsecret SCARAB_S3_REGION=us-east-1 \
    KUBECONFIG=dev/.kubeconfig SCARAB_NAMESPACE=scarab \
    cargo run -p scarab-server -- --role converged --serve --addr 127.0.0.1:8080

# Build + test the workspace. Set SCARAB_TEST_DATABASE_URL to run PG-backed tests.
test:
    cargo test --workspace

# Compile-check everything.
check:
    cargo check --workspace
