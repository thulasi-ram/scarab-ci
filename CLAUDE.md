# CLAUDE.md — working in this repo

Scarab is a durable-execution, k8s-native CI engine in Rust. The dir is
`scrarab-ci` but the product, crates, and CLI are all **`scarab`**. See
`CONTEXT.md` for the domain language and invariants, and `docs/adr/` for
decisions.

## Running & testing locally — use the `just` recipes

**The `just` recipes are the canonical entrypoints for running and testing
Scarab. Prefer them for any test run / dogfood / local verification — do not
hand-roll `docker` / `kind` / `kubectl` / `helm` invocations.** The recipes
encode the correct env, isolated kubeconfig, image source, pull policy, and
kube safety checks; a bare command skips those and drifts from what CI does.
CI runs the same recipes: `ci.yml` runs the workspace suite on every PR, and
`e2e.yml` runs `just test e2e` on PRs labelled `ci:full` and weekly.

`just` lists every recipe. The Justfile holds the small ones; the modules
`just/stack.just`, `just/test.just` and `just/check.just` hold the rest, each
recipe with its notes beside it.

| Task | Recipe | What it is |
|------|--------|-----------|
| Compile / test the workspace | `just check`, `just test`, `just test rust <filter>` | `cargo check`; the workspace suite (nextest against the compose Postgres + MinIO in `deploy/test-services/`, so PG and S3 tests run for real) then the UI no-DOM vitest suite; one test or a group by nextest filter |
| CI gates | `just check all`, `just pr <n>` | `cargo check` + the `openapi-drift` and chart-immutable gates; whether PR `<n>`'s required checks are green (`.github/required-checks.txt`) |
| UI | `just ui [mock]`, `just test ui` | Vite against the kind stack (host address from `deploy/kind/.env`) or against a built-in fixture; vitest + the Playwright specs |
| **Full local stack** (kind) | `just stack up` → `just stack demo [chain]` → `just stack down [keep]` | one kind cluster `scarab-dev`: Postgres + MinIO + Forgejo + the real chart in it; server and Depot run a host-built binary (cargo-zigbuild on macOS) mounted into a thin runtime image; `just stack reload` rolls a rebuild, `just stack logs` follows the server. See `deploy/kind/README.md` |
| **Data Depot** (ADR-0061, ADR-0063) | `just stack logs depot`, `just stack status` | StatefulSet `scarab-workspace` in the release, with its own log and its own `/readyz` |
| **Dogfood** | `just stack up dogfood` | the stack in GitHub App mode with its own database + bucket, behind a named Cloudflare tunnel; reads `~/.config/scarab/dogfood.env` |
| **Public demo** | `just stack up demo-oracle [ref]` | published images onto the Oracle box (`deploy/demo-oracle/`); refuses to run anywhere else |
| **Fault tier** (ADR-0077 rank 1) | `just test chaos [mid-drain\|mid-feed\|all] [seed]`, `just test chaos replay <scenario.json>` | switches the release to its **chaos mode** (database `scarab_chaos`, bucket `scarab-chaos`, both through toxiproxy; 2 servers + 2 Depot replicas), deletes a Depot Pod with grace 0 mid-drain / mid-feed under three chained Runs, and runs the invariant checker in `crates/scarab-chaos` (exit 0 PASS, 1 FAIL, 2 harness, 3 INCONCLUSIVE). Refused while step Pods of the current mode run; then cancels leftover chaos Runs and switches back to dev or dogfood |
| **Full-stack E2E** | `just test e2e` | every live suite against the release in its e2e mode (own database + bucket, plugins on): the `crates/scarab-e2e` cases, a REAL Forgejo in the cluster (adapter + add-connection → bind → hook → push → Run), the forge_token cases through a cloudflared quick tunnel (network calls), the `scarab-executor-k8s` + `scarab-plugin-k8s` cluster suites (namespace `scarab-kube-tests`), the Argo CD plugin against a fake Argo CD, and `crash_resume` last (SIGKILLs the server from the kind node). `just test e2e <filter>` narrows every nextest pass |
| Housekeeping | `just clean [days]`, `just docs [build]`, `just test coverage` | prune old build artifacts here and in every worktree; the docs site; the suite under cargo-llvm-cov, which rewrites the coverage baseline |

`just stack up` runs the **Data Depot** (ADR-0061's workspace service) as the
release's StatefulSet, and Step Pods reach it and the server through Services. Forgejo
runs in the cluster in every mode, like Postgres and MinIO. The release has
four **modes**, each its own database + bucket on the same Postgres and MinIO:
**dev** (`just stack up`), **dogfood** (`just stack up dogfood`), **e2e**
(`just test e2e`) and **chaos** (`just test chaos`). A mode switch is refused
while step Pods of the current mode run. `just test e2e` switches the release
to its e2e mode (database `scarab_e2e`, bucket `scarab-e2e`, a quick tunnel in
front of Forgejo while it runs, deleted when it exits) and leaves it there;
`just stack up` switches back to dev, and `just stack reload` after `just test
e2e` or `just test chaos` goes to dev. The tests run on the host: they reach the API on
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

