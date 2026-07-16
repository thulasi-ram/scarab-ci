# 0043. Launch parameters + manual dispatch as a repo-aware trigger

- **Status:** Accepted
- **Date:** 2026-07-16
- **Deciders:** thulasi.ram (architect)
- **Refines:** [0038](0038-invoke-and-local-reuse.md) (makes `interface.inputs` a *launch* contract, not only a reuse contract), [0041](0041-named-results-and-interpolation.md) (adds the `inputs` binding to the interpolation rail), [0009](0009-dsl-ir-yaml-cel.md) (typed IR + CEL), [0010](0010-forge-integration.md) (read-at-ref discovery)
- **Relates:** [0024](0024-environments.md)/[0037](0037-environment-governance.md) (a manual dispatch of a deploy still hits admission + protection rules), [0022](0022-upgrades-and-versioning.md) (self-describing Run records the resolved SHA + params), [0028](0028-ui-stack.md) (the Run-Pipeline flow), [0014](0014-secrets.md) (params are *not* the secret rail)

## Context

A user selecting a pipeline in the UI's Run-Pipeline flow wants to see the
**mandatory and optional parameters that pipeline dictates** — the thing
Woodpecker lacks (its manual runs are an untyped env-var soup, and it is hard to
tell *which* manual request drives *which* pipeline). Today Scarab cannot show
this because a top-level pipeline has **no way to declare launch parameters**:

- `Event::Manual { actor }` / `Event::Api { actor }` carry **no payload** beyond
  the actor; `trigger_run_from_event` treats `manual`/`api`/`cron` as repo-less
  and returns nothing for them.
- `POST /v1/runs` takes a **full inline pipeline IR**, is **repo-less**, and is
  **baseline-only** — it bypasses Environment, secrets, and admission entirely.
- The only declared-parameter concept, `PipelineIr::interface.inputs`
  ([0038](0038-invoke-and-local-reuse.md)), is scoped to `invoke` and documented
  as *"irrelevant on a top-level triggered pipeline"*. It is also untyped
  (`Vec<String>` names; `with:` is `BTreeMap<String,String>`) and all-required.

So the ask is not "add a form" — it is a domain gap at the IR/launch layer. Two
sub-problems: (1) a pipeline must be able to **declare typed parameters** with
mandatory/optional distinction; (2) the manual/API launch path must become
**repo+ref-aware** so a run created from a button carries the same governance a
webhook-triggered run does.

## Decision

### 1. One declaration: `interface.inputs` becomes the launch contract too

We **unify** rather than add a parallel `params:`/`dispatch:` block. A pipeline's
`interface.inputs` is the single declaration of the parameters it requires,
serving **both** an `invoke:` caller's `with:` **and** a `manual`/`api` launcher.
One declaration, one env rail (`SCARAB_PARAM_<NAME>`), one CEL binding
(`${{ inputs.<name> }}`). This reverses 0038's "irrelevant at top level" note.

The user-facing term is **Parameter** (see [CONTEXT.md](../../CONTEXT.md) §4.2).
The word "inputs" stays overloaded in the IR for continuity, but docs/UI say
*parameter* to keep it distinct from the per-Step **workspace** `inputs:`
([0007](0007-data-passing-model.md)) — a different concept sharing the word.

### 2. Parameters are typed, from a closed vocabulary

`interface.inputs` grows from `Vec<String>` (names) to a list of **param specs**:

```
{ name, type, required, default?, options?, validate?, description? }
```

- **`type` ∈ { `string`, `boolean`, `number`, `choice` }** — a *closed* vocabulary,
  chosen so each maps to exactly one bounded UI widget (text / checkbox / numeric /
  dropdown). `choice` carries an `options:` list. **No `dict`** (arbitrary-form tar
  pit, untyped values, no `SCARAB_PARAM` encoding — rejected permanently for launch
  params). **No `list`** in v1; the first future extension is `list<choice>`
  (bounded multi-select), added only when a concrete non-matrix consumer appears.
- **`required` is a static boolean.** Mandatory-vs-optional must be knowable from
  the compiled IR alone with **zero evaluation**, or the UI cannot render it
  deterministically. Dynamic/CEL-computed requiredness is explicitly out.
