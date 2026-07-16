# 0044. Branch protection matches a symbolic ref, not the commit SHA

- **Status:** Accepted
- **Date:** 2026-07-16
- **Deciders:** thulasi.ram (architect)
- **Refines:** [0037](0037-environment-governance.md) (what `allowed_refs` is evaluated against), [0043](0043-launch-parameters-and-manual-dispatch.md) (dispatch carries a symbolic ref + a resolved SHA), [0010](0010-forge-integration.md) (read-at-ref)

## Context

An Environment's `allowed_refs` protection rule ([0037](0037-environment-governance.md))
branch-scopes a deploy — e.g. `allowed_refs: ["refs/heads/main"]` means *"only
`main` may deploy to prod."* Its patterns are **symbolic ref globs**
(`refs/heads/main`, `refs/heads/*`, `refs/tags/*`), matched by `glob_match`.

But admission evaluated `ref_allowed` against `config_ref(event)` — the
**immutable commit** the config is read and the Run is pinned at (GitHub's
`after` for a push, `head` for a PR, the resolved commit for a dispatch). A
40-char SHA never matches a branch glob, so the rule was silently inert: an
empty `allowed_refs` admitted everything; a **non-empty** one rejected
everything — *including a legitimate `main` deploy*. The guardrail did the
opposite of its job. A second enforcement point had the same defect:
gate-approval re-runs `ProtectionRules::admits(&DeployContext.git_ref, …)` on
release, and `git_ref` was likewise stored from `config_ref` (a SHA), so an
approver-gated protected deploy would never release.

The root cause is one value serving two incompatible purposes: *where to read
the immutable config* (must be a SHA — reproducible, tamper-proof mid-run) and
*which branch/tag the run is "on"* (must be symbolic — that is what protection
rules, deployment history, and humans reason about).

## Decision

Split the two ref meanings explicitly.

- **Read/pin ref** — `config_ref(event)`, the immutable commit. Unchanged; used
  to fetch `.scarab/**` and pin the self-describing Run ([0022](0022-upgrades-and-versioning.md)).
- **Protection ref** — a new `Event::protection_ref() -> Option<String>`, the
  **symbolic** branch/tag ref used for *all* `allowed_refs` matching (both the
  creation-time check and the gate-approval re-check, via `DeployContext.git_ref`):
  - `Push` → its `refs/heads/…` ref;
  - `Tag`/`Release` → `refs/tags/<tag>`;
  - `PullRequest` → `refs/pull/<n>/head` — deliberately **not** a branch ref, so
    a PR is denied a branch-scoped Environment unless the env explicitly opts PRs
    in via `refs/pull/*` (the intended fail-safe);
  - `Manual`/`Api` → the symbolic dispatch ref (see below);
  - `Comment`/`Cron`/`Upstream` → `None`.
- **Fail-closed on `None`:** an event with no symbolic ref is admitted **only**
  when `allowed_refs` is empty (unrestricted); any non-empty rule denies it.
- **Manual/Api events carry both**, mirroring `Push { r#ref, after }`:
  `{ r#ref: <symbolic>, sha: <resolved commit> }`. Dispatch ([0043](0043-launch-parameters-and-manual-dispatch.md))
  resolves the user ref → SHA for reading/pinning, and **canonicalizes** the
  user ref for protection: a `refs/…` ref or a 40-char lowercase-hex SHA is kept
  verbatim (a raw SHA stays opaque so it can never match a branch glob — a bare
  commit is correctly denied a branch-scoped env), and any other bare name is
  treated as a branch (`main` → `refs/heads/main`).

## Consequences

- **`allowed_refs` finally gates by branch** — the headline fix — on the push,
  dispatch, and PR paths, at both enforcement points. Regression tests assert a
  `feature`-branch run is denied a `refs/heads/main`-scoped env and a `main` run
  is admitted while still pinning the resolved SHA; these tests fail against the
  pre-fix code.
- **Deployment history records the symbolic ref**, not a SHA — what an auditor
  expects.
- **PRs are fail-safe** against protected environments by default.
- The read/pin path is untouched, so reproducibility and tamper-resistance are
  preserved.

## Alternatives considered

- **Keep matching `config_ref` and require `allowed_refs` to list SHAs** —
  absurd: SHAs are per-commit, so the rule could never express "the `main`
  branch," which is the entire point of branch protection.
- **Normalize inside `ref_allowed`** (strip to a SHA, or fuzzy-match) — pushes
  ref semantics into the pure `scarab-projects` matcher and guesses; the ref
  meaning belongs to the event, which is where `protection_ref` lives.
- **A single canonical ref on every event** — collapses the two genuinely
  different needs (immutable read point vs symbolic branch) that this ADR exists
  to separate.
