# 0075. One policy engine for every governance rule: typed settings and CEL compile to a Policy IR

- **Status:** Proposed
- **Date:** 2026-10-28 (simplified 2026-10-29, see the end)
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0009](0009-dsl-ir-yaml-cel.md) (YAML frontend, typed IR, CEL),
  [0037](0037-environment-governance.md) (admission at the point of use),
  [0071](0071-org-level-environments.md) (org Environments, tighten-only overlays),
  [0073](0073-step-up-confirmation.md) (step-up, loosening rules)
- **Relates:** [0039](0039-privileged-images.md) (grants), [0055](0055-placement-profiles.md)
  (`k8s_overlay`, placement), [0038](0038-invoke-and-local-reuse.md) (`invoke`),
  [0074](0074-deployments-leave-core.md) (`deploy: true`), [0049](0049-identity-and-access.md)
  (RBAC), [0056](0056-run-takes-and-attempt-grain-evidence.md) (Takes, Attempts),
  [0069](0069-identity-links.md) (forge actor to Principal),
  [0076](0076-libraries-and-plugins-through-invoke.md) (libraries, plugin gates, `origin`)

## Context

Scarab's governance rules are written as code, one feature at a time:

- Run admission against an Environment: eligibility, allowed refs, approvers
  (`admit_and_create_run`, `crates/scarab-server/src/lib.rs`; `EffectiveProtection`,
  `crates/scarab-project/src/environment_governance.rs`).
- Step privileges: grants against a per-Project digest whitelist and the `k8s_overlay`
  permission, checked for every Step at Run creation (`admit_step_grants` and `admit_k8s_overlay`,
  `lib.rs:4919`, `4967`).
- Re-admission at each launch: the Run's Environment rules are checked again before a
  Step's Pod starts (`ensure_still_admitted`, `crates/scarab-server/src/secret_executor.rs:88-118`).
- Gates: who may approve, release or reject; allowed refs re-checked when a manual gate releases
  (`admits()` in `approve_gate`, `lib.rs:7481`); a step-up to approve where the
  Environment names approvers (`lib.rs:7384-7423`); `gate_release_governance`.
- Fork pull requests: no secrets, OIDC subject `env=none` (`fork_policy`), no build push
  (`lib.rs:5283`), a read-only clone token (`lib.rs:5222`).
- Settings changes that loosen governance need a step-up (`crates/scarab-server/src/step_up/`).

Each rule has its own data model, its own UI and its own error text. None can be tried
before it is enforced, none can be scoped to particular repositories unless the feature
added that by hand, and an org cannot add a rule Scarab did not anticipate. The next
features need more of the same: which libraries and plugins a repository may `invoke`,
per-repository allows and blocks at the org level, and requirements such as "a production
deploy that changes a migration needs a second approval".

The owner's direction (2026-10-28): one policy engine that covers every governance rule,
at the org level and on Environments; rules can deny and can also require things; typed
settings and CEL must work together, with an IR if needed.

Two facts shaped the design:

- **CEL is already Scarab's expression language** (ADR-0009), and Kubernetes'
  ValidatingAdmissionPolicy shows the pattern this needs: a cheap match selector, CEL
  rules, a message on denial, an audit mode.
- **The Rust CEL crate has no type checker.** Scarab uses `cel-interpreter` 0.10. Its
  successor, `cel` 0.15 (github.com/cel-rust/cel-rust), adds typed declarations (`Env`,
  `FunctionDecl`, `TypeRegistry`), but its conformance runner states that type checking is
  not available. `cel-cxx` binds Google's cel-cpp, which has a checker, but brings a C++
  and protobuf build into the control plane; the owner rejected it.

## Decision

### 1. One evaluator, two frontends, one Policy IR

```
typed settings (forms, API) ──┐
                              ├──► Policy IR ──► evaluator ──► PolicyDecision, recorded
CEL rules (YAML, API)    ─────┘    (typed)       at a PolicyCheckpoint
```

This mirrors Pipelines, where YAML compiles to a typed IR (ADR-0009). Both frontends
compile to the same **Policy IR**, and one evaluator runs it. Typed settings compile to
typed predicates, not to CEL text, so a form can still show and edit them. CEL covers
what a form cannot express.

