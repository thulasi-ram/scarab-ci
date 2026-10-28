# 0075. One policy engine for every governance rule: typed settings and CEL compile to a Policy IR

- **Status:** Proposed
- **Date:** 2026-10-28
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0009](0009-dsl-ir-yaml-cel.md) (YAML frontend, typed IR, CEL),
  [0037](0037-environment-governance.md) (admission at the point of use),
  [0071](0071-org-level-environments.md) (org Environments, tighten-only overlays),
  [0073](0073-step-up-confirmation.md) (step-up, loosening rules)
- **Relates:** [0039](0039-privileged-images.md) (grants), [0055](0055-placement-profiles.md)
  (`k8s_overlay`), [0038](0038-invoke-and-local-reuse.md) (`invoke`),
  [0074](0074-deployments-leave-core.md) (`deploy: true`), [0049](0049-identity-and-access.md)
  (RBAC)

## Context

Scarab's governance rules are written as code, one feature at a time:

- Run admission against an Environment: eligibility, allowed refs, approvers
  (`admit_and_create_run`, `crates/scarab-server/src/lib.rs`; `EffectiveProtection`,
  `crates/scarab-project/src/environment_governance.rs`).
- Step privileges: grants against a per-Project digest whitelist (`admit_step_grants`,
  `admit_grants_against_whitelist`) and the `k8s_overlay` permission (`admit_k8s_overlay`).
- Fork pull requests: no secrets, OIDC subject `env=none` (`fork_policy`).
- Gates: who may approve, release or reject (`approve_gate`, `gate_release_governance`).
- Settings changes that loosen governance need a step-up (`crates/scarab-server/src/step_up/`).

Each rule has its own data model, its own UI and its own error text. None can be tried
before it is enforced, none can be scoped to "these repositories only" unless the feature
added that by hand, and an org cannot add a rule Scarab did not anticipate. The next
features need more of the same: which libraries and plugins a repository may `invoke`,
per-repository allow and block lists at the org level, and requirements such as "a
production deploy that changes a migration needs a second approval".

The owner's direction (2026-10-28): one policy engine that covers every governance rule,
at the org level and on Environments; rules can deny and can also require things; typed
settings and CEL must work together, with an IR if needed.

Two facts shaped the design:

- **CEL is already Scarab's expression language** (ADR-0009), and Kubernetes'
  ValidatingAdmissionPolicy shows the pattern this needs: a cheap match selector, CEL
  rules, a message on denial, an audit mode.
- **The Rust CEL crate has no type checker.** Scarab uses `cel-interpreter` 0.10. Its
  successor, `cel` 0.15 (github.com/cel-rust/cel-rust), adds typed declarations (`Env`,
  `FunctionDecl`, `TypeRegistry`) but its conformance runner states that type checking is
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
typed predicates, not to CEL text, so they stay fast, explainable and editable in a form.
CEL covers what a form cannot express.

Every governance rule in the Context list moves into the engine (§9). RBAC stays where it
is: a role decides whether a Principal may call an API at all, and policy decides what
that call may do (§8).

### 2. PolicyCheckpoints: fixed places with typed inputs

A **PolicyCheckpoint** is a place where Scarab consults the engine. The set is closed and
grows only with a Scarab release. Each checkpoint publishes a versioned, typed input
schema:

| PolicyCheckpoint | When | Input (abridged) |
|---|---|---|
| `run.create` | Run creation, before any Step row | `org`, `project`, `trigger` (kind, ref, sha, fork, actor), `pipeline` (name, `environment`, `deploy`, step summaries), `principal` |
| `invoke.resolve` | compile, per `invoke` | `invoke.target` (kind: local, library, plugin; name; content hash), `project`, `environment` |
| `step.admit` | a Step's promotion to Ready | `step` (image, digest, grants, `k8s_overlay`, secrets, placement, `deploy`), `run`, `environment` |
| `gate.resolve` | approve, reject, release | `gate`, `principal`, `approvals` so far, `run` |
| `settings.change` | any governance setting, secret, policy, plugin or library change | `action`, `before`, `after`, `principal`, `scope` |

The input schemas are generated from the Rust types that build them, so a checkpoint's
schema cannot drift from what it passes. They are a public contract: adding a field is
compatible, removing or retyping one is a breaking change with a schema version bump.

### 3. A Policy

