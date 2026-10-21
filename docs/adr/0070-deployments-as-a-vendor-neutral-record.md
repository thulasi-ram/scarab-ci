# 0070. Deployments as a vendor-neutral record: what is live, not who approved

- **Status:** Proposed
- **Date:** 2026-10-22
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0024](0024-environments.md) (Environment as the deploy-governance
  home), [0037](0037-environment-governance.md) (approval is a gate; admission at the
  point of use)
- **Relates:** [0008](0008-step-contract.md) (`gate`), [0021](0021-double-effect-fencing.md)
  (fencing), [0041](0041-named-results-and-interpolation.md) (step results),
  [0047](0047-retry-classification-and-attempt-model.md) (failure classes, gate expiry),
  [0056](0056-run-takes-and-attempt-grain-evidence.md) (the sense of *superseded*),
  [0071](0071-org-level-environments.md) (where Deployments live),
  [0072](0072-plugins-a-governed-extension-point.md) (who reports them)
- **Executable spec:** `crates/scarab-deployment` (pure fold; invariants as property
  tests in `tests/invariants.rs`)

## Context

Scarab has a `deployments` table, but nothing in it is a deployment. A row is written in
exactly one place: when a manual gate is approved and released
(`crates/scarab-server/src/lib.rs:6603-6619`). It holds org, project, environment,
`git_ref`, run, `approved_by` and a timestamp. It is an **approval log**: it has no
target, no version, no digest and no outcome. A **Deploy Run** (a Run whose Pipeline
names an `environment:`) *governs* a deploy; it does not *record* one, and its success
means only that its Steps succeeded.

The motivating user is Treebo. Argo CD runs with auto-sync and `selfHeal`, a sync retry
limit of 5 (about 31 minutes of backoff), database migrations as Sync hooks at wave 5,
and image bumps made by PR to a shared `devops-gitops` repository. A watcher
(argo-watcher v0.10.7) waits 15 minutes and ignores `operationState`, so a sync that is
still retrying, or has already given up, reads as success or as an unrelated timeout.
QA's question is "what counts as a deployment?": failures surface late, there is no
single row per deploy, and two syncs of one change cannot be attributed.

Three properties of today's gates also block any "wait until it is live" step:

- **Gates cannot fail** except by the opt-in `gate_expires_after` (ADR-0047). There is no
  "passed or failed with a reason".
- **A waiting gate suspends the whole Run.** The gate pre-pass transitions the Run to
  `Suspended` as soon as any Pending gate has its dependencies satisfied
  (`crates/scarab-engine/src/scheduler.rs:1662-1677`), so independent branches stop.
- **Gate clocks are Run-level.** Timer release and expiry measure from the Run's
  `suspended_at` and pick the *first* matching gate with `find()`
  (`scheduler.rs:2858-2908`). Two gates cannot have two clocks.

The governing principle for this work, from the owner: *"We should not overfit scarab to
argo, we should ensure the plugin system is powerful enough to fit argo."* Argo is the
proving workload, not the model. No CD tool's nouns (Application, sync, Freight,
Promotion) enter core.

## Decision

### 1. Core owns a vendor-neutral Deployment record; plugins observe and act

Core owns the **Deployment** record and a **fixed** status vocabulary. Plugins
(ADR-0072) own vendor knowledge: how to observe a CD tool and how to act on it. This is
the shape of GitHub Deployments/Statuses, GitLab deployments, CDEvents and DORA's
deployment events: a neutral record that many tools write into.

The rules live in one pure crate, `crates/scarab-deployment`: a fold over requests and
source reports with no I/O, no clock (time is an input) and no vendor knowledge. It is
the only place that turns inputs into Deployments and statuses. Modularity is a crate
boundary; trust stays in core. There is no "deployment plugin", and plugins never depend
on plugins.

**Approval stops being "the deployment".** Approval answers *may this change go out?*. A
Deployment answers *what is live where, and how did it get there?*. They differ in
cardinality: one approved Run may deploy to several DeploymentTargets, and a change can
go live with no Run at all (a hotfix applied by hand, a `selfHeal` revert). Approval
becomes **evidence attached to** a Deployment, through the requesting Run's gate
history. The existing `deployments` rows stay readable as legacy approval history
(ADR-0071 §9).

### 2. A Deployment is an occurrence; the key only correlates

A **Deployment** is a specific **DeploymentVersion** made, or asked to be made, live on
a specific **DeploymentTarget**, together with what was reported about it afterwards. It
has its own id. *(Environment, DeploymentTarget, DeploymentVersion)* is only its
**correlation key**. (The Environment is part of the target, because a target belongs to
exactly one Environment.)

