# 0055. Placement profiles: named profiles + baseline + governed `k8s_overlay`

- **Status:** Accepted
- **Date:** 2026-07-19
- **Deciders:** thulasi.ram (architect)

Refines [0026](0026-resource-and-placement.md) (which set the *model* — abstracted labels +
admin mapping + raw escape hatch — but was never implemented and over-specified `size`
tiers). Reuses the governance pattern of [0039](0039-privileged-images.md) (a request
carries no authority; fail-closed at admission) and the admin-owned-bundle pattern of
[0037](0037-environment-governance.md). Keeps the IR portability goal of
[0009](0009-dsl-ir-yaml-cel.md).

## Context

[0026] decided placement should be abstract-in-the-IR with an admin-owned label→k8s mapping
and a raw escape hatch. It was **never built**: `scarab-pipeline::StepSpec` carries
`runs_on: { labels: Vec<String> }` and `resources: { cpu_millis, memory_mib }`, but neither
survives compilation into `scarab-engine::StepSpec`, so **nothing reaches the Pod**. Authors
can write these fields today and get zero effect and zero error. The `scarab-executor-k8s`
Pod builder ends with `..Default::default()` — no `node_selector`, `tolerations`, `affinity`,
or container `resources`.

The forcing function is Acme (the intended dogfood cluster). Its CI nodes are tainted
(`workload-type=application-sub-critical:NoSchedule`) across **multiple pools** by
architecture (amd64/arm64) and workload tier. Their Woodpecker setup needed the toleration
on *every* step Pod or nothing schedules, and was fragile enough that they bolted on **two**
admission controllers (a Kyverno `ClusterPolicy` *and* a `MutatingAdmissionPolicy`) to force
the toleration on when a workflow forgot it. That fragility is the root problem: **the
taint-toleration is a cluster-baseline fact, not a per-job choice**, and any design that
makes it opt-in-per-job reproduces the pain.

Scarab has a structural advantage Woodpecker lacks: **the control plane builds the step Pod
directly** (no agent carries a forgettable env var, no admission webhook is needed). So the
baseline can be stamped on at construction time.

Two kinds of leak must both be avoided, not just one:

- **k8s leak** — Kubernetes primitives (tolerations, nodeSelectors, taint keys) appearing in
  a pipeline file.
- **topology leak** — cluster-specific knowledge (pool names, node labels) in a pipeline
  file that won't survive moving to another cluster.

A named-profile reference has no k8s leak but a bounded topology leak (the profile name). Raw
k8s has both. `size:` tiers ([0026]) are a third, separate mistake: they hide a *magnitude*
(cpu/mem) the author would rather state exactly.

## Decision

A four-part surface. **Magnitudes are stated as specifics; identity/placement is a named,
admin-owned profile; the rare raw case is governed, fail-closed.**

### A. Control-plane **baseline** (the pain-killer)

An operator sets a baseline once (Scarab server/executor config): default `tolerations`,
`nodeSelector`, and default container `resources`. The **control plane stamps the baseline
onto every step Pod** at build time in `scarab-executor-k8s`. No agent env var to forget, no
Kyverno/`MutatingAdmissionPolicy` backstop. This alone unblocks a tainted cluster with zero
pipeline involvement.

### B. `PlacementProfile`s — admin-owned named profiles

- A new admin-owned registry, **`placement_profiles`** (operator config, gitops-managed like
  the rest of a Scarab deployment — *not* a per-project entity). Each entry:
  - a **`name`** (referenced by jobs),
  - an optional **`default: true`** (used when a step names no profile),
  - a **`k8s:`** block — the concrete, admin-owned mapping (`nodeSelector`, `tolerations`,
    `runtimeClass`, annotations…). This is an **opaque overlay**, not a fixed-field schema:
    an admin can bake *any* static k8s placement fact into a profile without a Scarab schema
    change (this is what makes Case 1 below absorb almost everything — see Consequences).
- Architecture (`amd64`/`arm64`) is **not** a special axis; it is just part of a profile's
  `k8s.nodeSelector`. Because a job composes **many** profiles (C), profiles can stay
  **orthogonal** — an `arm64` profile, a `critical` profile, a `gpu` profile — and a job
  lists the ones it wants. This avoids an arch × tier *combinatorial* set of profiles;
  expect a small, factored set.

### C. Job surface — name profile(s), state specifics

- **`placement_profiles: [<name>, …]`** (plural, on a step) — names one *or more*
  `PlacementProfile`s, whose `k8s:` overlays are **merged in listed order** (later wins on a
  key conflict — profiles are expected to be orthogonal, so conflicts are rare). Omitted /
  empty → the `default: true` profile. The job **names profiles**; it does not declare raw
  k8s. This is the GitHub-Actions `runs-on: [labels]` model, extended to compose. The field
  name is deliberately identical to the admin registry key — same word, two contexts (a list
  of *names* in a pipeline; a list of *definitions* in operator config).
