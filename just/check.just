# `just check [<recipe>]`: compile checks and the CI gates that are not test
# suites. Recipes run from the repository root.
set working-directory := '..'

# `just check` with no recipe.
[default]
[doc('cargo check --workspace')]
default:
    cargo check --workspace

# What CI checks besides the test suites: `cargo check`, then the contract gate
# (the CI `openapi-drift` job: rustfmt, then spec and client drift), the chart guard (a step of the CI `chart`
# job) and the chart render check, in that order; the first failure stops it.
# The chart recipes need helm + yq v4.
[doc('cargo check, then the fmt, openapi-drift, chart-immutable and chart-render gates')]
all: default _fmt _openapi-drift _chart-immutable _chart-render

# Formatting gate (a step of the CI `openapi-drift` job). The rustfmt version is
# pinned by rust-toolchain.toml and the style edition by rustfmt.toml.
[private]
_fmt:
    cargo fmt --all -- --check || { echo "error: run 'cargo fmt --all' and commit the result" >&2; exit 1; }

# Contract gate (mirrors the CI `openapi-drift` job, ADR-0054): regenerate the
# OpenAPI spec and the generated TS client, then fail if either differs from
# what is committed. Full-route coverage is asserted by the test suite
# (step_up_routes.rs: route_table_equals_the_openapi_spec) — this gates DRIFT only.
[private]
_openapi-drift:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "==> regenerating openapi.json"
    cargo run -p scarab-server -- --emit-openapi openapi.json
    git diff --exit-code openapi.json || {
      echo "error: openapi.json is stale — regenerate and commit it:" >&2
      echo "  cargo run -p scarab-server -- --emit-openapi openapi.json" >&2
      exit 1
    }
    echo "==> regenerating the UI client"
    npm --prefix ui/scarab-web-ui ci
    npm --prefix ui/scarab-web-ui run gen
    npm --prefix ui/scarab-web-ui run typecheck
    git diff --exit-code ui/scarab-web-ui/src/api/schema.ts || {
      echo "error: the generated TS client is stale — regenerate and commit it:" >&2
      echo "  npm --prefix ui/scarab-web-ui run gen" >&2
      exit 1
    }
    echo "==> no drift: openapi.json and schema.ts match the code"

# Chart guard (mirrors the CI `chart` job step): render the chart at two
# versions and fail if any immutable StatefulSet spec field differs — such a
# field makes every chart upgrade a rejected update. Needs helm + yq v4.
[private]
_chart-immutable:
    scripts/chart_statefulset_immutable.sh deploy/helm/scarab

# Chart render check: under login-on values, scarab.uiUrl reaches the ConfigMap
# as SCARAB_UI_URL, and each scarab.oauth.providers client secret reaches both
# the server Deployment and the Depot StatefulSet (whose config gate refuses
# the boot without them). Needs helm + yq v4.
[private]
_chart-render:
    scripts/chart_render_check.sh deploy/helm/scarab

# The CEL policy checker (ADR-0075) against the cel-spec conformance cases inside
# its subset. cel-rust's conformance/tests/gen embeds cel-spec's cases, so this
# needs a cel-rust checkout and no network:
#   git clone https://github.com/cel-rust/cel-rust && just check conformance cel-rust
# A relative <cel_rust> is read from the repository root.
[doc('The CEL policy checker against cel-spec conformance cases (needs a cel-rust checkout)')]
conformance cel_rust:
    #!/usr/bin/env bash
    set -euo pipefail
    out="${CARGO_TARGET_DIR:-target}/cel-conformance-cases.jsonl"
    # cargo runs the test from the crate directory: pass an absolute path.
    case "$out" in /*) ;; *) out="$PWD/$out" ;; esac
    mkdir -p "$(dirname "$out")"
    python3 -I crates/scarab-policy/tests/conformance/extract.py "{{cel_rust}}/conformance/tests/gen" > "$out"
    SCARAB_CEL_CONFORMANCE_CASES="$out" cargo test -p scarab-policy --test conformance -- --ignored --nocapture
