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
colima safety guards; a bare command skips those and drifts from what CI does.

| Task | Recipe | What it is |
|------|--------|-----------|
| Compile / test the workspace | `just check`, `just test` | `cargo check` / nextest against the compose Postgres (PG tests run for real) |
| UI / server from source | `just ui`, `just serve` | dev loop against the proc stack; env from `deploy/local-proc/.env` |
| **Full local stack** (kind) | `just up` → `just demo` → `just down` | one kind cluster `scarab-dev`: Postgres + MinIO + the real chart in it; server and Depot run a host-built binary (cargo-zigbuild on macOS) mounted into a thin runtime image; `just reload` rolls a rebuild, `just logs` follows the server. See `deploy/kind/README.md` |
| **Workspace service** (ADR-0061) | `just workspace-logs`, `just workspace-status` | the Depot, StatefulSet `scarab-workspace` in the release, with its own log and its own `/readyz` |
| **Dogfood** | `just dogfood` | `just up` in GitHub App mode with its own database + bucket, behind a named Cloudflare tunnel; reads `~/.config/scarab/dogfood.env` |
| Helm on colima (retired) | `just local-helm` | the old dogfood on colima's k3s, which is off; `just dogfood` replaces it |
| **Live Forgejo verification** | `just forgejo-verify` | a REAL Forgejo container + the proc stack; drives add-connection → bind → hook → push → Run. Env-gated (`SCARAB_TEST_FORGEJO`), never in CI |

`just up` runs the **workspace service** (ADR-0061) as the release's
StatefulSet, and Step Pods reach it and the server through Services. `just e2e`,
`just forgejo-verify`, `just kube-tests` and `just serve` still use **proc mode**
(`deploy/local-proc/`: compose Postgres + MinIO, the server and the Depot as
host processes, kind for steps), where the URL a *Pod* uses is **discovered by
probing from an actual Pod**, because the kind docker network gateway is wrong
on darwin (see `deploy/local-proc/up.sh`). Run `just down` before a proc-mode
recipe: both use `SCARAB_ADDR`'s port.

The deployment modes live under `deploy/`: `kind/` (the dev and dogfood stack),
`local-proc/` (proc mode) and `local-helm/` (retired: colima's k3s is off),
plus `local-forgejo/`, the throwaway Forgejo the verification tier drives.
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

