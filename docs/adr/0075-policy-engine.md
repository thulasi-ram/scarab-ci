# 0075. One policy engine for every governance rule: typed settings and CEL compile to a Policy IR

- **Status:** Proposed
- **Date:** 2026-10-28
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0009](0009-dsl-ir-yaml-cel.md) (YAML frontend, typed IR, CEL),
  [0037](0037-environment-governance.md) (admission at the point of use),
  [0071](0071-org-level-environments.md) (org Environments, tighten-only overlays),
  [0073](0073-step-up-confirmation.md) (step-up, loosening rules)
- **Relates:** [0039](0039-privileged-images.md) (grants), [0055](0055-placement-profiles.md)
  (`k8s_overlay`, placement), [0038](0038-invoke-and-local-reuse.md) (`invoke`),
  [0074](0074-deployments-leave-core.md) (`deploy: true`), [0049](0049-identity-and-access.md)
  (RBAC), [0056](0056-run-takes-and-attempt-grain-evidence.md) (Takes, Attempts),
  [0069](0069-identity-links.md) (forge actor to Principal)

## Context

Scarab's governance rules are written as code, one feature at a time:

- Run admission against an Environment: eligibility, allowed refs, approvers
  (`admit_and_create_run`, `crates/scarab-server/src/lib.rs`; `EffectiveProtection`,
  `crates/scarab-project/src/environment_governance.rs`).
- Step privileges: grants against a per-Project digest whitelist and the `k8s_overlay`
  permission, checked for every Step at Run creation (`admit_step_grants`,
  `admit_k8s_overlay`, `lib.rs:5545-5578`).
- Re-admission at each launch: the Run's Environment rules are checked again before a
  Step's Pod starts (`ensure_still_admitted`, `crates/scarab-server/src/secret_executor.rs:88-118`).
- Gates: who may approve, release or reject; allowed refs re-checked when a gate releases
  (`lib.rs:7801`); a step-up to approve where the Environment names approvers
  (`lib.rs:7711-7735`); `gate_release_governance`.
- Fork pull requests: no secrets, OIDC subject `env=none` (`fork_policy`), no build push
  (`lib.rs:5604`), a read-only clone token (`lib.rs:5537`).
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
PolicyInvariants (§6), which stay in code. RBAC stays where it is (§8).

### 2. PolicyCheckpoints: fixed places with typed inputs

A **PolicyCheckpoint** is a place where Scarab consults the engine. The set is closed and
grows only with a Scarab release. Each checkpoint publishes a versioned, typed input
schema, generated from the Rust types that build the input, so the schema cannot drift
from what is passed. Adding a field is compatible; removing or retyping one bumps the
schema version.