```yaml
kind: policy
name: payments-prod-libraries
checkpoint: invoke.resolve
match:                                  # typed selector, evaluated before any rule
  projects: ["payments-*"]
  environments: [production]
mode: enforce                           # or `audit`
rules:
  - when: >
      invoke.target.kind == "library" &&
      !(invoke.target.name.startsWith("scarab/") ||
        invoke.target.name in ["acme/deploy-helm"])
    effect: deny
    message: "${invoke.target.name} is not approved for payments in production"
  - when: invoke.target.kind == "plugin"
    effect: require_approval
    approvers: { group: platform, count: 1 }
```

- `match` is typed (glob lists over projects, environments, trigger kinds) and cheap, so
  most policies never evaluate CEL.
- A rule is `when` (a CEL boolean over the checkpoint's input), an effect, and a message.
- A Policy names one checkpoint. A rule that reads a field its checkpoint does not have
  is rejected when the Policy is saved (§5).

### 4. PolicyEffects: deny and require

The effects are a closed list:

| PolicyEffect | Meaning | Valid at |
|---|---|---|
| `deny` | refuse, with the message | every checkpoint |
| `require_approval` | add an approval requirement (who, how many) | `run.create`, `step.admit` (holds the Step), `invoke.resolve` |
| `require_step_up` | the acting human must confirm with a passkey (ADR-0073) | `settings.change`, `gate.resolve` |
| `require_reason` | the actor must give a reason | `run.create` (manual), `gate.resolve`, `settings.change` |

There is no `allow` effect: a decision is allowed when nothing denies it. An allow list
is written as "deny unless listed". Policies never change a Pipeline or a Step; there is
no mutating effect.

A `require_approval` at `step.admit` holds the Step behind a gate that the engine adds
for that Step, with the policy's approvers and message. It uses the gate machinery that
already exists: its own clock, holding only dependents, expiry, fail with a reason. The
Run page shows which policy added it.

### 5. CEL is type-checked, over a subset

Scarab moves from `cel-interpreter` 0.10 to `cel` 0.15 for parsing, the AST, declarations
and evaluation, and adds a **type-checking pass** over the 0.15 AST:

- Each checkpoint's schema is declared into the CEL `Env` as typed variables and struct
  types.
- The checker accepts a subset: field access, literals, comparisons, `&&`, `||`, `!`,
  `?:`, `in` on lists and maps, string functions (`startsWith`, `endsWith`, `contains`,
  `matches`), `size`, and the `exists`, `all` and `exists_one` macros over typed lists.
  Anything else is rejected with "not supported in policies".
- An expression that type-checks cannot raise a type error at evaluation. Evaluation
  errors that remain (an absent optional field) deny, with the error in the decision
  (fail-closed).
- Expressions have a size and evaluation-cost limit.

Pipelines' `when:` and `${{ }}` move to `cel` 0.15 too; they stay unchecked until the
checker covers their contexts.

The checker is offered upstream to cel-rust, whose conformance suite already has
`check_only` cases waiting for one. If upstream ships its own, Scarab switches to it.

**A spike comes first** (§10): the upgrade, the checker for one checkpoint, a dozen real
policies, and the cel-spec `check_only` cases inside the subset.

### 6. Layers, combination and defaults

Policies attach at three levels:

| Level | Written by | May match |
|---|---|---|
| Org | org admin | any project, environment |
| Environment | that Environment's admins | Runs governed by it |
| Project | that Project's admins | its own Runs |

- **Combination.** Every Policy whose checkpoint and `match` fit is evaluated. Any
  `deny` wins, and every deny message is reported. Requirements add up: two
  `require_approval` effects need both. No layer can remove what another layer adds, so
  a lower layer can only tighten, as Environment overlays do today (ADR-0071 §3).
- **PolicyInvariants** are rules in code that no Policy can turn off: the hardened Step
  baseline (ADR-0039), fork pull requests receiving no secrets and an `env=none` OIDC
  subject, and that a policy change which loosens governance needs a step-up. The
  engine runs after them.
- **Default Policies** ship with Scarab and are installed into every org, as first-party
  libraries are: editable and removable by an org admin, upgraded independently of the
  server. Today's built-in governance defaults become default Policies (§9).

### 7. Decisions are recorded, and use the current policies

- Each consultation produces a **PolicyDecision**: the checkpoint, a hash of the input,
  the policy set version, every matching Policy, its outcome, messages and requirements.
  Decisions for a Run are stored with the Run and shown on the Run page ("why was this
  denied"). Decisions for `settings.change` go to the org security log (ADR-0073).
- A checkpoint uses the policy set **current when it is consulted**. A Policy added or
  tightened mid-Run applies to the Steps not yet admitted. A Rerun is consulted again.
  This is what lets a yanked library or a new block take effect at once.
- `audit` mode evaluates and records "would deny" or "would require" without enforcing,
  so a Policy can run against real traffic first.
- A policy test API evaluates a Policy against a stored input (a past decision) or a
  supplied one, without enforcing.

### 8. RBAC stays separate

A role (ADR-0049) decides whether a Principal may call an API, and is checked first.
Policy then decides what that call may do: a policy can deny or require on top of a role,
and never grants what the role does not.

### 9. Typed settings compile into the same IR

The existing settings keep their forms and their API, and compile to Policy IR
predicates instead of running their own code:

| Today | Becomes |
|---|---|
| EnvironmentProjectEligibility | `run.create`: deny unless the project is eligible |
| allowed refs (baseline and overlay) | `run.create`: deny unless the ref matches every layer |
| approvers, `require_reason` | `gate.resolve` and `run.create` requirements |
| EnvironmentProjectPrivilegeGrant (digests, `k8s_overlay`) | `step.admit`: deny an ungranted grant or overlay |
| step-up loosening rules (ADR-0073) | `settings.change`: `require_step_up` |

Fork-PR secret lockout stays a PolicyInvariant (§6), not a setting.

### 10. Changing a policy is governed

Saving a Policy is a `settings.change`. Adding a Policy, or moving one from `audit` to
`enforce`, tightens. Any other change (editing a rule, deleting a Policy, moving to
`audit`) counts as loosening and needs a step-up: whether an edited CEL rule is looser
cannot be decided in general, so the check fails closed, as ADR-0073 does.

## Consequences

- One place decides, with one log, one "why", one audit mode and one test API, for every
  governance rule, including the ones orgs write themselves.
- Per-repository allow and block lists, library and plugin approval, and requirements
  such as extra approvals need no feature-specific code.
- The checkpoint input schemas become a public contract.
- Scarab owns a CEL type checker for a subset until upstream has one.
- Moving every existing rule into the engine is a large migration. Each rule moves with
  a test showing the same decisions before and after, and runs in `audit` beside the
  old code before the old code is deleted.
- Evaluating at each checkpoint with current policies means a Run can be admitted under
  one policy set and have a later Step denied under another. The decision record shows
  which.

## Alternatives considered

- **`cel-cxx`** (cel-cpp bindings with the official checker). Rejected by the owner: a C++
  and protobuf build in the control plane.
- **Cedar** (`cedar-policy`, pure Rust, schema-validated, analysable). Rejected: a second
  language next to CEL, and it decides allow or deny only, so require effects would ride
  on annotations.
- **OPA / Rego.** Rejected: a separate engine and language, and Rego is harder to bound.
- **Untyped CEL** (parse-only checks, as Pipelines have today). Rejected: a misspelt field
  in a deny rule would deny at runtime, or never match, with nothing at save time.
- **Typed settings compiled to CEL text.** Rejected: settings lose their fast, explainable
  form and gain parsing for nothing.
- **Pinning the policy set at Run creation.** Rejected: a yanked library or an urgent block
  would not apply to Runs already in flight.
- **An `allow` effect.** Rejected: with deny-wins it is either redundant or an override,
  and overrides make a policy set hard to reason about.

## Implementation horizon

1. **Spike:** `cel` 0.15 upgrade (Pipelines' `when:` and interpolation still pass), the
   subset checker against `invoke.resolve`'s schema, a dozen policies, the cel-spec
   `check_only` cases in the subset. Decide on the checker from the result.
2. **Engine:** Policy IR, evaluator, PolicyDecision records, `audit` mode, the policy test
   API, org and Project layers, `invoke.resolve` and `run.create`. First users: library
   and plugin allow and block lists.
3. **Requirements:** `require_approval` (engine-added gates), `require_reason`,
   `require_step_up`; `step.admit`, `gate.resolve`, `settings.change`; Environment layer.
4. **Migration** of §9's settings, one at a time, each in `audit` beside the old code
   first.
5. **UI:** policy editor with the save-time checker's errors, decision view on the Run
   page, "would deny" view for `audit` policies.

Open:

- Whether Project admins may write CEL policies, or only org and Environment admins.
- Whether a Policy may span checkpoints (one name, rules at several), or stays one
  checkpoint per Policy.
- Whether policies can also live as files in a governance repository and sync in, with
  the UI read-only for them.