- **`required: true` ⇒ no `default`** (launcher must supply).
  **`required: false` ⇒ `default` is mandatory.** Consequence: **every declared
  parameter always resolves to a value**, so `${{ inputs.<name> }}` is *total* —
  there is no missing-parameter runtime failure mode (unlike 0041 outputs).
- **`validate?`** is an optional per-param CEL predicate the supplied value must
  satisfy, evaluated server-side at launch, pure/total ([0031](0031-pure-computation-deps.md)),
  fail-closed. `description?`/order are UI-serving metadata.

### 3. Typed interface, string-authored `with:`, one shared `coerce`

`with:` stays `BTreeMap<String,String>` (zero IR churn; `${{ … }}` is naturally a
string). Coercion is the shared primitive: every supplier produces a *raw* value —
an already-typed JSON scalar (UI), or a string (CLI `--param k=v`, a rendered
`${{ }}`, or `with:`) — and one pure function turns it into the declared type or
fails closed:

```
scarab_pipeline::coerce(raw: serde_json::Value, ty: ParamType) -> Result<TypedValue>
```

`"3"`+`number`→`3`; `true`/`"yes"`+`boolean`→`true`; value ∉ `options`+`choice`→error;
then the `validate:` CEL runs. Three call sites (UI JSON, CLI strings, `with:`
strings), one path, all fail-closed.

### 4. Manual/API dispatch becomes a repo+ref-aware trigger (World B)

The Run-Pipeline flow rides the **read-at-ref/compile/admission** machinery the
webhook path already has (`list_dir_at_ref` → `read_file_at_ref` → transitive lib
pre-fetch → pure compile), **not** the inline-IR `POST /v1/runs` shortcut.

- **Opt-in = `on: manual`** (the `workflow_dispatch` analogue). A pipeline appears
  in the human catalog **iff** its `on:` includes `manual`; `on: api` is the
  programmatic sibling supplying the same declared parameters. This makes the
  pipeline→trigger mapping explicit and discoverable — the exact ambiguity the
  Woodpecker/inline model (World A) cannot resolve.
- **Ref-scoped + resolve-to-SHA.** The flow is repo → ref → catalog → pipeline →
  params → dispatch, one ref threading through. Describe **resolves `ref` → a
  concrete commit SHA** and returns it; dispatch runs against that **SHA**, so the
  form and the run see byte-identical config (no branch-moved skew) and the Run is
  reproducible + self-describing ([0022](0022-upgrades-and-versioning.md)).