- **Join unless final.** A request or report attaches to the newest Deployment with its
  key unless that one is `superseded`. Otherwise it creates a new Deployment. A retried
  deploy Step, or two concurrent requesters, therefore make one Deployment.
- **Rollback is a new Deployment** of an older DeploymentVersion. An old Deployment is
  never edited back to life.
- A Deployment **outlives** the Deploy Run that requested it, and may have **no** Deploy
  Run at all. A report that creates one makes an **Unexplained** Deployment.

### 3. DeploymentTarget: the smallest unit that independently succeeds or fails

One Argo Application, one Helm release, one ECS service. Its identity is opaque and
source-supplied, *(source, id)*. Core compares it and never parses it. It comes into
existence the first time it is named, by a request or a report. There is no prior
registration. A rename at the source is a new DeploymentTarget. Fan-out (one Environment
across regions or clusters) is several DeploymentTargets, not a grouping concept.

### 4. DeploymentVersion: an opaque label in a scheme the source declares (PROVISIONAL)

A DeploymentVersion is an opaque label, compared only for equality. Its **scheme is
declared by the source, per DeploymentTarget**, and requesters must use that scheme. A
requester and a source that disagree on the scheme never correlate.

The first-party Argo plugin's default scheme is **the set of image digests the
Application runs** (`status.summary.images`). This is immune to unrelated commits on a
shared gitops repository: under a gitops-commit scheme, an unrelated team's merge to
`devops-gitops` would change every Application's version and break correlation (audit
finding 1, a blocker). A per-target option is *the last commit touching the target's
path*. Artifact digests attach to a Deployment as **evidence**, zero or more, never as
identity. A config-only change and a multi-image service are therefore both ordinary
Deployments.

**This decision is provisional.** The owner's words: *"not grasping the gravity… once
it's used practically I will know."* Revisit after the first real use against Treebo's
Argo. The fold does not depend on the scheme. Only the reporter and the requester do.

### 5. Six statuses, and who may set each

| Status | Set by | Meaning |
|---|---|---|
| `requested` | core | Asked for; no source report yet. |
| `in_progress` | source | The source is working on it. |
| `live` | source | The requested version is serving. |
| `failed` | source | The source has given up, with a reason. |
| `stalled` | core | Not converged within the EnvironmentDeploymentDeadline. Can still recover. |
| `superseded` | core | A newer Deployment took its place on the same DeploymentTarget. Final. |

A source can assert only `in_progress`, `live` or `failed`. The type system enforces this
(`SourceAssertedStatus`), so a plugin cannot fake a timeout or hide a change by calling
it superseded. At most one Deployment per DeploymentTarget is `live`. `failed → live` is
allowed while the Deployment is still the newest on its target. Only `superseded` is
final. *Superseded* means what it means for an Attempt (ADR-0056): a newer one took its
place.

**A status earns its place only if something branches on it**: a gate, a governance rule
or a view. "Was it ever live?" is a fact read from the reports, not a status. *Unexplained*
(no requester) and *Unconfirmed* (requested on a target its source has not yet
reported) are display terms, not statuses.

### 6. DeploymentStatusReport: a status plus a label

