# Handoff — building `invoke` (ADR-0038)

Implements the local-reuse primitive decided in
[`docs/adr/0038-invoke-and-local-reuse.md`](../adr/0038-invoke-and-local-reuse.md).
Read that ADR and `CONTEXT.md` (`invoke`, **Library pipeline**, `matrix` entries)
before starting. This is **doc-decided but unbuilt** — `StepSpec` today has only
image-steps and `gate`.

## The one-paragraph model

`invoke` = **compile-time inlining, not a runtime object.** A step
`invoke: ./lib/deploy.yaml` is resolved at `compile` by flattening the referenced
pipeline's steps into the caller's DAG, id-namespaced by the invoke-step id
(`deploy/build`), with `needs` rewritten across the seam. There is **no** new
durable object — restart/resume/time-travel keep working because the result is
just more steps in one flat DAG. `invoke` is **local-only, forever**
(repo-relative path, read at the caller's ref); third-party reuse is an OCI image
(ADR-0008) or a vendored (committed) lib, never a remote `invoke`.

## The load-bearing architectural decision (already made)

Compilation is **pure** (ADR-0031, no I/O), but inlining must read *other* files
(the lib sources). Resolution:

> The server pre-fetches `.scarab/**` via `ForgePort` and passes a
> `{path → source}` map to a resolver-based pure `compile` entrypoint. **I/O at
> the edge; `compile` stays pure over the provided map.**

Do not make `compile` do I/O. Do not add a `ForgePort` dep to `scarab-pipeline`.

## Current state (what exists to build on)

- **Compile path:** `scarab_pipeline::compile_yaml(&yaml)` → IR, called
  server-side at submit (`scarab-server` trigger path). Matrix expansion
  (`expand_step`) and validation already run here.
- **Discovery is flat:** the trigger path lists `.scarab/` top-level only and
  runs each `*.yaml` whose `on:` matches. So `.scarab/lib/*.yaml` is *already*
  not triggered — but the invoke resolver must still be able to **fetch** lib
  files (recursive listing or on-demand by path).
- **`matrix`** is a built, orthogonal per-step modifier (ADR-0023) — slice 4
  composes with it for free.
- **`gate`** is the only other non-image step kind; follow its shape for adding
  `invoke` to `StepSpec` + `validate`.

## The issues (git-bug, `[invoke]` prefix, `area:pipeline type:feat tracer-bullet`)

Do them in dependency order; 2–4 all sit on top of 1 and are independent of each
other.

| ID | Slice | Blocked by |
|---|---|---|
| `a8d536a` | Step kind + single-level compile-time inlining (**tracer bullet** — includes path safety + lib-not-triggered) | — |
| `13c03bc` | Termination: recursive nesting + cycle detection + depth cap | a8d536a |
| `aae36ff` | Explicit `inputs:`/`outputs:` interface, validated at compile | a8d536a |
| `440759d` | `matrix` × `invoke` fan-out | a8d536a |

Grab one with `git-bug bug show <id>` for full acceptance criteria.

## Watch-outs

- **Path safety is in slice 1, not deferred** — a resolver without traversal
  guards (absolute / `../` / cross-repo) is a vuln in the interim.
- **`/` becomes reserved in step ids** (the namespace separator). Check the id
  sanitizer / DNS-1123 pod-name path in `scarab-executor-k8s` still holds once
  ids contain `/` and matrix `[k=v]`.
- **Slice 3 carries the one open syntax choice** (how you declare/pass module
  params — reuse `SCARAB_PARAM_*`/results from ADR-0008; pick the smallest thing
  and record it in the commit). If a reviewer wants to weigh in first, treat it
  as HITL.
- **`inputs:`/`outputs:` here are the *interface* of an invoked pipeline** — do
  not confuse with the existing per-step workspace `inputs:`/`outputs:` (ADR-0007,
  ADR-0035). Name them so they don't collide.

## Ritual (unchanged)

Per issue: read cited ADRs → implement → keep `cargo check --workspace` green →
minimal tests the acceptance implies (real Postgres via `SCARAB_TEST_DATABASE_URL`,
mock only true externals; cluster live-runs `#[ignore]`+env-gated) → commit
`<type>(<area>): <subject>` with a body + `Co-Authored-By: Claude Opus 4.8
(1M context) <noreply@anthropic.com>` → `git-bug bug status close <id>`. Honor
hexagonal purity (ADR-0016/0031) and classical testing (ADR-0017).