- **API surface** (UI and CLI share it — invariant #5, one validator):
  - `GET /v1/repos/:id/pipelines?ref=` → lightweight catalog (name, `manual?`,
    resolved SHA). *Two-call* design: no full compile just to list.
  - `GET /v1/repos/:id/pipelines/:name/interface?ref=` → the **compiled** param
    specs for the selected pipeline (compile-for-interface only on selection).
  - Dispatch `{ repo, ref/sha, pipeline, params }` → server re-reads at SHA,
    compiles, `coerce`+`validate` fail-closed, binds, creates the run.
- Inline `POST /v1/runs` **remains** the ad-hoc/dogfood escape hatch
  (baseline-only, no params UI, no Environment).

### 5. Parameters are a frozen run-level constant on the 0041 rail

The resolved `name → typed value` map is written **onto the Run at creation**,
immutable for its life (a `params` JSON blob on the run record — a run-level
constant, *not* `step_results`).

- **Binding:** `inputs` joins the **same** CEL context 0041 assembles for
  `outputs.*`, so `${{ inputs.region }}` in `image`/`command`/`env`/`when:` flows
  through the existing `interpolate_spec`; typed values make guards type-correct.
  Because params are known at **creation** (not per-step), `inputs` is in scope
  **run-wide from creation** — including run-level CEL like
  `concurrency.group: ${{ inputs.env }}` — whereas `outputs` joins per-step at
  launch. Same rail, wider scope. Unreferenced params still reach steps as
  `SCARAB_PARAM_<NAME>` env.
- **Restart determinism is free:** params are resolved once and frozen, so a
  restart (new Attempt) re-reads the identical map → byte-identical interpolation.
  Strictly simpler than 0041 outputs (which re-derive).

### 6. Fail-closed and safety red lines

- **Parameters are never secret.** A param is persisted in cleartext, visible in
  run detail / audit / time-travel, injected as plain env. Secrets stay on the
  `secrets:`/SecretProvider rail ([0014](0014-secrets.md)/[0037](0037-environment-governance.md)).
  No `type: secret`, no masked param in v1.
- **Manual dispatch is a *trigger*, never *authority*.** A dispatched deploy
  (`environment:` pipeline) hits admission → the Environment's protection rules
  (approvers, wait timer, **allowed-refs**, concurrency) identically to a
  webhook deploy: params supplied ≠ approval granted; the dispatch **ref must
  satisfy allowed-refs**, fail-closed. Every gate guarding an automatic deploy
  guards a manual one.
- **Client validation is static-only.** The UI mirrors `required`/`type`/`options`
  for instant feedback; the `validate:` CEL runs **server-side at dispatch only**
  (never a JS CEL evaluator — that is an impurity/backchannel smell). CEL failure
  returns as a structured per-param error the form renders.
- **Re-run prefill:** a re-run reads the prior Run's frozen params and pre-fills
  the form, then resolves a fresh SHA and re-validates against the *current*
  interface — a since-changed param surfaces as a form diff, fail-closed.

## Consequences

- **The UI can finally show mandatory/optional params** — the headline ask —
  because the compiled IR now carries a typed, statically-classifiable parameter
  declaration served by a ref-scoped describe endpoint.
- **Manual/API launch is unified with the trigger path**, so deploys dispatched
  from a button inherit Environment governance for free; `manual` graduates from
  an enum variant to a first-class trigger.
- **One parameter model** spans invoke-reuse and launch — fewer concepts, one
  `coerce`, one env rail, one CEL binding.
- **New surface:** typed `interface.inputs` (expand-only, version-stamped IR
  change — [0022](0022-upgrades-and-versioning.md)); the pure `coerce` +
  `validate` path; `inputs` in the 0041 CEL context (run-wide); repo-aware
  `manual`/`api` dispatch reusing the webhook machinery; catalog + interface +
  dispatch endpoints; a `params` blob on the Run; CLI `scarab run … --param k=v`.
- **`with:` unchanged for authors** — typing is additive; existing libraries keep
  their string `with:` and gain coercion.

## Deferred / future (doors left open)

- **`list<choice>` multi-select** — the first type-vocabulary extension, added
  when a concrete non-matrix consumer appears (matrix-from-param hits the
  deferred dynamic-graph-shape wall of [0023](0023-dag-shape.md)/[0038](0038-invoke-and-local-reuse.md)).
- **Privileged dispatch that carries *authority*** — a future "dispatch that may
  bypass/short-circuit a gate" is intentionally *not* precluded by this design;
  v1 dispatch is trigger-only, and gate-bypass authority is a separate ADR to
  brainstorm when demand appears.
- **Dynamic requiredness / conditional params** (a param mandatory only when
  another holds) — deferred; would break static describe-time classification.
- **Secret-typed / masked params** — deferred; use the secrets rail.

## Alternatives considered

- **A separate top-level `params:`/`dispatch:` block** (GHA keeps
  `workflow_dispatch.inputs` distinct from reusable-workflow inputs) — rejected:
  a parallel declaration, a second validator, a second env convention, for no v1
  payoff. Unification is more consistent with the wedge.
- **World A — inline-IR `POST /v1/runs` + client-side interface parsing** — the
  Woodpecker model. Rejected: no catalog (server doesn't know what exists at a
  ref), interface-parsing duplicated in JS (dances around invariant #5), and —
  fatally — manual runs stay repo-less, so a *deploy* dispatched manually gets
  **no Environment/secrets/admission**. Manual dispatch's headline use case is
  exactly a deploy.
- **String-only parameters** — rejected: reduces the UI to naked text boxes (the
  Woodpecker soup we are escaping) and reintroduces 0041's lexicographic footgun
  in guards.
- **`dict` / free `list` params** — rejected (see §2): unbounded widget, untyped
  values, no env encoding.
- **Branch-ref dispatch without SHA resolution** — rejected in favour of
  resolve-to-SHA: cheap, kills the form/run skew class, and makes the Run
  reproducible.
- **Retyping `with:` to JSON** — rejected: fights YAML (`${{ }}` is a string) and
  churns the IR for no gain once `coerce` exists.