Every governance rule in the Context list moves into the engine, except the
PolicyInvariants (§7), which stay in code. RBAC stays where it is (§8). Settings changes
are not decided by Policies; the step-up route table governs them (§10).

### 2. Three PolicyCheckpoints with typed inputs

A **PolicyCheckpoint** is a place where Scarab consults the engine. The set is closed and
grows only with a Scarab release. Each checkpoint publishes a versioned, typed input
schema, generated from the Rust types that build the input, so the schema cannot drift
from what is passed. Adding a field is compatible; removing or retyping one bumps the
schema version.

| PolicyCheckpoint | When | Input (abridged) |
|---|---|---|
| `run.create` | Run creation, after compiling and trigger matching, before any Step row | `org`, `project`, `trigger` (kind, optional symbolic `ref`, `sha`, `fork`, forge actor, optional `principal` linked to that actor, optional `reason` of a manual or API dispatch), `pipeline` (name, `environment`, `deploy`), `steps` (every Step's static `step.admit` input, with its `origin`, whether it uses the Environment's authority (§5), and the gates upstream of it: id and kind, transitive, after invoke inlining and matrix expansion), `gates` (each gate's id, kind, `origin`, and the Steps that depend on it), `principal`, `time` |
| `step.admit` | every launch of an Attempt (§3) | `step` (image, digest, grants, `k8s_overlay`, secrets, placement, `deploy`, `forge_token`, `origin`: the chain of invoke targets it was inlined from, each with kind, name, source and content hash), the approval of each manual gate upstream of it (when released, by whom, the approver set and PolicySetDigest at release), `run`, `environment`, `time` |
| `gate.resolve` | approve, reject, release | `gate` (id, kind, `origin`, the Steps that depend on it), `verdict` (`release` or `reject`), `actor` (kind: `human` in a browser session, `human_token` for a human through an API token or a CLI, `plugin` or `external`; optional `principal`), optional `reason`, `approvals` in the current wait, `run` (with its trigger and the trigger's `principal`), `time` |

`time` is part of the input and is recorded with the decision, so a time-window rule
decides the same way on replay.

**Library and plugin rules are written at `run.create`.** The compiler records an
`origin` on every inlined Step and gate (ADR-0076 §1), so `run.create` sees every
library and plugin gate a Run uses, together with the trigger and its ref. There is no
checkpoint at compile time. On the server, compiling and creating the Run happen in the
same request (`trigger_runs_from_event`, `lib.rs:4211` then `4235`), so an earlier
checkpoint would not reach the author any sooner. It would also run for Pipelines whose
trigger does not match, since the webhook path compiles every Pipeline file before
matching, and a deny there would abort the whole delivery.

**Early feedback comes from a dry run.** `scarab validate --server` builds the
`run.create` input from the local file and a supplied trigger (by default a push to the
current branch; `--ref` and `--event` choose another) and evaluates it through the policy
test API (§9), without enforcing. The checker knows which fields each rule reads. A rule
that reads only the Project, the Pipeline, the Steps and their `origin` gets a definite
answer; a rule that reads the trigger is answered for the supplied trigger and labelled
that way. A check on the forge pull request is not planned.

### 3. `step.admit` is decided once per Attempt, at launch

`step.admit` replaces `ensure_still_admitted`, which today runs only for
Environment-governed Runs (`secret_executor.rs:88-118`); `step.admit` runs for every
launch. It is decided once per Attempt, including automatic retries, which go from Running
to Ready without passing promotion (`rearm_step`, `crates/scarab-engine/src/scheduler.rs:4318`).

The executor makes the decision before it uses any credential or creates the Pod, and
records it with insert-if-absent, keyed by *(run, step, attempt)*. The first write wins.
A redelivered launch finds the recorded decision and uses it, so a crash between the
decision and the Pod cannot produce a second decision under a newer policy. The engine's
claim commit does not change and does not read the policy store. If the policy store
cannot be read, the launch fails for that tick and is retried; a Step is never admitted
without a decision.

**An approval is checked again at launch.** An Environment *has approvers* when its
effective approvers setting (the baseline and the Project's overlay) is not empty at the
time of the check; an org Policy with `require_approval` does not count. In such an
Environment, a Step that uses the Environment's authority (§5) is refused at
`step.admit` when:

- it has no manual gate upstream, which happens to a Run created before the approvers
  were added;
- the Run's symbolic ref no longer matches the Environment's allowed refs;
- for any manual gate upstream of it, the approver set, or the digest of the enforced
  `gate.resolve` Policies that matched that gate, differs from what was recorded when the
  gate released;
- any of those releases is older than the Environment's maximum approval age (24 hours by
  default; an Environment can raise it, lower it or set none). This applies to every
  launch, including a first launch that waited a day in the queue.

The refusal says "rerun the gate", or "start a new Run" when there is no gate. This is what makes a Rerun safe: rerunning a Step
downstream of a released gate does not ask again, as today, so without these checks a
member could rerun the deploy Step of a six-month-old Run on its original approval
(`step.rerun` needs no step-up, `step_up/ops.rs:586`). Rerunning the gate asks again.

**The OIDC subject follows the gate.** When the OIDC issuer is configured, every Step of
an Environment-governed Run gets an OIDC token, as today. In an Environment with approvers, a Step with no released manual
gate upstream gets the subject fork pull requests get, `env=none`, which no trust policy
for a real Environment matches (`secret_executor.rs:154-161`). Steps after the gate get
the subject with the Environment in it. Authors write nothing for this, and existing
trust policies keep working. The Step's log says why its subject is `env=none`. This is a
PolicyInvariant (§7) and has no `audit` period; the release notes list it as a breaking
change.

`run.create` also evaluates every Step's static `step.admit` input as a preflight. A grant
or `k8s_overlay` that would be denied refuses the whole Run before any row exists. Today
the webhook and dispatch paths check grants inside the per-Step loop of
`persist_run_from_ir`, after the Run row is created (`lib.rs:5022`, `5252`, `5311`), so a
refusal leaves a partial Run; the preflight closes that gap. The launch-time check still runs, so a policy tightened after
Run creation applies to Steps not yet launched.

### 4. A Policy

```yaml
kind: policy
name: payments-prod-libraries
checkpoint: run.create
match:                                  # typed selector, evaluated before any rule
  projects: ["payments-*"]
  except_projects: [payments-sandbox]
  environments: [production]
mode: enforce                           # or `audit`
rules:
  - when: >
      steps.exists(s, s.origin.exists(o,
        o.kind == "library" &&
        !(o.source.startsWith("github.com/thulasi-ram/scarab-ci/") ||
          o.source in ["github.com/acme/scarab-libs//deploy-helm.yaml"])))
    effect: deny
    message: "This Run uses a library that is not approved for payments in production."
```

- `match` is typed: glob lists over projects and environments, typed exceptions, trigger
  kinds, and at `gate.resolve` actor kinds. Most policies never reach CEL. The typed
  setting "libraries allowed here" writes the rule above without CEL.
- A rule is `when` (a CEL boolean over the checkpoint's input), an effect and a static
  `message`. There is no message expression and no templating: the PolicyDecision stores
  the input (§9), so the Run page shows what the rule matched.
- A Policy names exactly one checkpoint. A rule that reads a field its checkpoint does not
  have is rejected when the Policy is saved (§6). A requirement that needs two checkpoints
  is written as two Policies; a typed setting can compile into both.

### 5. PolicyEffects: deny and two requirements

| PolicyEffect | Meaning |
|---|---|
| `deny` | refuse, with the message |
| `require_approval` | named Principals must approve (`principals`, `count`) |
| `require_step_up` | the acting human confirms with a passkey (ADR-0073) |

There is no `allow` effect: a decision is allowed when nothing denies it. Allow lists are
written as "deny unless listed", and per-repository allows as exceptions in the org's own
Policy (§7). Policies never change a Pipeline or a Step: there is no mutating effect, and
no Policy adds a gate or holds a Step.

A missing reason is a `deny` over the `reason` field, which is what the code does today
(`TriggerError::ReasonRequired`, `lib.rs:4552`). A `require_reason` effect can be added
later if the UI should ask for a reason instead of refusing; Policies written as a deny
keep working.

Each checkpoint allows only some effects. The checker rejects any other when the Policy is
saved.

| Checkpoint | Who acts | `deny` | `require_approval` | `require_step_up` |
|---|---|---|---|---|
| `run.create` | no one (a webhook), or a human or token (manual and API dispatch) | yes | no | no |
| `step.admit` | no one: the scheduler launches it | yes | no | no |
| `gate.resolve` | a human session, an API token, a PluginPrincipal, or an external token with no Principal | yes | yes | yes |

`run.create` offers no step-up: creating a Run is not a step-up operation
(`step_up/ops.rs:576`), and a StepUpConfirmationRequest replays its request, which would
decide twice.

**Approval happens on gates the author wrote.** `require_approval` adds Principals to the
release quorum of a `manual` gate in the Pipeline; the checker rejects it on any other
gate kind, since approvals are recorded only on manual gates (`scheduler.rs:1514`). To
require approval, an org writes two rules: a `run.create` deny that refuses a Run in
which a Step uses the Environment's authority with no manual gate upstream (its message
names the Step and the YAML to add), and a `gate.resolve` `require_approval` on the
Run's manual gates. The deny alone is not enough, because a plain manual gate releases on
the first approval by anyone with write access (`lib.rs:7482`). Every gate stays visible
in the YAML, and reruns, expiry and the DAG view need no special cases. An approval
imposed by Policy that is not a gate would be a separate mechanism, not designed here.

**A Step uses the Environment's authority** when it receives something only because the
Environment governs the Run:

- it declares `secrets:` (a Run with no Environment resolves no secrets,
  `secret_executor.rs:228-237`, so every declared secret counts);
- it requests a governed grant (`privileged`, `add-capabilities`) or a `k8s_overlay`;
- it has `forge_token: write` (ADR-0076 §9);
- it is marked `deploy: true`, or its Pipeline is, which marks every non-gate Step for
  this rule.

Steps that use none of these, such as build and test, run before the gate, unless the
Pipeline itself is marked `deploy: true`, which puts every Step behind it. The OIDC
token is not on the list; its subject follows the gate instead (§3). The rule relies on
today's join, where a Step runs only when every one of its `needs` succeeded and a
skipped gate skips its dependents (`scheduler.rs:2295-2300`). A richer join rule would
have to keep that true for gates.

**Requirements apply to releases only.** A reject is never held: no `require_approval` or
`require_step_up` applies to a `reject` verdict, so a gate someone is trying to fail is
never kept open by a Policy.

**A requirement the actor cannot meet refuses that actor.** A `human` steps up with a
passkey and a `human_token` through a StepUpConfirmationRequest. A `plugin` and an
`external` token have no human, so a `require_step_up` on their release refuses it, with
a message naming who can release the gate. A Policy that should apply to people only says
so in `match` (`actors: [human, human_token]`); the approvers typed setting does (§12).

One approval counts toward every requirement that names its Principal.

**Separation of duties is a Policy, not a Scarab rule.** Scarab adds no built-in rule
such as "the person who triggered a Run may not approve it". The `gate.resolve` input
carries the acting `principal` and the Run's trigger actor, resolved to a Principal
through identity links (ADR-0069) when one exists, so an org writes the rule itself:

```yaml
checkpoint: gate.resolve
rules:
  - when: has(actor.principal) && has(run.trigger.principal) && actor.principal == run.trigger.principal
    effect: deny
    message: "You started this Run, so someone else must approve it."
```

Groups and roles as approvers wait for a group concept in identity (ADR-0049 has none
today).

### 6. CEL is type-checked, over a subset

Scarab moves from `cel-interpreter` 0.10 to `cel` 0.15 for parsing, the AST, declarations
and evaluation, and adds a **type-checking pass** over the 0.15 AST:

- Each checkpoint's schema is declared into the CEL `Env` as typed variables and struct
  types.
- The subset: field access, `has()` on a field (`has(run.trigger.ref)`; an optional
  top-level variable is tested with `!= null`), literals, comparisons, `&&`, `||`, `!`,
  `?:`, `in` on lists and maps, string functions (`startsWith`, `endsWith`, `contains`,
  `matches`), `size`, timestamp and duration comparison, and the `exists`, `all` and
  `exists_one` macros over typed lists. Anything else is rejected with "not supported in
  policies". Arithmetic, string concatenation, indexing, conversions, map iteration and
  time-of-day functions (`getHours()`) are outside the subset, so a time-window rule
  compares absolute timestamps; daily windows need a later addition.
- `matches` takes a literal pattern only; it is compiled when the Policy is saved.
- The checker computes a **static cost bound** from macro nesting and the list-size caps
  each schema declares, and rejects a rule above the limit. `cel` 0.15 has parse limits
  but no runtime cost tracking.
- A rule that type-checks cannot raise a type error. With arithmetic and indexing outside
  the subset, the evaluation error left is reading an optional field that is absent,
  without `has()`. Any evaluation error denies, with the error in the decision
  (fail-closed). The editor warns on optional fields read without `has()`, since a deny
  rule on `trigger.ref` would otherwise deny every Run that has no ref.

Pipelines' `when:` and `${{ }}` move to `cel` 0.15 too, and stay unchecked until the
checker covers their contexts. The spike found 11 behaviour changes in 90 expressions run
through both versions: `list[5]` and `map['missing']` raise an error where 0.10 returned
null; `size()` on a string counts code points; `max()` is gone; `string(bool)`, `type()`
and `dyn()` exist; and an error on one side of `||` is absorbed when the other side is
true, as the spec says. The upgrade release notes list them.

**Parser depth.** A deeply nested or very long left-recursive expression overflows the
stack inside the CEL parser, which aborts the process; `catch_unwind` cannot catch it.
With 0.10 about 1,000 nested parentheses abort a release build; 0.15 stops nesting at
depth 96 with a parse error, but a left-recursive chain of about 5,000 links (about
10 KB) still aborts. Policy rules are capped at 4 KiB and 64 levels. Pipelines' `when:`
and `${{ }}` get the same caps, and all CEL parsing runs on a thread with a large
stack.

The checker is offered upstream to cel-rust, whose conformance suite has `check_only`
cases waiting for one. If upstream ships its own, Scarab switches to it.

### 7. Layers, combination, invariants and defaults

Policies attach at three levels:

| Level | Written by | May match |
|---|---|---|
| Org | org admin | any Project, Environment |
| Environment | that Environment's admins | Runs it governs |
| Project | that Project's admins, **typed settings only** | its own Runs |

- **Project admins write typed settings, not CEL.** Where a typed setting exists for a
  rule, it is the only way to write that rule at the Project level. CEL at the Project
  level is added one use case at a time, by a later decision. Org and Environment admins
  write both.
- **Combination.** Every Policy whose checkpoint and `match` fit is evaluated. Any `deny`
  wins, and every deny message is reported. Requirements add up. No layer can remove
  what another adds, so a lower layer can only tighten, as Environment overlays do today
  (ADR-0071 §3).
- **Per-repository allows** are exceptions inside the Policy that denies:
  `except_projects` in `match`, typed. A Project cannot exempt itself from an org Policy.
- **PolicyInvariants** are rules in code that no Policy can turn off, and the engine runs
  after them:
  - the hardened Step baseline (ADR-0039);
  - fork pull requests: no secrets, an `env=none` OIDC subject, no build push, a
    read-only clone token;
  - step-up for settings changes that loosen governance, including editing or deleting a
    Policy (§10), through the route table (ADR-0073);
  - a yanked LibraryVersion and the forge-credential rules (ADR-0076 §6 and §9);
  - the `env=none` OIDC subject for Steps with no released manual gate upstream, in an
    Environment with approvers (§3).
- **Defaults are typed-setting defaults.** Scarab ships no Policies of its own. Each
  typed setting states its default, and no default allows more than Scarab does today:
  no plugin's gates may run until an org admin lists the plugin (ADR-0076 §7); every
  installed library may be used until a list narrows it, because installing it was the
  approval (ADR-0076 §4); `forge_token: write` is denied unless the Run is
  Environment-governed and the Environment restricts refs (ADR-0076 §9). A list left
  unset at a lower layer adds no rule, so an Environment or Project that never sets
  "plugins whose gates may run here" does not deny a plugin the org listed. A default
  changes only with a Scarab release.

### 8. RBAC stays separate

A role (ADR-0049) decides whether a Principal may call an API, and is checked first.
Policy then decides what that call may do: a Policy can deny or require on top of a role,
and never grants what the role does not.

### 9. Decisions are recorded, and use the current policies

- Each consultation produces a **PolicyDecision**: the checkpoint, the **input itself with
  its schema version**, the **PolicySetDigest** (a content hash of every Policy evaluated,
  across the three layers), every matching Policy with its outcome, messages and
  requirements. A Run's decisions are stored with the Run and shown on the Run page. A
  `run.create` that refuses a Run stores its decision in the org's decision log, since
  there is no Run.
- A checkpoint uses the policies current when it is consulted, once (§3). A Policy added
  or tightened mid-Run applies to Steps not yet launched.
- A Rerun recompiles nothing: inlined Steps keep their `origin`, so `step.admit` can deny
  a Step whose library was yanked or blocked after the Run started.
- `audit` mode evaluates and records "would deny" or "would require" without enforcing.
- A policy test API evaluates a Policy against a stored PolicyDecision's input or a
  supplied one, without enforcing and without recording. `scarab validate --server` uses
  it (§2).

### 10. Changing a policy is governed by the route table

Saving a Policy is a settings change, classified in the step-up route table (ADR-0073)
like every other mutating route. It is not a Policy decision, and no Policy can change
these classifications:

- Adding a Policy, or moving one from `audit` to `enforce`, tightens: no step-up.
- Editing a Policy that is in `audit` mode needs no step-up, since it enforces nothing.
- Any other change (editing an enforced rule, deleting a Policy, moving to `audit`) counts
  as loosening and needs a step-up. Whether an edited CEL rule is looser cannot be
  decided in general, so the check fails closed, as ADR-0073 does.
- A plugin's kill switch tightens and never needs a step-up.

Orgs cannot write their own rules over settings changes. Restricting who may edit one
Environment belongs to RBAC ("Administer on that Environment", not built), not to
Policies.

### 11. Policies are stored in the database, versioned and diffable

Policies, and the typed settings that compile into Policy IR, live in Scarab's database,
not as files in a repository. Every save creates a new **PolicyVersion** with its author,
time, reason and the step-up that approved it (when one was needed). Scarab shows the
history of each Policy and a diff between any two versions, and a PolicyDecision's
PolicySetDigest names the exact versions it evaluated. Restoring an earlier version is a
save like any other (§10). Syncing policies from files in a governance repository is not
planned.

### 12. Typed settings compile into the same IR

The existing settings keep their forms and their API, and compile to Policy IR predicates
instead of running their own code. Each move must make the same decisions, except where
the table says it tightens:

| Today | Becomes |
|---|---|
| EnvironmentProjectEligibility (globs) | `run.create`: deny unless the Project is eligible |
| allowed refs, baseline and each overlay | one `run.create` deny per non-empty layer, unless the symbolic ref matches it; together these keep today's AND across layers (`AllowedRefConstraint`). A Run with no ref is denied where any layer restricts refs. The same check at `gate.resolve` for manual gates keeps the release-time re-check. External gates do not re-check refs today; adding that is a tightening. **Tightens:** also re-checked at `step.admit` for Steps that use the Environment's authority (§3). |
| approvers | a `gate.resolve` `require_approval` on the Run's manual gates, matching `actors: [human, human_token]`: every named Principal, counted per current wait. **Tightens:** also a `run.create` deny when a Step uses the Environment's authority with no manual gate upstream (§5), and the launch-time checks of §3 (approver set, PolicySetDigest, maximum age). Today an Environment with approvers asks no one when the Pipeline has no gate (`lib.rs:4519-4523`, `7480`). |
| step-up to approve where approvers are named | `gate.resolve`: `require_step_up` on manual gates, matching `actors: [human, human_token]`. External releases keep needing no step-up (`step_up/ops.rs:626-636`). |
| `require_reason` | `run.create`: deny a manual or API dispatch with no `reason`. Other triggers are exempt, as today (`lib.rs:4546-4555`). |
| EnvironmentProjectPrivilegeGrant (digests, `k8s_overlay`) | `step.admit`, with the `run.create` preflight |
| `wait_timer_s` | not enforced today (`environment_governance.rs:48`); stays unmapped until it is |

The new typed settings that ADR-0076 needs, "libraries allowed here" and "plugins whose
gates may run here", compile to `run.create` denies over `origin`.

## Consequences

- One place decides every governance rule, including the ones orgs write themselves, with
  one log, one "why", one audit mode and one test API.
- Per-repository allows and blocks, library and plugin approval, and extra approvals need
  no feature-specific code.
- The three checkpoint input schemas become a public contract.
- Scarab owns a CEL type checker for a subset until upstream has one.
- Moving every existing rule into the engine is a large migration. Each rule moves with a
  test showing the same decisions, and runs in `audit` beside the old code before the old
  code is deleted (§12). The approvers move tightens on purpose, and its `audit` run shows
  which Pipelines would be refused.
- In an Environment with approvers, every Step that uses the Environment's authority
  needs a gate the author wrote upstream of it. Authors coming from GitHub Actions, where
  Environment reviewers pause a job without one, write one more Step.
- In an Environment with approvers, Steps before the gate get the `env=none` OIDC
  subject. A Pipeline whose build Step assumed a production cloud role before any gate
  stops working, and the Step's log says why.
- An approval expires after 24 hours by default, for every launch it covers, and when the
  approvers or the gate's Policies change.
- The `env=none` subject before the gate applies on the release day, with no `audit`
  period.
- A Run admitted under one policy set can have a later Step denied under another. The
  decision record shows which, with its PolicySetDigest.
- Decisions store their inputs, which grows the Run's record.

## Alternatives considered

- **`cel-cxx`** (cel-cpp bindings with the official checker). Rejected by the owner: a C++
  and protobuf build in the control plane.
- **Cedar** (`cedar-policy`, pure Rust, schema-validated, analysable). Rejected: a second
  language next to CEL, and it decides allow or deny only, so require effects would ride
  on annotations.
- **OPA / Rego.** Rejected: a separate engine and language, and Rego is harder to bound.
- **Untyped CEL** (parse-only checks, as Pipelines have today). Rejected: a misspelt field
  in a deny rule would deny at runtime, or never match, with nothing at save time.
- **Typed settings compiled to CEL text.** Rejected: settings would lose their form-backed
  shape and gain parsing for nothing.
- **Pinning the policy set at Run creation.** Rejected: a yanked library or an urgent block
  would not apply to Runs in flight.
- **An `allow` effect, or Project-level exemptions.** Rejected: with deny-wins an allow is
  either redundant or an override; exceptions belong to whoever wrote the deny.
- **An `invoke.resolve` checkpoint at compile time.** Rejected in the 2026-10-29 pass: it
  fires in the same request as `run.create`, so it gives no earlier feedback; it has no
  trigger or ref in its input; and on the webhook path it would run for Pipelines that do
  not match and abort the whole delivery.
- **A `settings.change` checkpoint.** Rejected in the 2026-10-29 pass: its input would
  make every setting's shape a public schema, `require_approval` has no form there (a
  StepUpConfirmationRequest lasts 5 minutes and only the requester's human can confirm
  it), and step-up on loosening already lives in the route table.
- **Gates inserted by Policy, and a PolicyApprovalHold on a Step.** Rejected in the
  2026-10-29 pass: both change the Run's DAG or its Steps, against "no mutating effect";
  an inserted gate is not in the YAML and needs its own cases in reruns, expiry and the
  DAG view.
- **A gate required only before Deploy Steps.** Rejected in the 2026-10-29 pass:
  `deploy: true` is the author's mark and is optional, so a Run that leaves it off would
  reach the Environment's secrets with nobody asked.
- **An opt-in OIDC token per Step, or a claim for the cloud trust policy to check.**
  Rejected in the 2026-10-29 pass: the owner wants the token to need nothing from
  authors, and a claim that every trust policy must be rewritten to check fails open for
  every policy nobody rewrites.
- **`require_reason` as an effect.** Rejected in the 2026-10-29 pass: it behaves as a deny
  everywhere it is valid; it can be added later without changing saved Policies.
- **A message expression (`message_expr`).** Rejected in the 2026-10-29 pass: the subset
  has no `map` or `filter`, so it cannot name which list element matched, and it was the
  only use of string concatenation.
- **Default Policies shipped with Scarab and upgraded on their own.** Rejected in the
  2026-10-29 pass: a third shipped artifact with its own upgrade path; fail-closed
  typed-setting defaults do the same job.
- **Writing the `step.admit` decision in the engine's claim commit.** Rejected in the
  2026-10-29 pass: it would make the engine read the policy store for every Step;
  insert-if-absent at launch decides once with the engine unchanged.

## Implementation horizon

1. **Spike: done 2026-10-28** (branch `spike/cel-type-checker`, crate `scarab-policy`).
   The checker is about 1,000 lines over the `cel` 0.15 AST; schemas are generated from
   Rust types by two `macro_rules` macros, with optional fields and list caps in the
   types. 31 policy tests pass; of the 2,507 cel-spec cases in cel-rust's conformance
   data, 469 fall inside the subset and pass, and none fail. The upgrade keeps the suite
   green (2,005 passed). A static cost bound of 10,000 rejects three nested `exists` over
   32-element lists. Release binary +3.6%. The spike models an `invoke.resolve` input and
   admits string `+`; both go with the 2026-10-29 pass. Remaining for production, about
   1,500 to 2,000 lines: map and nested-struct schemas, string-length caps in the cost
   bound, several errors per check, parsing on a large-stack thread, editor-shaped
   diagnostics. Moving the spike to the `run.create` input (2026-10-29) set the list caps
   to 512 Steps, 8 `origin` entries (the invoke depth limit), 32 gates and secrets, 16
   grants and 8 placement profiles, and raised the cost limit to 100,000 so that §4's
   example passes (cost 75,265); one 512-by-64 evaluation took about 11 ms in a release
   build. A Run larger than the caps is refused when any `run.create` Policy applies. Upstream would want full CEL typing, so the checker stays in Scarab for
   now; its type core and tests could seed an upstream checker.
2. **Engine:** Policy IR, evaluator, PolicyDecision records, `audit` mode, the policy test
   API and `scarab validate --server`, org and Project layers, `run.create`. First users:
   library and plugin allow and block lists.
3. **Requirements:** `step.admit` at launch with the approval checks, `gate.resolve`,
   `require_approval` on authored manual gates, `require_step_up`, actor kinds in
   `match`, the gate-scoped OIDC subject; the Environment layer.
4. **Migration** of §12's settings, one at a time, each in `audit` beside the old code
   first.
5. **UI and docs:** policy editor with the checker's errors, version history and diffs,
   the decision view on the Run page, a "would deny" view for `audit` policies;
   CONTEXT.md terms (Policy, PolicyCheckpoint, PolicyEffect, PolicyDecision,
   PolicyInvariant, PolicySetDigest, PolicyVersion).

Decided by the owner on 2026-10-28, from this ADR's first draft:

- Project admins write typed settings only; CEL at the Project level is decided per use
  case (§7).
- One checkpoint per Policy (§4).
- Policies live in the database, versioned and diffable; no file sync (§11).
- Separation of duties is written as a Policy (§5).

Decided by the owner on 2026-10-29, in a simplification pass:

- Three checkpoints. Library and plugin rules are written at `run.create`, with early
  feedback from `scarab validate --server`; `invoke.resolve` is dropped (§2).
- `settings.change` is dropped; the step-up route table governs settings changes (§10).
- Only authors write gates. `require_approval` adds approvers to an authored manual gate
  at `gate.resolve`; a `run.create` deny requires such a gate upstream of every Step that
  uses the Environment's authority. The approvers typed setting compiles to both,
  starting in `audit` (§5, §12).
- Reruns downstream of a released gate do not ask again; at launch, allowed refs, the
  approver set, the PolicySetDigest and a maximum approval age (24 hours by default) are
  checked (§3).
- Steps before the gate get the `env=none` OIDC subject, with nothing for authors to
  write (§3).
- Three effects; a missing reason is a deny; each checkpoint lists the effects it allows
  (§5).
- Defaults are fail-closed typed-setting defaults (§7). No `message_expr` (§4).
- `step.admit`'s decision is recorded at launch, insert-if-absent (§3).
