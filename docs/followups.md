# Follow-ups & demand-gated features

Work that is **deliberately not built yet**. Scarab's bias (ADR-0017 "grow the
suite from real bugs", ADR-0023 "dynamic reserved", CONTEXT §8 "minimal in v1")
is to ship the smallest correct thing and let *real usage* pull the rest. This
doc is the backlog of what we consciously left out and the **demand signal** that
should trigger building each item — so a deferral is a recorded decision, not an
omission.

Two buckets:

- **Quick follow-ups** — small, low-risk, mechanical. Do them when convenient or
  when the first user trips over the edge. No new design.
- **Demand-gated features** — real design/complexity/blast-radius. Build **only**
  when a concrete workload asks for it. Each names the trigger that would justify
  the cost.

> Not to be confused with [`docs/adr/README.md` → "Open / deferred"](adr/README.md),
> which tracks *ADRs to be written when their slice begins*. This doc tracks
> *features to be built when demand appears*.

---

## Quick follow-ups

| Item | Origin | Trigger / notes |
|---|---|---|
| `StepDto.security` + an environment reference on the inline `POST /v1/runs` body, mapped through `admit_step_grants`, then regen OpenAPI + UI client | ADR-0039 | Only if someone needs a **privileged inline run**. Today inline runs are baseline-only by construction (no `security`, no Environment). Deliberate: the ad-hoc endpoint is the ungoverned path. |
| Live-cluster verification of the step Pod `SecurityContext` (restricted baseline + applied grants) | ADR-0039 | Currently unit-tested only; the k8s round-trip stays `#[ignore]` + `SCARAB_TEST_KUBE=1` until a dev `kind` cluster exists. |
| Trigger-path e2e test proving fail-closed grant rejection (forge fake + Postgres) | ADR-0039 | The admission *logic* is unit-tested; an end-to-end reject-the-run test is a nice regression guard. |
| `always()` / `if:` skip opt-out | ADR-0033 tail | Optional; only if a real pipeline needs a step to run despite a skipped ancestor. |

---

## Demand-gated features

### Privilege model (ADR-0039)

| Feature | Trigger to build | Blast radius / caveat |
|---|---|---|
| `host-path` grant | A workload that genuinely needs a host directory *and* cannot use `privileged` | `privileged` already reaches host-fs power; build the narrower grant only to let admins grant **less** than full privileged. |
| `host-network` / `host-pid` / `host-ipc` grants | **Provisionally a hard no.** Revisit only with hard-tenant isolation (gVisor/Kata, ADR-0005) *and* a real need | `hostNetwork` → node metadata endpoint → node IAM creds → cloud-account compromise. The most dangerous item on the list; `privileged` does **not** even include it. |
| `cluster-access` grant (in-cluster k8s API) | A concrete need to talk to Scarab's *own* cluster API that kubeconfig-as-a-secret can't serve | Ruled out for v1 — it's a privilege-escalation path to the control plane on a soft-multitenant system. Deploys to a *target* cluster already work via kubeconfig-as-secret (ADR-0037). |
| Admin-extensible capability catalog (orgs define named capability bundles) | Strong, repeated demand for grants outside the closed vocabulary | Rejected for v1: a friendly-named admin-defined "capability" that is secretly `privileged` breaks the audit story. Closed vocabulary stays until proven insufficient. |

### Supply chain / provenance (ADR-0015) — all usage-driven

| Feature | Trigger to build | Notes |
|---|---|---|
| **SBOM export + signing** (SLSA / cosign) for built images | A user/compliance requirement for build provenance | Slice-7 fast-follow; the `ImageArtifact` + push-fence from slice 5 is the foundation. |
| **Plugin cert / provenance-based keying for privilege** — "any image signed by identity X may run this grant" | Digest-only re-whitelisting friction becomes a real pain **and** the provenance substrate above exists | ADR-0039 keying tiers: allowed later **only** for the low-risk grants (`run-as-root`, `add-capabilities`). **`privileged` stays digest-only forever** — the friction is the separation-of-duties control, and signatures trust the signer, not the exact bytes. Also adds signature-verification I/O to admission (a port/adapter, ADR-0016/0031). |
| Image **mirroring + self-signing** as the recommended operational pattern for third-party privileged images | When teams start running third-party privileged images at scale | Pull the image into your own registry, sign on ingest, whitelist your-org's signature (paired with the keying above). |

