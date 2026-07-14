# 0040. Named step results + launch-time interpolation (`${{ outputs.<id>.<name> }}`)

- **Status:** Accepted
- **Date:** 2026-07-14
- **Deciders:** thulasi.ram (architect)
- **Refines:** [0008](0008-step-contract.md) (makes the `results` emit channel real), [0009](0009-dsl-ir-yaml-cel.md) (the first `${{ … }}` interpolation pass), [0038](0038-invoke-and-local-reuse.md) (makes an invoked pipeline's `outputs` interface *live*, not just compile-checked)
- **Relates:** [0029](0029-workspace-cas.md) (results are **not** the workspace snapshot), [0021](0021-double-effect-fencing.md) (capture under the fence), [0027](0027-restart-semantics.md) (deterministic re-derivation), [0031](0031-pure-computation-deps.md) (interpolation stays pure)

## Context

[0038](0038-invoke-and-local-reuse.md) gave a Library pipeline an explicit
`interface: { inputs, outputs }`, and the caller can already write
`${{ deploy.url }}` — but the reference is **inert**. At compile it is validated
against the library's declared `outputs`; at run time nothing replaces it with
the value the module produced. The payoff of an `outputs` interface — a module
computes a value (a deployed URL, an image digest, a version) and a downstream
step consumes it — does not yet exist.

The gap is bigger than `invoke`. Two things are missing system-wide:

1. **There is no consumable step result.** The only thing a step "produces"
   today is the content-addressed **workspace snapshot** ([0029](0029-workspace-cas.md)) —
   a merkle-root *hash*, used for input materialization and skip-if-unchanged.
   That is a hash, not a value: you cannot post it to Slack.
   [0008](0008-step-contract.md) *promised* a results channel (`/scarab/results/*.json`,
   an optional injected `scarab` CLI for structured emit) but nothing captures or
   persists it.

2. **There is no launch-time interpolation.** The trigger path keeps every step
   string literal on purpose ("a later slice"); matrix coordinates
   ([0023](0023-dag-shape.md)) are recorded but never substituted. So this ADR
   builds the **first** `${{ … }}` interpolation pass, and `invoke` outputs are
   its first consumer — the same rail later carries matrix/event interpolation.

The decision must not break the load-bearing invariants: interpolation stays
**pure** and **total** ([0031](0031-pure-computation-deps.md), [0009](0009-dsl-ir-yaml-cel.md)),
capture happens **under the fence** ([0021](0021-double-effect-fencing.md)), and
a restart **re-derives the same context** deterministically ([0027](0027-restart-semantics.md)).

## Decision

### 1. Named results are a first-class, per-step `name → JSON value` map

Separate from the workspace snapshot ([0029](0029-workspace-cas.md)): the
snapshot is *bytes in the workspace*, a result is a *small declared value*. A
step emits results through [0008](0008-step-contract.md)'s channel (its
`/scarab/results/*.json` — already JSON); the executor reads them back **before
teardown** and returns them alongside the exit state:

```rust
// scarab_engine::Executor
async fn results(&self, _handle: &ExecHandle) -> Result<BTreeMap<String, serde_json::Value>, ExecError> {
    Ok(BTreeMap::new()) // default: a backend that captures none need not implement it
}
```

The engine persists them **only on successful completion**, under the fence,
keyed `(run, step, name)` — a new `Db::set_step_results` / `step_results`,
recorded at the same point in the scheduler that already calls `set_step_output`
for the snapshot. Re-running a step overwrites its results deterministically
(same fence → same values); `serde_json::Value` serializes canonically, so
restart-hashing stays sound.

**Typed (JSON) values from v1, not strings.** The emit channel is already JSON,
so reading a result as a `serde_json::Value` is free — and typing is what makes
outputs *correct in decisions*, not just substitutable in strings:

- A `when:` / matrix `exclude:` guard over an output must compare with the right
  type — `outputs.test.coverage > 80` is a **numeric** comparison; as a string,
  `"80" > "9"` is lexicographic and silently **wrong** (the opposite of
  fail-fast, §5). Typed values make the guard mean what it reads.
- Structured access falls out: `outputs.plan.summary.cost`,
  `size(outputs.build.artifacts)`, a bool `outputs.scan.clean` — object/list/bool
  navigation instead of re-parsing strings in shell.
- When a typed value lands in a **string** interpolation (`command:`/`env:`), the
  existing `cel::render` stringifies it (`3` → `"3"`, a string verbatim).

Deep/opaque JSON is allowed but not encouraged; the sweet spot is scalars and
small structures. This avoids a guaranteed later string→typed migration.

### 2. `invoke` outputs are coupled to a library step (the confirmed model)

`interface: { outputs: [url] }` means **"expose the result named `url` emitted by
the library step `url`."** Output name = step id = result name — one word, no
mapping layer. This is exactly what [0038](0038-invoke-and-local-reuse.md)'s
compile check already enforces (an exposed output must be a real library step
id), now given runtime meaning. The compiler records, per invoke, the
exposed-name → **concrete inlined step** (`deploy` exposing `url` → inlined step
`deploy/url`), so at run time `outputs.deploy.url` resolves to the result `url`
of step `deploy/url`. A decoupled `iface-name → step.result` mapping was
considered and rejected for v1 (more syntax, no v1 payoff).

### 3. `${{ outputs.<id>.<name> }}` — one small top-level `outputs` namespace