A report is a DeploymentStatus, a free-form label ("canary 2/4 at 25%", "hook migrate:
secret missing"), an optional link, and a time. The ordered reports are the Deployment's
**stages** as shown to QA. Core displays the label and never interprets it. **New vendor
states become labels, never statuses.**

### 7. DeploymentRequest: a Step field, recorded on success

A **DeploymentRequest** is a Step's declaration, in a Deploy Run, that its success asks
for a DeploymentVersion on a DeploymentTarget. It is a field on any Step, not a step
kind. The shape below is illustrative; spelling is settled in implementation.

```yaml
environment: prod
steps:
  - id: bump-gitops
    image: ghcr.io/acme/gitops-bump@sha256:…
    deployment_requests:
      - target: { source: argo-prod, id: payments-api }  # static: literal or ${{ inputs.* }}
        version: images        # this Step's own result, in the source's scheme
        evidence: digests      # optional: artifact digests
  - id: payments-live
    gate: { deployment: bump-gitops }
    needs: [bump-gitops]
```

- **Recorded only when the Step succeeds**, with values taken from the Step's own
  results (ADR-0041). A Step that changed the world and then failed records nothing; the
  change surfaces as Unexplained.
- **The DeploymentTarget is static**: a literal or a launch input. Ownership (ADR-0071)
  and the Environment check therefore happen at Run creation, before any Pod.
- **Effective time.** A request carries the time its Step *started* (when the change was
  made) as well as the time it was recorded (when the Step succeeded). If the key's newest
  Deployment is already final, the request still **joins it as history** when the source
  first saw that Deployment at or after the effective time: the source simply got there
  before the request was recorded. Only an *older* final Deployment makes the request a
  genuine rollback (INV-DEP-11).
- **Incomplete request.** If the Step exits 0 but its DeploymentRequest cannot be
  completed (a referenced result is missing), the Step fails with the failure class
  **DeploymentRequest incomplete**. It is **never auto-retried**: the change has probably
  already landed. The Pipeline check rejects a request that references a result the Step
  does not declare, before any Run exists. This needs a per-Step declaration of emitted
  results, which Steps do not have today (ADR-0041's results are an open map).
- **Environment mismatch.** A DeploymentTarget already known in **another** Environment
  is rejected at Run creation. One discovered there later is flagged as a conflict and
  never moved.

### 8. DeploymentGate: core resolves it, looking only forward

A **DeploymentGate** is a gate kind that waits on a DeploymentRequest made by an upstream
Step of the same Run. **Core resolves it** from status reports. A plugin reports facts
and never resolves a DeploymentGate (ADR-0072).

Its window opens at **max(request effective time, gate Attempt start)**. From there:

- **passed** if the Deployment was live at that point, or is reported `live` after it;
- **failed** on a `failed` report after it, on supersession, or once the
  EnvironmentDeploymentDeadline has elapsed since it. The source's label is the reason.

Whatever happens first decides. A failure that predates the window never fails the gate:
a re-deploy after fixing a missing secret is not failed by the earlier failure. "Before"
and "after" are judged on `(time, arrival sequence)`, never on time alone, because two
inputs can share a millisecond. A deadline sorts before every input stamped the same
time.

The verdict is **frozen per Attempt**. The Deployment's status may move on: a Run that
went red on `stalled` stays red even if the Deployment later goes `live`. **Retry is a
new Attempt that waits afresh** on the same Deployment, and its later Attempt start opens
a new window. The recovery path is: fix the secret, Resync (a PluginAction, ADR-0072),
Retry the gate. The deploy Step does not run again.

The fold today opens the window at the request's effective time
(`Deployment::gate_verdict`). The Attempt-start lower bound arrives with gate Retry.

### 9. Every gate can fail, holds only its dependents, and has its own clock

These changes apply to **all** gate kinds, not only DeploymentGate:

- **A gate resolves `passed` or `failed` with a reason.** A failed gate skips its
  dependents and fails the Run (as ADR-0047's expiry already does for one case).
- **A waiting gate holds only the Steps that depend on it.** The Run suspends only when
  nothing else can run. Waiting bills no Run budget, because the budget sums Running
  intervals only.
- **Each gate has its own wait clock**, started when its dependencies are satisfied and
  evaluated whether the Run is running or suspended. This replaces the Run-level
  `suspended_at` and the `find()` of the first matching gate (`scheduler.rs:2858-2908`)
  and the whole-Run suspension in the pre-pass (`scheduler.rs:1662-1677`).
- A verdict is final **per Attempt**. A Retry of a failed gate is a new Attempt that waits
  afresh: a rejected approval asks again.

### 10. EnvironmentDeploymentDeadline: the stall clock

How long a Deployment in an Environment may stay `requested` or `in_progress` before core
marks it `stalled`. It is measured from creation and **restarted by every request that
joins**. A later `live` or `failed` report still recovers a stalled Deployment. It is set
on the Environment and tightenable per Project through the EnvironmentProjectOverlay
(ADR-0071).

### 11. DeploymentReporterContract: what every reporter promises

Core publishes the contract every `report_deployments` plugin must follow, so that
behaviour is the same across CD tools:

- Report `failed` **only when the source has given up**: it will not progress without
  intervention. For Argo, that means retries are exhausted or the sync was terminated by
  timeout. Until then, report `in_progress` with the reason as the label.
- Report `live` **only when the requested DeploymentVersion is actually serving**:
  applied, healthy, and running the requested digests where the source can tell. For Argo,
  that is synced, healthy and running the requested digests.
- `live → failed` is allowed.
- A re-apply of the same DeploymentVersion is a report, not a new Deployment.

The first-party Argo plugin is the contract's reference implementation.

### 12. The invariants are the executable spec

The fold's invariants are numbered so that tests, ADRs and code comments can cite them.
Each is a property test in `crates/scarab-deployment/tests/invariants.rs`, except
INV-DEP-6, which the type system enforces. The crate is the spec. If this list and the
crate disagree, the crate wins and this ADR is stale.

- **INV-DEP-1** At most one Deployment per DeploymentTarget is `live`.
- **INV-DEP-2** `superseded` is absorbing. A late request may still add itself as a
  requester (history, not change).
- **INV-DEP-3** The newest Deployment on a DeploymentTarget is never `superseded`.
- **INV-DEP-4** At most one non-superseded Deployment per (target, version).
- **INV-DEP-5** Every non-superseded Deployment other than the newest is `live`, so at
  most two are open per target.
- **INV-DEP-6** A source can assert only `in_progress`, `live` or `failed` (types).
- **INV-DEP-7** `stalled` only when the deadline has elapsed on the stall clock and the
  latest report is neither `live` nor `failed`.
- **INV-DEP-8** No input is lost: every request and every report lands on exactly one
  Deployment with its key.
- **INV-DEP-9** A DeploymentGate looks forward. A failure that predates its request never
  fails it.
- **INV-DEP-10** A DeploymentGate verdict, once reached, never changes.
- **INV-DEP-11** A late request joins the Deployment the source already saw. It is not a
  rollback.

The property suite found three real holes before any persistence existed: an older
version reported live again rejoining its old Deployment (INV-DEP-3), a gate verdict
flipping between two inputs in the same millisecond (INV-DEP-10, hence the total order),
and a late request being treated as a rollback (INV-DEP-11).

### 13. One Environment per Run now; one per `invoke` segment in the target state

The **target state** is an Environment per `invoke` segment. Each segment is governed by
its own Environment: approval, secrets and eligibility are checked at Run creation for
every Environment named, and concurrency is per segment, never per Run. The **first
cut** keeps ADR-0037's one Environment per Run. Promotion is chained Deploy Runs over the
`upstream` trigger.

## Consequences

- "What is live in prod" has one org-wide answer, with stages QA can read and an outcome
  that does not depend on a watcher's timeout.
- A Deploy Run can wait for its change to go live, as a gate that can fail with the
  source's reason, without stopping unrelated branches of the Run.
- Changes made outside Scarab become visible as Unexplained rather than invisible.
- The gate rework touches every gate kind. Per-gate clocks and dependent-only holding
  change scheduler behaviour that existing Pipelines rely on. A Run with a waiting gate
  and runnable siblings now keeps running instead of suspending.
- New failure class (DeploymentRequest incomplete) and a new per-Step result declaration
  for the Pipeline check.
- A provisional decision (the version scheme) is load-bearing for correlation. It is
  isolated in the reporter and the requester so that revising it does not touch the fold.
- Known gaps this ADR does not close: approvers are opt-in, so an Environment with
  approvers does not force a gate into a Pipeline that omits one (git-bug `cd43bf3`); and
  external gate release skips `admits()` (git-bug `220917f`). Both matter more once gates
  gate real deploys.

## Alternatives considered

- **Deployment as a plugin, with core unaware.** Rejected. Gates, governance and
  red/green must read a status core understands, and a plugin would then have to depend
  on another plugin to share it. Plugins depend only on core; modularity is achieved as a
  crate boundary (`scarab-deployment`) instead.
- **Keep approval as the deployment** (today's row). Rejected. It has the wrong
  cardinality, records no target, version or outcome, and cannot represent a change
  made outside Scarab.
- **Let plugins define statuses.** Rejected. Core could not branch on them; new vendor
  states become labels instead.
- **Plugins resolve DeploymentGates.** Rejected. A plugin reports facts; core decides,
  so that the forward-looking and freeze rules hold across every CD tool.
- **(target, version) as the Deployment's identity.** Rejected. It cannot tell a
  rollback from a retried request, and it forces an old record to be edited when an older
  version returns.
- **Gitops commit as the default version scheme.** Rejected as the Argo default. On a
  shared gitops repository, every unrelated merge changes the version of every target
  (audit finding 1). It is kept as a per-target option (*last commit touching the path*).
- **Admin-registered DeploymentTargets.** Rejected. A target is born when first named.
  Registration duplicates the source's own inventory and drifts from it.
- **A gate that looks backwards** (fail on any `failed` on the Deployment). Rejected. A
  re-deploy after a fix would be failed by the failure it fixes (audit finding 2).
- **A DeploymentRequest step kind.** Rejected in favour of a field on any Step, because
  the Step that makes the change is the one that knows its version.

## Implementation horizon

- **MMP:** the Deployment record persisted from the fold; a read API; a Deployments view
  and an Environment board. `deployment_requests`, DeploymentGate, and the general gate
  changes (gates can fail, per-gate clocks, holding only dependents). The
  DeploymentReporterContract, with the first-party Argo reporter as its implementation
  (ADR-0072).
- **MLP:** gate Retry (the Attempt-start window) together with the Resync PluginAction
  that makes the recovery path usable.
- **Target state:** an Environment per `invoke` segment; the path-commit version option.
- **Revisit:** the DeploymentVersion scheme, after the first real use.
