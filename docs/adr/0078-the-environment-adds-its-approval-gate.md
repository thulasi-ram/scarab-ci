# 0078. The Environment adds its approval gate

- **Status:** Accepted
- **Date:** 2026-11-07
- **Deciders:** thulasi.ram (architect)
- **Supersedes in part:** [0075](0075-policy-engine.md) §5 and the 2026-10-29 owner
  decision "Only authors write gates": an Environment now adds a gate when the author
  wrote none. The rest of 0075 stands, including §3's launch-time re-check, which this
  ADR relies on.
- **Builds on:** [0037](0037-environment-governance.md) (approval is a gate),
  [0071](0071-org-level-environments.md) (org Environments, tighten-only overlays),
  [0073](0073-step-up-confirmation.md) (step-up on approvals)
- **Tickets:** git-bug cd43bf3, 220917f, 2b94989, c2f97b4

## Context

An Environment's approvers are checked only inside `approve_gate`, when someone
approves a `manual` gate. That has three consequences:

- A Run that names `prod` and has no `manual` gate asks no one. Its Steps get prod's
  secrets and grants unasked (cd43bf3).
- Every `manual` gate in a Run that names `prod` needs prod's approvers, including a
  "QA signs off" gate in front of a smoke test that holds nothing of prod's.
- `external`, `timer` and `plugin` gates never pass through `approve_gate`. An
  external gate releases on its HMAC token alone, without the Environment's rules
  (220917f).

The Environment's `wait_timer` is stored and editable, and nothing reads it (2b94989).

ADR-0075 planned to close the first gap by refusing the Run: a `run.create` deny when a
Step uses the Environment's authority with no `manual` gate upstream. Under that plan,
an org admin who adds approvers to `prod` makes every pipeline in every Project that
deploys to `prod` without a gate start failing, until each team edits its YAML. A
governance setting should be safe to tighten without breaking other teams' pipelines.

## Decision

**What a protected Environment promises.** In an Environment with approvers, no Step
receives the Environment's authority (its secrets, a governed grant or `k8s_overlay`,
`forge_token: write`, or a `deploy: true` mark) until a named approver has released an
**Environment approval gate** upstream of it. At every launch of that Step, the ref is
still allowed, the approver set is the one recorded when the gate released, and the
release is within the maximum approval age.

1. **An Environment approval gate** is a `manual` gate with a Step after it that uses
   the Environment's authority. Only these gates need the Environment's approvers and a
   step-up. Every other `manual` gate in the Run is a plain gate: anyone with write on
   the Project releases it. Which gates are Environment approval gates is decided at
   Run creation, stored on the gate, and shown on the gate in the UI and by
   `scarab validate`.
2. **The Environment adds one when the author wrote none.** At Run creation, the
   earliest Steps that use the Environment's authority with no `manual` gate upstream
   get one added gate in front of them: a real `manual` Gate Step, one per Run, whose
   `needs` are those Steps' `needs` combined and whose `origin` names the Environment.
   It is an ordinary Gate: approve, reject, expiry, rerun, step-up and the DAG view work
   as for any other. With `deploy: true` on the whole Pipeline, every Step uses the
   authority, so the added gate has no `needs` and comes before the build. Authority
   Steps further down are already behind it, so the added gate cannot form a cycle.
3. **Missing-gate mode.** Each Environment chooses what happens when an authority Step
   has no `manual` gate upstream: **add the gate** (the default) or **refuse the Run**
   (ADR-0075's plan, for teams that want every wait in their YAML). The Environment
   also sets the added gate's expiry, the number of approvals it needs, and the maximum
   approval age (24 hours by default).
4. **The wait timer** is a minimum wait on the Environment approval gate, counted on
   the gate's own wait clock. The gate releases when the approvals are in and the timer
   has run out; approvals given early count. The timer applies only with approvers: the
   UI disables it and the API refuses it on an Environment with none, and an existing
   timer-only Environment shows a warning that the timer does nothing.
5. **External gates are fixed at launch, not at release.** An `external`, `timer` or
   `plugin` gate is never an Environment approval gate. The launch-time re-check of
   ADR-0075 §3 (allowed refs, approver set, maximum age, for every Step that uses the
   Environment's authority) covers every gate kind, and reruns and retries that pass no
   gate release. The external release does not get its own ref check.
6. **One engine.** These rules are the first typed settings moved into the policy
   engine (ADR-0075 §12): the gate requirement at `run.create`, the release rules at
   `gate.resolve`, the re-check at `step.admit`. Library and plugin lists follow. API
   token scopes stay typed settings under the step-up route table, with no
   `token.issue` checkpoint.
7. **Shipped enforced.** There is no `audit` period. In the default mode nothing is
   refused; Runs that deployed unasked start pausing for approval on the release day.
   The changelog says so, and says that `manual` gates guarding no authority Step stop
   needing the approvers.

The Environment page changes its copy to match: "Who may deploy" becomes "Projects
that may use <Environment>" (EnvironmentProjectEligibility admits every Run that names
the Environment, not only deploys), and the "Projects" panel becomes "Project overlays
and grants". The web UI gets an org Policies page listing CEL Policies with their
decisions and audit counts, and the typed settings read-only with a link to where each
is edited.

## Considered options

- **Refuse only (ADR-0075 §5).** Every wait stays in the YAML, but tightening an
  Environment breaks other teams' pipelines. Kept as the non-default mode.
- **An approval pause that is not a Gate**, as GitHub's Environment reviewers pause a
  job. It would need its own events, rerun rules, expiry, reject, DAG display and
  step-up path, all of which Gates already have.
- **One added gate per authority Step.** No extra waiting, but approvers are asked once
  per Step; a matrix deploy to five regions asks five times. One gate per Run makes
  "approve this deploy" one decision. Its cost: in a pipeline with parallel deploy
  branches, each branch waits for the others' `needs`. A team that wants approval per
  branch writes its own `manual` gates there.
- **Every `manual` gate in a governed Run needs the approvers** (today's rule). The
  sentence is shorter, but a gate that guards nothing of the Environment's needs prod's
  approvers and a step-up.
- **Marking the gate in YAML** (`for: environment`). The author can get the mark wrong
  in both directions, and Scarab would have to check it against the graph anyway.
- **A wait timer without approvers adds a `timer` gate.** It would make the
  Environment's gate sometimes an approval and sometimes a timer. A Run can be
  cancelled at any point before its deploy Step starts, which covers the "time to
  cancel" use.

## Consequences

- The Run's DAG can differ from its YAML by one Step. The gate's `origin` names the
  Environment, and `scarab validate` shows where it would go.
- A Run created before its Environment gained approvers has no added gate. Its
  authority Steps are refused at launch by the §3 re-check, with a `Config` failure
  naming the reason.
- Steps before the Environment approval gate get the `env=none` OIDC subject
  (ADR-0075 §3).
- CONTEXT.md defines **Environment approval gate** and marks each rule decided and not
  built until it ships.
