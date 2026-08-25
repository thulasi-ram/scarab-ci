# 0007. Data-passing model: Workspace / Result / Artifact / Cache

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

A step may emit four genuinely different kinds of data with different lifecycles. The classic
mistake (Argo's single "artifact"; Concourse's everything-is-a-"resource") collapses them and
breeds confusion.

## Decision

Keep **four distinct concepts**:

| Concept | Scope | Lifetime | Purpose |
|---|---|---|---|
| **Workspace** | intra-run, on DAG edges | ephemeral | the filesystem steps build on |
| **Result** | intra-run, on DAG edges | ephemeral | small typed values (version, bool) for params/conditionals |
| **Artifact** | output of record | retained (TTL), downloadable | binaries, reports, images |
| **Cache** | cross-run | best-effort, evictable | `~/.cargo`, `node_modules` (keyed) — **not** correctness-critical |

**Workspace is implicit-by-default, explicit-on-demand:** by default a step inherits the
merged content-addressed workspace of its `needs` (GHA ergonomics). It *may* declare explicit
`inputs:`/`outputs:` to get precise cache keys, restrict what flows (safe fan-out), or remap
(Concourse precision). Explicit I/O is exactly what powers skip-if-unchanged
([0027](0027-restart-semantics.md)).

## Consequences

- Clear retention/UI/caching semantics per concept; no reinvention by users.
- Workspace mechanism is per-file merkle CAS ([0029](0029-workspace-cas.md)).
- More concepts to learn than a 2-concept minimal model — deemed worth it.

## Amendment (2026-07-25) — explicit workspace I/O is fully built

Both halves of "explicit-on-demand" now ship:

- **`inputs:`** (2026-07-24) — a subset of `needs` whose workspaces flow in; also sharpens the
  rerun invalidation signature (`workspace_inputs` / `input_signature`).
- **`outputs:`** (2026-07-25) — the workspace-relative paths a step publishes downstream.
  Authored paths ride the step Pod (`scarab.io/workspace-outputs`), and the egress leg prunes the
  post-step snapshot to exactly those paths via `scarab_storage::prune_tree`. **Fail-closed:** a
  declared path the step did not produce fails the step with a permanent, author-fixable verdict
  (`FailureClass::Config`) — never a quietly narrower publish.

`remap` (the third Concourse-style affordance named above) is **not** built and has no demand.

Note for anyone re-reading old notes: the `outputs:` half was deferred for months as "blocked on
CAS sub-tree addressing". That was a mistaken premise. [0029](0029-workspace-cas.md)'s CAS is a
per-file merkle store, so a tree *is* a hashed list of `name -> blob|tree` entries; restricting a
snapshot to a path subset is a walk plus a bottom-up rebuild over primitives that already existed,
sharing every blob with the full snapshot. No new storage capability was required.

## Amendment (2026-07-26) — `outputs:` is a precision tool, never a performance tax

[0061](0061-workspace-data-path.md) makes wide edges cheap in the substrate (lazy
materialisation + content addressing), and adopts the principle that **the system pays for
its own idiosyncrasies, not the author**. So the implicit inherit-everything default stated
above is permanent, and neither `inputs:` nor `outputs:` may become something an author has
to declare *for speed*. They remain what this ADR made them: tools for precise cache keys,
safe fan-out, and restricting what flows.

The same principle retires "don't put your cache in the workspace" as advice. **Cache**
remains the better-fitting concept for `~/.cargo` / `node_modules`, but using it is an
optimisation, not a rule authors must learn.

Also note the vocabulary split in [CONTEXT.md](../../CONTEXT.md): the mutable filesystem a
Step runs in is a **Workspace**; the immutable content-addressed tree on the DAG edge — the
thing this ADR's `inputs:`/`outputs:` actually govern — is a **Workspace Snapshot**.

## Alternatives considered

- **Workspace + Result only** — folds artifacts/caches into "DIY object store"; users rebuild
  retention/cache-keys badly.
- **Explicit I/O everywhere (Tekton)** — precise but verbose ceremony on every step.