- **`resources: { cpu, memory }`** — exact specifics, applied to the container's
  requests/limits. **No `size:` tiers** ([0026]'s `size` is dropped).
- These two fields **carry authority** (self-service): any author may set them.

### D. `k8s_overlay` — the governed escape hatch (Case 2)

- **`k8s_overlay: { … }`** (on a step) — a raw pod-spec fragment, strategic-merged onto the
  generated Pod **last** (so it wins). The `k8s_` prefix is deliberate: it announces the
  backend-coupling loudly (a pipeline using it will not run on the local/dev executor).
- It **carries no authority** — exactly like `add_capabilities`/`privileged` in [0039]. At
  admission the engine checks whether the run's target Environment permits raw overlays:
  - **permitted** → merged onto the Pod;
  - **not permitted** → the run is **rejected, fail-closed** (never silently dropped).
- Reserved for the rare Case 2: a k8s specific that is **per-job and dynamic** (an unbounded
  value that cannot be pre-baked into a finite set of profiles). Static placement facts
  belong in a profile (B), never here.

### Merge order (final Pod spec)

`baseline (admin)` → `named PlacementProfiles.k8s` (in listed order) → `resources` →
`k8s_overlay (only if granted)`. Later layers win. This mirrors Woodpecker's
`POD_NODE_SELECTOR` + `backend_options` layering, but governed and control-plane-owned.

### Plumbing

`placement_profiles`, `resources`, and `k8s_overlay` must be carried from
`scarab-pipeline::StepSpec` **through** `scarab-engine::StepSpec` (today's gap) into the
`scarab-executor-k8s` Pod builder. The old `runs_on: { labels }` field is replaced by
`placement_profiles: Vec<String>`.

## Consequences

- A tainted, multi-pool cluster (Acme) is unblocked with **no admission-controller
  backstop** — the control plane owns the baseline.
- **Pipelines contain no k8s.** The one bounded topology leak is the profile *names*
  (`placement_profiles: [critical]`), accepted as GHA does — portability across orgs was never
  a real CI property, and Acme is the one cluster that matters.
- **Profiles absorb Case 1 with zero break-away.** Because `k8s:` is an opaque overlay (not a
  fixed vocabulary), a new static placement fact — an exotic annotation, `runtimeClass`,
  topology spread — is an admin edit to a profile, not a Scarab change and not a pipeline
  change. Break-away to `k8s_overlay` is needed only for genuinely *dynamic per-job* k8s,
  which is rare.
- Governance is **not new machinery** — `k8s_overlay` reuses [0039]'s "request carries no
  authority / fail-closed" admission, applied to a new axis. Consistent mental model with the
  existing `security:` field.
- `resources` states real numbers; no `size` abstraction hides magnitude.
- New surface: a `placement_profiles` registry + resolver, baseline application, the
  `k8s_overlay` admission grant, and end-to-end plumbing of the three step fields. Replaces
  the stubbed, never-read `runs_on` labels.

## Alternatives considered

- **Capability matching** (job declares `runs_on: [arm64, critical]` *needs*; profiles
  advertise `provides: [...]`; the engine matches). Eliminates the topology leak entirely, but
  adds a matcher, tie-break rules, and a shared capability vocabulary — more machinery than a
  single cluster warrants. Composing *named* profiles (C, plural) captures most of the
  compositional benefit (orthogonal arch/tier/gpu profiles) without a matcher; the leak it
  reintroduces is bounded and, per GHA, tolerated. Revisit if multi-cluster portability
  becomes a real requirement.
- **Raw k8s in every job** (Woodpecker's model) — the escape hatch *as* the interface. Both
  leaks, and the exact fragility that forced Acme's two admission controllers. Rejected;
  survives only as the governed `k8s_overlay` for Case 2.
- **`size:` named tiers** ([0026], CircleCI `resource_class`) — hides the magnitude the author
  wants to state precisely and invites cost-blindness. Dropped.
- **Placement as an `Environment` field** ([0037] entity) — placement is per-step and
  mandatory-by-cluster even for runs that target *no* Environment (a PR-test step still needs
  the toleration). Binding it to the optional, pipeline-level Environment would leave un-placed
  steps. Placement is its own cluster-scoped concern.
- **Fully resource-derived placement** (no placement field; pool chosen from cpu/mem) — elegant
  for magnitudes, but cannot express a qualitative business tier (`critical`), which is not a
  number. Kept as a possible future routing convenience, not the primary surface.
- **Naming: `pool` / `RunProfile` / `AllocationProfile` / `provision_config`.** `pool` leaks
  cloud/k8s topology-speak; `RunProfile` collides with the `Run` durable entity;
  `Allocation*` pulls toward resource-*quantity* (which `resources` owns); `provision*`
  collides with [0045] source provisioning. **`PlacementProfile`** inherits [0026]'s own word
  ("placement") and is collision-free.
