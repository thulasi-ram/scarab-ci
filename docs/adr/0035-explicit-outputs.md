# 0035. Explicit workspace `outputs:`

- **Status:** Accepted
- **Date:** 2026-07-13
- **Deciders:** thulasi.ram (architect)

## Context

The workspace data-passing model ([0007](0007-data-passing-model.md)) is
implicit-by-default, explicit-on-demand. The input half (`inputs:`) landed with
restart skip-if-unchanged ([0027](0027-restart-semantics.md)). The output half —
which paths a step *publishes* downstream — was still whole-workspace only, so a
step always exported everything (`target/`, `node_modules/`) and its output hash
changed on any incidental file.

## Decision

**A step may declare `outputs: [<workspace-relative paths>]`** — the subset of its
workspace it publishes. Absent = the whole workspace (the implicit default).

- **Authored + validated now** (submit-time): each path must be non-empty and
  **workspace-relative** — no leading `/`, no `..` traversal out of the
  workspace. An empty list is rejected (omit the key for "everything"). The field
  travels in the run's stored IR (self-describing, [0022](0022-run-versioning.md)).
- **Enforced on the live-workspace path.** The actual restriction happens at the
  post-step CAS snapshot ([0029](0029-workspace-cas.md)): the executor ingests
  only the declared paths, so the output tree hash reflects exactly them. That
  ingest is part of the live k8s post-step workspace path (not yet built), so
  enforcement lands with it; the executor reads `outputs:` from the run IR.
- **Not in the REST DTO.** Like `inputs:`/`gate:`/`concurrency:`, `outputs:` is
  authored via a committed `.scarab` (`compile_yaml`), not the inline
  `POST /v1/runs` IR-subset DTO — that subset stays deliberately small.

## Consequences

- Precise cache/output hashes and safe fan-out become expressible; a change
  outside a step's declared outputs won't churn its downstream once the live path
  enforces it.
- Submit-time validation rejects unsafe paths (absolute, escaping) early rather
  than at run time.
- The field is authored and stored ahead of its enforcing consumer — a deliberate
  "author now, enforce with the live path" split, matching how `environment:`
  landed ahead of full admission enforcement.

## Alternatives considered

- **Defer entirely** until the live-workspace path — but authoring + validation
  have standalone value (early rejection, the IR carries intent), and this keeps
  the `inputs:`/`outputs:` pair coherent in the DSL.
- **Thread `outputs` onto the engine launch `StepSpec` now** — needless churn
  across ~15 construction sites for a consumer that does not exist yet; the run IR
  already carries it for the live executor to read.