The reference binding is a single top-level `outputs` map, keyed by producer id
then output name:

- **Module output:** `outputs.<invoke-id>.<exposed-name>` — `outputs.deploy.url`.
- **Plain step result:** `outputs.<step-id>.<result-name>` — identical shape, so
  a step's own emitted results are consumable the same way (this ADR specifies
  the invoke case; the plain-step case falls out of the same rail).

Rejected: `deploy.outputs.url` (producer id at the top level). It pollutes the
top-level CEL scope with arbitrary step ids, colliding with reserved bindings
(`event`, and the matrix coordinate variables [0023](0023-dag-shape.md) already
puts at top level). [0009](0009-dsl-ir-yaml-cel.md) wants a *small, deliberate*
binding; one `outputs` map is that. Non-output step facts (status, duration), if
ever needed, get their **own** small namespace rather than overloading this one.

### 4. Launch-time interpolation is pure, over `needs`-satisfied upstreams only

Just before launch, the engine builds a CEL context from the launching step's
**upstream results** and runs the total, pure
`scarab_pipeline::interpolate_spec(spec, &ctx) -> spec` over the interpolatable
surfaces (image, command, env). I/O stays at the edge: the engine reads the
persisted results and constructs `ctx`; `scarab-pipeline` stays pure
([0031](0031-pure-computation-deps.md)).

- **A reference is only legal to a step you `needs`.** `${{ outputs.build.x }}`
  in a step that does not depend on `build` is a **compile error** — this keeps
  the data-dependency identical to the DAG edge (no hidden edges), and
  *guarantees the value exists* by the time the step launches (all `needs` are
  `Succeeded` before a step is ready, [0033](0033-transitive-skip.md)). For an
  `invoke` reference the required edge is `needs: [<invoke-id>]`, which the exit
  seam already rewrites onto the module's leaves.
- **Determinism:** the context is re-derived from persisted results on a
  re-launch, so interpolation yields the same bytes — restart-safe
  ([0027](0027-restart-semantics.md)).

### 5. Fail-closed and fail-fast

No output error ever renders empty or degrades silently — it fails the dependent
step, loudly, consistent with the fail-closed posture of
[0037](0037-environment-governance.md)/[0039](0039-privileged-images.md):

- **Missing result:** a step declared/exposed an output but did not emit it
  (succeeded, no `results/<name>`) → a dependent's `${{ outputs.… }}` reference
  **fails the dependent step**. (An *unreferenced* missing result is harmless.)
- **Type mismatch in a guard:** a CEL guard that compares an output against the
  wrong type (`number > string`) **errors at eval and fails the step** rather
  than falling back to a lexicographic comparison — the whole reason results
  carry types (§1).
- **Undeclared reference** stays a **compile** error
  ([0038](0038-invoke-and-local-reuse.md)): you cannot reference an output a
  library does not expose, or an output of a step you do not `needs` (§4).

The progression is deliberate: reject at compile what is knowable at compile;
fail the step at run time for what is only knowable then; never silently
continue with a wrong or empty value.

## Consequences

- **The first interpolation rail exists.** Matrix-coordinate and event
  interpolation ride the same `interpolate_spec` pass in later slices — this ADR
  builds the mechanism, `invoke` outputs are merely its first consumer.
- **A small, typed (JSON) results model** distinct from the workspace snapshot:
  values for control-flow/interpolation, not bytes — so guards over outputs are
  type-correct, not lexicographic.
- **[0038](0038-invoke-and-local-reuse.md)'s `outputs` become live** — the module
  boundary now transports values, not just a compile-checked promise.
- **New surface:** `Executor::results`, `Db::set_step_results`/`step_results`,
  the pure `interpolate_spec`, the `outputs.<id>.<name>` CEL binding, and the
  compile-time "reference only what you `needs`" check + exposed-name→inlined-step
  record in the IR (self-describing, [0022](0022-upgrades-and-versioning.md)).
- **Matrixed-invoke output references are deferred.** `outputs.<invoke-id>.<name>`
  is ambiguous when the invoke fans out over a matrix ([0038](0038-invoke-and-local-reuse.md)
  slice 4) — which copy? A reference to an output of a matrixed invoke is a
  compile error in v1; per-coordinate referencing needs a coordinate-qualified
  syntax, a later slice.

## Alternatives considered

- **`deploy.outputs.url` (producer id at top level)** — rejected: pollutes the
  top-level CEL scope and collides with reserved/`matrix` bindings.
- **Reuse the workspace snapshot as the value** — rejected: it is a merkle hash,
  not a consumable value; consumers would have to re-read files at use time,
  which is what named results exist to avoid.
- **Decoupled `interface-name → step.result` mapping** — rejected for v1: an
  extra mapping layer and more syntax with no v1 payoff; the coupled
  name=step=result model is the smallest thing and matches
  [0038](0038-invoke-and-local-reuse.md)'s existing compile check.
- **Strings-only results** — rejected: forces a lexicographic footgun on any
  guard over an output (`"80" > "9"` is silently false) and guarantees a later
  string→typed migration. Since the emit channel is already JSON, typed values
  cost almost nothing now (§1).
- **Interpolate at compile time** — impossible: result values are only known at
  run time. Compile validates the *reference*; launch binds the *value*.
- **Hidden data edges** (let a reference create the dependency implicitly) —
  rejected: a reference must ride an explicit `needs` edge, keeping the DAG the
  single source of ordering truth and guaranteeing availability.