### Local reuse / composition (ADR-0038)

| Feature | Trigger to build | Notes |
|---|---|---|
| Remote `invoke` (`invoke: repo//lib@sha`) | **Likely never.** Only if vendoring proves unworkable | Reopens the trust seam ADR-0038 closed. Cross-repo *composition* is served by vendoring; cross-repo *causation* by `on: upstream`. |
| Git submodule support for vendored libs (pinned SHA in tree, resolved at the ref) | Teams want to vendor without copying bytes, at scale | Needs `ForgePort` to resolve submodule content at a ref (it doesn't today). The only *reproducible* "don't copy bytes" variant. |
| Blessed, total `data → IR` generation frontend (Starlark / CUE) | A real workload needs **variable graph *shape*** per input (not just N instances of a fixed subgraph — `matrix × invoke` already covers that) | ADR-0009 pt 4 reserved this ("multi-frontend by construction"). A frontend, **never** an AST-mutating plugin. |

### Workspace inputs/outputs (ADR-0007 / ADR-0029)

Both halves are now **shipped**: `inputs:` per-need scoping (2026-07-24) and
per-PATH `outputs:` publishing (2026-07-25). The `outputs:` entry that used to
sit here called itself "blocked on CAS sub-tree addressing" — that turned out to
be wrong, and is worth recording as a lesson: the CAS is a **per-file merkle**
store, so a tree is already a hashed list of `name -> blob|tree` entries.
Selecting a path subset needed no new storage capability at all, just a walk with
`tree_entries` and a bottom-up rebuild with `put_tree` (`scarab_storage::prune_tree`),
sharing every blob with the full snapshot. The deferral had been reasoning about a
path-prefix-addressing design the code never needed.

One optimization is genuinely left, and it is small: the egress leg still tars
the **whole** workspace out of the Pod before pruning, so a narrow `outputs:` on
a huge workspace saves storage but not transfer. Selecting at `tar` time would
fix that; it was not done because it moves authored paths into a shell command
string (quoting/injection surface) and makes "declared path was not produced"
a `tar` exit code instead of a precise diagnostic. Build it if egress transfer
time on large workspaces becomes a measured problem.

### Identity & access (ADR-0049 / ADR-0060)

| Feature | Trigger to build | Notes |
|---|---|---|
| Org **RBAC / member management** UI — the third global-Settings section (view/grant/revoke `Principal × scope × Role`) | Committed for "sometime" per the ADR-0060 grill; build when multi-user access needs hands-on management beyond forge-import bootstrap | The `/v1/orgs/{org}/bindings` API + native binding model already exist (ADR-0049); this is a management surface, not new model. Deferred out of ADR-0060 to keep that work to forge/secrets. |

---

## Planned, *not* demand-gated

These are committed roadmap work, listed here only to keep them off the "deferred"
mental bucket:

- ~~**Build `invoke`** (ADR-0038)~~ — **SHIPPED** (2026-07-24 sweep:
  `inline_invokes` compile-time flattening + local-only validation in
  scarab-pipeline). Only the demand-gated remainders above (remote invoke,
  submodule vendoring, data→IR frontend) are still open.
- **Slice 7** (CONTEXT §9.7) — local executor + CLI + provenance/signing fast-follow.
- ~~**ADR-0059 tick fault isolation**~~ — **SHIPPED 2026-07-25.** Per-run
  isolation now covers `admit`/`advance` (plus per-*message* isolation in the
  teardown drains, where a malformed payload could wedge the fleet
  permanently), bounded by a wall-clock deadline that dead-letters a
  persistently un-tickable run. See the ADR's 2026-07-25 amendment for how the
  open questions resolved.
