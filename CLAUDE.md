# CLAUDE.md — working in this repo

Scarab is a durable-execution, k8s-native CI engine in Rust. The dir is
`scrarab-ci` but the product, crates, and CLI are all **`scarab`**. See
`CONTEXT.md` for the domain language and invariants, and `docs/adr/` for
decisions.

## Running & testing locally — use the `just` recipes

**The `Justfile` recipes are the canonical entrypoints for running and testing
Scarab. Prefer them for any test run / dogfood / local verification — do not
hand-roll `docker` / `kind` / `kubectl` / `helm` invocations.** The recipes
encode the correct env, isolated kubeconfig, image source, pull policy, and
kube safety checks; a bare command skips those and drifts from what CI does.
CI runs the same recipes: `ci.yml` runs the workspace suite on every PR, and
`e2e.yml` runs `just e2e` on PRs labelled `ci:full` and weekly.

| Task | Recipe | What it is |
|------|--------|-----------|
| Compile / test the workspace | `just check`, `just test` | `cargo check` / nextest against the compose Postgres + MinIO in `deploy/test-services/` (PG and S3 tests run for real) |
| UI / server from source | `just ui`, `just reload` | Vite against the kind stack; `just reload` rebuilds the server and rolls its Pods; host address from `deploy/kind/.env` |
| **Full local stack** (kind) | `just up` → `just demo` → `just down` | one kind cluster `scarab-dev`: Postgres + MinIO + Forgejo + the real chart in it; server and Depot run a host-built binary (cargo-zigbuild on macOS) mounted into a thin runtime image; `just reload` rolls a rebuild, `just logs` follows the server. See `deploy/kind/README.md` |
| **Workspace service** (ADR-0061) | `just workspace-logs`, `just workspace-status` | the Depot, StatefulSet `scarab-workspace` in the release, with its own log and its own `/readyz` |
| **Dogfood** | `just up dogfood` | the stack in GitHub App mode with its own database + bucket, behind a named Cloudflare tunnel; reads `~/.config/scarab/dogfood.env` |
| **Fault tier** (ADR-0077 rank 1) | `just chaos [mid-drain\|mid-feed\|all] [seed]`, `just chaos replay <scenario.json>` | switches the release to its **chaos mode** (database `scarab_chaos`, bucket `scarab-chaos`, both through toxiproxy; 2 servers + 2 Depot replicas), deletes a Depot Pod with grace 0 mid-drain / mid-feed under three chained Runs, and runs the invariant checker in `crates/scarab-chaos` (exit 0 PASS, 1 FAIL, 2 harness, 3 INCONCLUSIVE). Refused while step Pods of the current mode run; then cancels leftover chaos Runs and switches back to dev or dogfood |
| **Full-stack E2E** | `just e2e` | every live suite against the release in its e2e mode (own database + bucket): the `crates/scarab-e2e` cases, a REAL Forgejo in the cluster (adapter + add-connection → bind → hook → push → Run), the forge_token cases through a cloudflared quick tunnel (network calls), the `scarab-executor-k8s` + `scarab-plugin-k8s` cluster suites (namespace `scarab-kube-tests`), and `crash_resume` last (SIGKILLs the server from the kind node). `just e2e <filter>` narrows every pass |

`just up` runs the **workspace service** (ADR-0061) as the release's
StatefulSet, and Step Pods reach it and the server through Services. Forgejo
runs in the cluster in every mode, like Postgres and MinIO. The release has
four **modes**, each its own database + bucket on the same Postgres and MinIO:
**dev** (`just up`), **dogfood** (`just up dogfood`), **e2e** (`just e2e`) and
**chaos** (`just chaos`). A mode switch is refused while step Pods of the
current mode run. `just e2e` switches the release to its e2e mode (database
`scarab_e2e`, bucket `scarab-e2e`, a quick tunnel in front of Forgejo while it
runs, deleted when it exits) and leaves it there; `just up` switches back to
dev, and `just reload` after `just e2e` or `just chaos` goes to dev. The tests run on the host: they reach the API on
`SCARAB_ADDR`, Postgres, Forgejo and the Depot through `kubectl port-forward`, and when a test is itself a server a Pod must call (a
fake forge, a results listener), the host address a Pod uses is **probed from
an actual Pod** (`deploy/kind/test-env.sh`), because the kind docker network
gateway is wrong on darwin.

The deployments live under `deploy/`: `kind/` (the stack in its dev, dogfood,
e2e and chaos modes, Forgejo included) and `demo-oracle/` (the public demo), plus
`test-services/`, the compose Postgres + MinIO `just test` uses.
Image build contexts live under `docker/` (`server/`, `runtime/`, `clone/`,
`sidecar/`, `wsfetch/`). `deploy/helm/` is the distribution chart.

If a recipe is missing for something you need to test, **add/extend a recipe**
rather than running the raw commands ad hoc — that keeps the next run
reproducible.

## Writing docs

Any prose (docs site, READMEs, ADRs, CONTEXT.md, UI copy) follows
`docs/WRITING.md`: plain and concrete, no intensifiers, few adjectives, none of
the banned jargon. What we may *claim* is governed by `docs/positioning.md`.

## Testing philosophy

Classical (not mockist): mock only true externals; keep the suite minimal in v1
and grow tests from real bugs. See `CONTEXT.md` §8 and ADR-0017.

