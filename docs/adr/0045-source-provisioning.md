# 0045. Source provisioning: the `clone` step kind

- **Status:** Proposed
- **Date:** 2026-07-16
- **Deciders:** thulasi.ram (architect)

## Context

ADR-0008 named `clone` (forge-aware checkout) as a first-class built-in step
kind, with the workspace convention at `/workspace`. The *mechanism* was never
designed and never built: `build_pod` mounts no source, the `ForgePort`
exposes only per-file `read_file_at_ref`/`list_dir_at_ref` (no archive fetch),
and `Repo` carries only `{owner, name}` (no clone URL). Consequence: a step Pod
today runs with only its image filesystem + fence env — **no repo contents ever
reach a step** (proven live, 2026-07-16 audit). Until this lands, Scarab is an
orchestrator of prebuilt images, not a CI, and cannot dogfood (build itself).

This ADR decides how the `clone` kind provisions source and how that source
becomes the content-addressed workspace (ADR-0029) that downstream steps
inherit along `needs` edges (ADR-0007).

## Decision

### Mechanism — SHA-pinned `git` clone in a step Pod (not a control-plane fetch)

`clone` runs `git` inside a restricted step Pod and produces a workspace, rather
than the control plane fetching a tarball via the `ForgePort`.

- **Why git, not a control-plane archive:** a tarball at a SHA has no history.
  Real workloads need it — a full clone is required for e.g. a security/forensic
  agent that walks blame and the object graph. Only a real clone can serve that.
- The clone is **pinned to the resolved commit SHA**, not the symbolic ref
  (ref→SHA resolution already exists for dispatch, ADR-0043/0044). The SHA is
  the natural fence (ADR-0021): re-cloning on restart is idempotent.
- **Depth knob:** shallow (`depth: 1`) by default — small CAS snapshots for the
  common build case; `depth: full` (complete history, all refs) opt-in for the
  history-dependent cases.

### Trust — one mechanism, read-only short-TTL token for fork PRs

Pure git-clone for **all** runs in v1. No second checkout path.

- Trusted runs (push / internal PR / dispatch): repo-scoped forge token.
- **Fork-PR runs:** the *same* `clone` step, but with a token that is
  **read-only, repo-scoped, and short-TTL (minutes)** — the model GitHub Actions
  itself uses for fork builds. Combined with the already-designed fork-PR secret
  lockout and restricted OIDC subject (ADR-0032/0015), the blast radius of a
  leaked token is "read the repo you already forked."
- This is coherent with CONTEXT §3: Scarab is **not a hostile-tenant system in
  v1** (gVisor/Kata deferred). Defending against untrusted fork code reading its
  own source with a scoped token is not a v1 threat.
- **Deferred (rides with hostile-tenant isolation):** a tokenless
  control-plane fetch that seeds the CAS with no credential ever entering a
  Pod. Recorded as the hardening path if/when gVisor/Kata lands; not built now
  (it would be a second checkout mechanism yielding historyless trees).

### Authoring — explicit `clone` step, zero-config

`clone` is authored explicitly (no implicit auto-prepended checkout), preserving
ADR-0007's single rule ("a Step inherits the merged workspace of its `needs`")
with no exception, and staying honest for repo-less triggers (`upstream` on
artifacts, `cron`/`manual`/`api` with no ref).

- Because `clone` is a first-class *kind* (not a marketplace action), the author
  writes `clone: {}` and the engine fills repo/ref/SHA/token from the run's
  trigger context. Downstream steps `needs: [checkout]`.
- **Guardrail is a lint, not a hard error:** a `push`/`pull_request` pipeline
  with no `clone` step emits a non-fatal compile diagnostic (some triggered
  pipelines legitimately need no source). This is the first real rule for the
  `scarab lint` subcommand.

### Workspace contents — working tree **and** `.git`

The CAS snapshot includes `.git`, not just the working tree.

- Stripping `.git` would silently break `git describe`/`rev-parse` versioning
  (a large fraction of real builds) and make the full-history forensic case
  impossible. Retaining it is essential context.
- Per-file merkle CAS over `.git` packfiles dedups *across runs* well (stable
  history → identical packfiles → only new objects transfer). It cannot dedup
  *within* a repacked packfile — exactly the content-defined-chunking
  optimization ADR-0029 already parks as "later," so v1 is coherent.
- Skip-if-unchanged (ADR-0027) stays intact: the implicit whole-workspace hash
  changing on every commit is *correct* (new commit = re-run); a step wanting
  subtree precision declares explicit `inputs:` (ADR-0007).
- No strip knob in v1 (CONTEXT §8 minimalism); add one only if a real workload
  complains about `.git` size.

### Token delivery — tmpfs + askpass, credential-free `.git/config`

Because `.git` now enters the CAS, a credential embedded in the remote URL would
leak into every downstream workspace *and* into durable object storage. So:

- The short-TTL token is written to a **tmpfs file under `/scarab/secrets/`**
  (ADR-0008's secrets convention); `git` reads it via `GIT_ASKPASS` (or a
  credential helper). tmpfs is never part of the workspace tree — it never
  enters the CAS and vanishes with the Pod.
- The remote URL persisted in `.git/config` is **credential-free**.
- **Never** the token-in-URL or token-in-argv form (leaks into `.git/config`,
  CAS, and `ps`).
- **Pre-snapshot guard:** before the CAS snapshot, assert `.git/config` (and
  `.git/**` config) carry no credential — cheap belt-and-suspenders now that the
  snapshot carries `.git`.

### Clone image — canonical `scarab-clone`, opt-in submodules/LFS

`clone` runs a **Scarab-maintained image** (`scarab-clone`: `git` + `git-lfs` +
the askpass helper + the config-scrub guard baked in), pinned by digest and
versioned with the server — not the author's image. The kind has security
invariants (credential-free config, tmpfs askpass) and tooling needs an
arbitrary image can't guarantee.

- `submodules: true` → recursive fetch with the run's token. **Cross-installation
  private submodules are a documented limitation** (the installation token may
  not cover another org's repo).
- `lfs: true` → git-lfs fetch, served by the canonical image.
- **Scope acknowledged:** Scarab now ships two first-party images — the
  results-egress sidecar (ADR-0042, currently unbuilt/phantom `:latest`) and
  `scarab-clone`. Both build + digest-pin in the same `image.yml` pipeline as
  the server.

### Fencing & determinism — pinned SHA always; vanished SHA fails fast

`clone` is a **pure read** — no external mutation, so no double-effect hazard
(unlike `docker push`). It takes the standard `{run, step, attempt}` fence
(ADR-0021); re-execution is inherently safe.

- The run is pinned to the commit SHA resolved **once at trigger time**
  (ADR-0043/0044, stored on the self-describing run). `clone` always fetches
  *that SHA*, **never re-resolves the ref** on restart/resume. Re-cloning the
  same SHA → same tree → same CAS root → deterministic resume. This is the wedge
  paying off: a run resumed after a crash or a weeks-long gate rebuilds from the
  identical source.
- A **vanished pinned SHA** (force-pushed away / ref deleted between attempts)
  is a **terminal, diagnostic failure** (`SourceUnavailable`), not a retry loop —
  the SHA will not come back. Its message says so explicitly: the commit this
  run was pinned to no longer exists on the forge, which **signals an upstream
  integrity anomaly** (history rewritten / ref deleted), not routine churn.
  Surfaced loudly; dead-letters with that reason once the ADR-0020 dead-letter
  machinery exists.

### CAS boundary & GC — ephemeral run workspace, dedup across runs

- The clone snapshot is the run's **ephemeral** workspace (ADR-0007); its CAS
  entries are reclaimable when the run is retired under retention (ADR-0030).
- GC is refcount / mark-sweep over CAS roots: shared objects (stable `.git`
  packfiles) survive as long as *any* live run references them — cross-run
  dedup is the win. This is the "CAS store + GC" ADR-0029 already committed to;
  no new mechanism.

## Consequences

- `clone` becomes the run's workspace root; downstream steps inherit it via
  `needs` (ADR-0007) with no new inheritance rule.
- **Implementation surface (all currently absent):**
  - `clone` step kind in the IR + `scarab-pipeline` compiler (today: image +
    `gate` only); the lint rule for the "no clone on push/PR" warning.
  - CAS `materialize`/`ingest` wired into `build_pod` — the ADR-0029 substrate
    that exists in `scarab-storage-s3` but is never called on k8s. This is the
    *same* wiring the workspace-passing and `outputs:` gaps need, so `clone`
    and inter-step workspace flow land together.
  - A clone URL on the normalized forge model (`Repo` carries only
    `{owner, name}` today).
  - A scoped, short-TTL, read-only-for-forks token-minting path — **depends on
    the GitHub App adapter** (installation tokens), which is itself unbuilt
    (8× `unimplemented!()`). `clone` for private repos cannot ship before it.
  - The `scarab-clone` image + its build in `image.yml`.
- **Ordering:** `clone` is the keystone of "Scarab builds Scarab" — until it
  lands, no step sees source and the project cannot dogfood. It is gated on the
  GitHub App adapter (auth) and the CAS-into-`build_pod` wiring; those two are
  its prerequisites.
- Fork-PR runs get git + a read-only short-TTL token; the tokenless
  control-plane fetch is deferred with hostile-tenant isolation.

## Alternatives considered

- **Control-plane tarball → CAS seed (option B):** no token in any Pod, source
  content-addressed from moment one — but a new `ForgePort` method per adapter,
  and yields historyless trees (fails the full-clone requirement). Kept as the
  deferred hardening path.
- **`clone` as a user-space step with a long-lived token (naive A):** simplest,
  but a broadly-scoped token in every Pod, worst fork-PR exposure. Rejected.