| PolicyCheckpoint | When | Input (abridged) |
|---|---|---|
| `run.create` | Run creation, before any Step row | `org`, `project`, `trigger` (kind, optional symbolic `ref`, `sha`, `fork`, forge actor, optional `principal` linked to that actor), `pipeline` (name, `environment`, `deploy`), `steps` (every Step's static `step.admit` input), `principal`, `time` |
| `invoke.resolve` | compile, once per `invoke` | `invoke.target` (kind: local, library or plugin; name; content hash), `project`, `environment`, `time` |
| `step.admit` | every launch of an Attempt (§3) | `step` (image, digest, grants, `k8s_overlay`, secrets, placement, `deploy`, `origin`: the invoke kind, name and content hash it was inlined from), `run`, `environment`, `time` |
| `gate.resolve` | approve, reject, release | `gate`, optional `principal` (an external release has none), `approvals` in the current wait, `run`, `time` |
| `settings.change` | any governance setting, secret, policy, plugin or library change | `action`, `before`, `after`, `principal`, `scope`, `time` |

`time` is part of the input and is recorded with the decision, so a time-window rule
decides the same way on replay.

### 3. `step.admit` runs once per Attempt, in the claiming commit

`step.admit` replaces `ensure_still_admitted`. It is decided once per Attempt, including
automatic retries, which go from Running to Ready without passing promotion
(`rearm_step`, `crates/scarab-engine/src/scheduler.rs:4759`). The PolicyDecision is
written as an event in the same commit that claims the Step and records the Attempt,
keyed by *(run, step, attempt)*. A redelivered launch reads the recorded decision and
never evaluates again, so a crash between two commits cannot produce a second decision
under a newer policy. If the policy store cannot be read, the Step stays where it is for
that tick; it is never admitted without a decision.

`run.create` also evaluates every Step's static `step.admit` input as a preflight. A grant
or `k8s_overlay` that would be denied refuses the whole Run before any row exists, as
today (`lib.rs:5545-5578`). The launch-time check still runs, so a policy tightened after
Run creation applies to Steps not yet launched.

### 4. A Policy

```yaml
kind: policy
name: payments-prod-libraries
checkpoint: invoke.resolve
match:                                  # typed selector, evaluated before any rule
  projects: ["payments-*"]
  except_projects: [payments-sandbox]
  environments: [production]
mode: enforce                           # or `audit`
rules:
  - when: >
      invoke.target.kind == "library" &&
      !(invoke.target.name.startsWith("scarab/") ||
        invoke.target.name in ["acme/deploy-helm"])
    effect: deny
    message: "This library is not approved for payments in production."
    message_expr: '"not approved: " + invoke.target.name'   # optional, type-checked as a string
  - when: invoke.target.kind == "plugin"
    effect: require_approval
    approvers: { principals: ["user:alice", "user:bob"], count: 1 }
```

- `match` is typed (glob lists over projects and environments, with typed exceptions,
  and trigger kinds). Most policies never reach CEL.
- A rule is `when` (a CEL boolean over the checkpoint's input), an effect, a static
  `message` and an optional `message_expr` (CEL, checked to return a string). There is no
  separate templating syntax.
- A Policy names exactly one checkpoint. A rule that reads a field its checkpoint does not
  have is rejected when the Policy is saved (§6). Policies that span checkpoints may come
  later; one checkpoint per Policy is where this starts.

### 5. PolicyEffects: deny and require

| PolicyEffect | Meaning |
|---|---|
| `deny` | refuse, with the message |
| `require_approval` | named Principals must approve (`principals`, `count`) |
| `require_step_up` | the acting human confirms with a passkey (ADR-0073) |
| `require_reason` | the actor gives a reason |

There is no `allow` effect: a decision is allowed when nothing denies it. Allow lists are
written as "deny unless listed", and per-repository allows as exceptions in the org's own
Policy (§7). Policies never change a Pipeline or a Step; there is no mutating effect.

Where a `require_approval` takes effect depends on the checkpoint:

| Checkpoint | A `require_approval` becomes |
|---|---|
| `run.create`, `invoke.resolve` | a **gate node in the Run's IR**, added when the Run is persisted: before the Run's root Steps (`run.create`), or before the inlined library's or plugin's Steps (`invoke.resolve`). It appears in the DAG, has a stable Step id, and uses the existing gate machinery: its own clock, holding only dependents, expiry from the Policy. |
| `step.admit` | a **PolicyApprovalHold** on that Step: the Step does not launch until the named Principals approve. It has its own clock and the Policy's expiry, counts approvals per hold, appears as a badge on the Step's node, and is decided per Attempt. No node is added mid-Run. The per-DeploymentTarget concurrency hold that ADR-0074 deleted had the same shape (a Step held at promotion, counted by the suspend rule, its reason shown on the Run page); its code is at tag `archive/deployment-record` and is the starting point. |
| `gate.resolve` | an addition to that gate's **release quorum**. No gate is added. |

One approval counts toward every requirement that names its Principal.

**Separation of duties is a Policy, not a Scarab rule.** Scarab adds no built-in rule
such as "the person who triggered a Run may not approve it". The `gate.resolve` input
carries the approving `principal` and the Run's trigger actor, resolved to a Principal
through identity links (ADR-0069) when one exists, so an org writes the rule itself:

```yaml
checkpoint: gate.resolve
rules:
  - when: principal != null && has(run.trigger.principal) && principal == run.trigger.principal
    effect: deny
    message: "You started this Run, so someone else must approve it."
```
 Groups and roles
as approvers wait for a group concept in identity (ADR-0049 has none today).

### 6. CEL is type-checked, over a subset

Scarab moves from `cel-interpreter` 0.10 to `cel` 0.15 for parsing, the AST, declarations
and evaluation, and adds a **type-checking pass** over the 0.15 AST:

- Each checkpoint's schema is declared into the CEL `Env` as typed variables and struct
  types.
- The subset: field access, `has()` on a field (`has(run.trigger.ref)`; an optional
  top-level variable is tested with `!= null`), literals, comparisons, `&&`, `||`, `!`,
  `?:`, `in` on lists and maps, string functions (`startsWith`, `endsWith`, `contains`,
  `matches`) and string `+`, `size`, timestamp and duration comparison, and the `exists`,
  `all` and `exists_one` macros over typed lists. Anything else is rejected with "not
  supported in policies". Arithmetic, indexing, conversions, map iteration and
  time-of-day functions (`getHours()`) are outside the subset, so a time-window rule
  compares absolute timestamps; daily windows need a later addition.
- `matches` takes a literal pattern only; it is compiled when the Policy is saved.
- The checker computes a **static cost bound** from macro nesting and the list-size caps
  each schema declares, and rejects a rule above the limit. `cel` 0.15 has parse limits
  but no runtime cost tracking.
- A rule that type-checks cannot raise a type error. With arithmetic and indexing outside
  the subset, the evaluation error left is reading an optional field that is absent,
  without `has()`. Any evaluation error denies, with the error in the decision (fail-closed). The
  editor warns on optional fields read without `has()`, since a deny rule on `trigger.ref`
  would otherwise deny every Run that has no ref.

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
and `${{ }}` get the same length cap, and all CEL parsing runs on a thread with a large
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
  - step-up for any `settings.change` that loosens governance, including deleting or
    editing a Policy (§10) and a default-Policy upgrade that loosens.
- **Default Policies** ship with Scarab and are installed into every org, as first-party
  libraries are: editable and removable by an org admin, and upgraded independently of
  the server. An upgrade is a `settings.change`.

### 8. RBAC stays separate

A role (ADR-0049) decides whether a Principal may call an API, and is checked first.
Policy then decides what that call may do: a Policy can deny or require on top of a role,
and never grants what the role does not.

### 9. Decisions are recorded, and use the current policies

- Each consultation produces a **PolicyDecision**: the checkpoint, the **input itself with
  its schema version**, the **PolicySetDigest** (a content hash of every Policy evaluated,
  across the three layers), every matching Policy with its outcome, messages and
  requirements. A Run's decisions are stored with the Run and shown on the Run page.
  `settings.change` decisions go to the org security log (ADR-0073).
- A checkpoint uses the policies current when it is consulted, once (§3). A Policy added
  or tightened mid-Run applies to Steps not yet launched.
- A Rerun recompiles nothing: inlined Steps keep their `origin`, so `step.admit` can deny
  a Step whose library was yanked or blocked after the Run started.
- `audit` mode evaluates and records "would deny" or "would require" without enforcing.
- A policy test API evaluates a Policy against a stored PolicyDecision's input or a
  supplied one, without enforcing.

### 10. Changing a policy is governed

Saving a Policy is a `settings.change`:

- Adding a Policy, or moving one from `audit` to `enforce`, tightens: no step-up.
- Editing a Policy that is in `audit` mode needs no step-up, since it enforces nothing.
- Any other change (editing an enforced rule, deleting a Policy, moving to `audit`) counts
  as loosening and needs a step-up. Whether an edited CEL rule is looser cannot be
  decided in general, so the check fails closed, as ADR-0073 does.
- A plugin's kill switch tightens and never needs a step-up.

### 11. Policies are stored in the database, versioned and diffable

Policies, and the typed settings that compile into Policy IR, live in Scarab's database,
not as files in a repository. Every save creates a new **PolicyVersion** with its author,
time, reason and the step-up that approved it (when one was needed). Scarab shows the
history of each Policy and a diff between any two versions, and a PolicyDecision's
PolicySetDigest names the exact versions it evaluated. Restoring an earlier version is a
`settings.change` like any other save. Syncing policies from files in a governance
repository is not planned.

### 12. Typed settings compile into the same IR

The existing settings keep their forms and their API, and compile to Policy IR predicates
instead of running their own code. Each move must make the same decisions:

| Today | Becomes |
|---|---|
| EnvironmentProjectEligibility (globs) | `run.create`: deny unless the Project is eligible |
| allowed refs, baseline and each overlay | one `run.create` deny per non-empty layer, unless the symbolic ref matches it; together these keep today's AND across layers (`AllowedRefConstraint`). A Run with no ref is denied where any layer restricts refs. The same check at `gate.resolve` keeps the release-time re-check. |
| approvers | a `gate.resolve` release quorum on the Environment's existing manual gates: every named Principal, counted per current wait. Adds no gate. |
| step-up to approve where approvers are named | `gate.resolve`: `require_step_up` |
| `require_reason` | `run.create` and `gate.resolve`: `require_reason` |
| EnvironmentProjectPrivilegeGrant (digests, `k8s_overlay`) | `step.admit`, with the `run.create` preflight |
| `wait_timer_s` | not enforced today (`environment_governance.rs:48`); stays unmapped until it is |

## Consequences

- One place decides every governance rule, including the ones orgs write themselves, with
  one log, one "why", one audit mode and one test API.
- Per-repository allows and blocks, library and plugin approval, and extra approvals need
  no feature-specific code.
- The checkpoint input schemas become a public contract.
- Scarab owns a CEL type checker for a subset until upstream has one.
- Moving every existing rule into the engine is a large migration. Each rule moves with a
  test showing the same decisions, and runs in `audit` beside the old code before the old
  code is deleted (§12).
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
- **Gates added mid-Run for `step.admit` requirements.** Rejected: gates are IR nodes with
  stable ids, read by expiry, reruns and the DAG view; a PolicyApprovalHold on the Step
  avoids rewriting the graph during a Run.

## Implementation horizon

1. **Spike: done 2026-10-28** (branch `spike/cel-type-checker`, crate `scarab-policy`).
   The checker is about 1,000 lines over the `cel` 0.15 AST; schemas are generated from
   Rust types by two `macro_rules` macros, with optional fields and list caps in the
   types. 31 policy tests pass; of the 2,507 cel-spec cases in cel-rust's conformance
   data, 469 fall inside the subset and pass, and none fail. The upgrade keeps the suite
   green (2,005 passed). A static cost bound of 10,000 rejects three nested `exists` over
   32-element lists. Release binary +3.6%. Remaining for production, about 1,500 to 2,000
   lines: map and nested-struct schemas, string-length caps in the cost bound, several
   errors per check, parsing on a large-stack thread, editor-shaped diagnostics. Upstream
   would want full CEL typing, so the checker stays in Scarab for now; its type core and
   tests could seed an upstream checker.
2. **Engine:** Policy IR, evaluator, PolicyDecision records, `audit` mode, the policy test
   API, org and Project layers, `invoke.resolve` and `run.create`. First users: library and
   plugin allow and block lists.
3. **Requirements:** gate nodes from `run.create` and `invoke.resolve`, PolicyApprovalHold,
   `require_reason`, `require_step_up`; `step.admit` in the claiming commit, `gate.resolve`,
   `settings.change`; the Environment layer.
4. **Migration** of §12's settings, one at a time, each in `audit` beside the old code
   first.
5. **UI and docs:** policy editor with the checker's errors, version history and diffs, the decision view on the Run
   page, a "would deny" view for `audit` policies; CONTEXT.md terms (Policy,
   PolicyCheckpoint, PolicyEffect, PolicyDecision, PolicyInvariant, PolicySetDigest,
   PolicyApprovalHold, PolicyVersion).

Decided by the owner on 2026-10-28, from this ADR's first draft:

- Project admins write typed settings only; CEL at the Project level is decided per use
  case (§7).
- One checkpoint per Policy (§4).
- Policies live in the database, versioned and diffable; no file sync (§11).
- Separation of duties is written as a Policy (§5).
