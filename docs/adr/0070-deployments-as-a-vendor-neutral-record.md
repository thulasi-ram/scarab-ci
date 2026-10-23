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
- **Explanation is decided by timing.** A request that joins a report-created Deployment
  explains it only if its Step **started at or before core first received** that
  Deployment (effective time ≤ creation time): the Step was already running when the
  source showed the change, so it can be what made it. That is the same test that makes a
  late request history rather than a rollback (§7, INV-DEP-11), and it covers the normal
  Argo flow, where the source reports after the Step starts but before it succeeds. A
  request whose Step started **after** is recorded as a **claim**: it stays among the
  requesters, so a DeploymentGate on it still resolves, but it never explains the
  Deployment, which **stays Unexplained**. Otherwise any request that happened to name the
  same version would launder a hand-applied hotfix (ADR-0071 §4). The decision is made as
  inputs arrive and is **sticky** (INV-DEP-12): a rerun Step's later request never
  un-explains a Deployment, and a requester that explained it is never demoted to a claim.
  With two requesters, any one that qualifies explains it. "Made live by" lists only the
  requesters that are not claims.
- **Known limits of the timing rule.** A retried Attempt whose new start is after the
  source's report reads as a claim, although its first Attempt made the change. Widening
  the effective time to the first Attempt would open the gate window over that Attempt's
  failure (§8). The DeploymentRequest writer (MMP slice (b)) stamps the **succeeding**
  Attempt's start (§7), so this limit still stands. A
  long-running Step that spans a hand-applied change of the same version explains it; the
  record cannot tell the two apart, and that ambiguity is inherent.

### 3. DeploymentTarget: the smallest unit that independently succeeds or fails

One Argo Application, one Helm release, one ECS service. Its identity is opaque and
source-supplied, *(source, id)*. Core compares it and never parses it. It comes into
existence the first time it is named, by a request or a report. There is no prior
registration. A rename at the source is a new DeploymentTarget. So is a **re-home**: when
the source reports a known target under a grouping that maps to another Environment (an
Argo Application moved to another Argo project, or an admin moving the grouping between
Environments), it is recorded as a new DeploymentTarget in the new Environment and the old
one ends. Its history stays where it was; nothing is moved, and the move is not a standing
conflict.

"Ends" is a **display, not a status**: the old Environment's board shows the target as
**Moved** — naming where its reports go now ("now: prod"), or that no Environment lists
the grouping ("unmapped") — instead of claiming what was last live there; no input is
appended and no seventh status exists. Moved is derived on read from the current
EnvironmentDeploymentSources, so an admin's edit shows at once. A request for the target on
the old Environment waits and, honestly, stalls: its reports now land elsewhere. The new
Environment never shows the old one's last status as its own; the target appears there
when its reporter re-sends its current state (§11). Fan-out (one Environment
across regions or clusters) is several DeploymentTargets, not a grouping concept.

### 4. DeploymentVersion: an opaque label in a scheme the source declares (PROVISIONAL)

A DeploymentVersion is an opaque label, compared only for equality. Its **scheme is
declared by the source, per DeploymentTarget**, and requesters must use that scheme. The
source publishes a **scheme id** with each target. A request whose version does not fit
the target's scheme is **rejected when it is recorded**, instead of waiting silently on a
version the source will never report.

The first-party Argo plugin's default scheme is **the set of image references in the pod
templates of the Application's target manifests**: what Argo rendered from git (its
`manifests`, or the `targetState` of its managed resources), not what is running. It is
deliberately **not** `status.summary.images`, which is collected from live Pods: during a
rolling update it is the union of old and new, it includes sidecars a mutating webhook
injected, Sync-hook Job Pods flicker in and out of it, and a scale-to-zero app has an
empty set. Each of those would create a spurious Deployment that supersedes the requested
one and fails its DeploymentGate. The target-manifest set avoids all four, and shows the
new version as soon as Argo refreshes, before the sync. Like any per-target scheme, it
also avoids the gitops-commit problem: on a shared gitops repository an unrelated team's
merge would change every Application's version (audit finding 1, a blocker).

Its blind spots, stated plainly:

- **Tags, not digests.** The references are the strings written in the manifest, usually
  tags. A tag re-pushed to new bytes is the same DeploymentVersion.
- **Config-only changes are invisible.** A values, ConfigMap or replica change leaves the
  image set as it was. A request for it joins the Deployment already live, and a
  DeploymentGate on it passes at once, before Argo applies anything. **A DeploymentGate
  proves "V is live", not "my change took effect".**
- **The requester must predict the whole rendered set**, including every image the chart
  renders that the change did not touch.
- **The path-commit option has its own gaps.** *The last commit touching the target's
  path* sees config changes, but misses kustomize bases outside the path, multi-source
  Applications and remote charts, and needs git read access because Argo does not expose
  it. It is no rollout signal either: whether a version is serving is the reporter's job
  (§11).

Artifact digests attach to a Deployment as **evidence**, zero or more, never as identity.

**The known weakness is equality-only correlation**, which forces the requester to predict
how the source will render its change. The leading alternative, to be evaluated with real
data, is **source acknowledgement**: the source maps a request, or the commit that carried
it, to the version it actually rendered, in the style of GitHub Deployments. The
observed-only board (Implementation horizon, step 2) exists partly to gather that
evidence before DeploymentGates bake a scheme into Steps.

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
secret missing"), an optional link, and the source's own time. The source's time is
**display only**; core orders every input by the time it received it (§8). The ordered
reports are the Deployment's
**stages** as shown to QA. Core displays the label and never interprets it. **New vendor
states become labels, never statuses.**

### 7. DeploymentRequest: a Step field, recorded on success

A **DeploymentRequest** is a Step's declaration, in a Deploy Run, that its success asks
for a DeploymentVersion on a DeploymentTarget. It is a field on any Step, not a step
kind. As built (MMP slice (b)):

```yaml
environment: prod
interface: { inputs: [ { name: app, type: string, required: false, default: payments-api } ] }
steps:
  - id: bump-gitops
    image: ghcr.io/acme/gitops-bump@sha256:…
    results: [images, digests]   # the result names this Step emits
    deployment_requests:
      - target: { source: argo-prod, id: "${{ inputs.app }}" }  # literal or ${{ inputs.* }}
        version: images          # a declared result, in the source's scheme
        evidence: [digests]      # optional: declared results holding artifact digests
  - id: payments-live
    gate: deployment
    deployment: { step: bump-gitops }  # + target: {…} when the Step requests >1 target
    needs: [bump-gitops]
```

- **The Pipeline check** rejects, before any Run exists: a request on a Pipeline with no
  `environment:`; one on a matrixed Step (every leg would request the same target); a
  `version` or `evidence` that names a result the Step does not declare in `results:`
  (result names are file stems: `/scarab/results/<name>.json`); a target part that is
  neither a literal nor exactly `${{ inputs.<declared input> }}`; and two requests of one
  Step naming the same target. A gate Step declares neither `results` nor requests.
- **Recorded only when the Step succeeds**, with values taken from the Step's own
  results (ADR-0041): the version must be a non-empty JSON string, each evidence result a
  string or an array of strings. The success transition, the Attempt's outcome and one
  Deployment input per request commit in **one transaction**. An exit 0 observed for an
  Attempt that is no longer the Step's latest (superseded, cancelled, or a successor
  runs) records nothing. A Step that changed the world and then failed records nothing;
  the change surfaces as Unexplained.
- **On Kubernetes, results arrive through the results-egress sidecar**, not from the
  executor. The executor withholds the Step's success until the sidecar has terminated,
  and the sidecar exits only after the results were committed or its POST failed. A
  failed drain therefore reads as "exit 0, result missing" and fails the Step as
  DeploymentRequest incomplete, which misattributes an infrastructure loss to the author
  (git-bug `0cb5895`).
- **The DeploymentTarget is static**: a literal or a launch input, resolved once at Run
  creation against the launch parameters (a webhook Run uses the inputs' defaults). An
  unresolved or empty part refuses the Run, and so do two requests of one Step that
  resolve to the same target. `POST /v1/runs` (inline IR) refuses any request or
  DeploymentGate: they need a Deploy Run. The Environment check happens at Run creation,
  before any Pod. The ownership check (ADR-0071) belongs there too, and is **not built**
  (git-bug `9c1173a`).
- **Effective time.** A request carries the time its Step *started* (when the change was
  made) as well as the time it was recorded (when the Step succeeded). As built, the
  effective time is the **succeeding Attempt's** `started_at`, and the receipt time is
  read right before the commit. Both are control-plane times, and the effective time is
  clamped to be no later than the request's receipt time; a source's clock never enters
  the comparison. If the key's newest Deployment is already final, the request still
  **joins it as history** when core first received that Deployment at or after the
  effective time: the source simply got there before the request was recorded. Only an
  *older* final Deployment makes the request a genuine rollback (INV-DEP-11).
- **Scheme check.** A version that does not fit the target's published scheme id (§4), or
  the source's default scheme for a target it has not named yet, is to be rejected when
  the request is recorded, failing the Step as **DeploymentRequest incomplete** below.
  **Not built** (git-bug `5fc1860`): today any non-empty string is recorded.
- **Incomplete request.** If the Step exits 0 but its DeploymentRequest cannot be
  completed (a referenced result is missing or of the wrong type), the Step fails with the
  failure class **DeploymentRequest incomplete** (`deployment-request-incomplete`), naming
  every problem at once. It is Failed, not dead-lettered, and **never auto-retried**,
  `retry:` included: the change has probably already landed. The per-Step `results:`
  declaration is what lets the Pipeline check catch an undeclared result first.
- **Environment mismatch.** A DeploymentTarget already known in **another** Environment
  is refused at Run creation: a webhook delivery names the refused Pipeline, a dispatch
  answers 403. "Known" means the source's current routing of the target, else the
  Environment of its newest recorded input. A request that an admitted Run has not yet
  recorded does not count, so two Runs in different Environments that both name a
  never-seen target are both admitted, and the target's home then follows its newest
  input (git-bug `9e66f2c`). A target the source later reports under another
  Environment is re-homed (§3), never moved.

### 8. DeploymentGate: core resolves it, looking only forward

A **DeploymentGate** is a gate kind that waits on a DeploymentRequest made by an upstream
Step of the same Run. **Core resolves it** from status reports. A plugin reports facts
and never resolves a DeploymentGate (ADR-0072).

It is spelled `gate: deployment` with `deployment: { step, target? }` (§7's example).
The Pipeline check requires `step` to be one the gate needs, directly or transitively,
that declares `deployment_requests`. `target` must be omitted when that Step makes
exactly one request, and must otherwise spell one of them exactly as the request does. A
matrixed DeploymentGate is refused. Approving or releasing it is refused: core
resolves it. Any principal who can write to the Run may **reject** it with a reason, as for
every gate (§9), because rejecting only tightens. `gate_expires_after` still bounds its
wait.

Its window opens at **max(request effective time, gate Attempt start)**, for **every**
Attempt. From there:

- **passed** if the Deployment was live at that point, or is reported `live` after it;
- **failed** on a `failed` report after it, on supersession, or once the
  deadline has elapsed since it. The source's label is the reason.

Whatever happens first decides. A failure that predates the window never fails the gate:
a re-deploy after fixing a missing secret is not failed by the earlier failure.

In practice the window opens at the Attempt's start, because a gate's first Attempt
starts only after the deploy Step has succeeded, which is after the request's effective
time. So a `failed` received *while the deploy Step was still running* is history to the
gate: the gate does not fail with that label but waits, and fails `stalled` at the
deadline unless the source reports again after the Attempt opened. Whether the first
Attempt should open at the effective time instead is an open owner question (git-bug
`fce05da`).

The Attempt-start bound is the instant *(Attempt start, after every sequence number)*.
An input received in the millisecond the Attempt started therefore counts as **before**
the window: a `live` then passes the gate at once, and a `failed` then is history. A
window bounded this way opens only once that millisecond has ended. Until then, more
inputs stamped in it can still arrive, and deciding early let one of them flip a verdict
already reached (INV-DEP-10). A supersession that predates the bound does fail a retried
gate, at its start: `superseded` is final.

**As built.** The deadline is the Project's effective one, the tighter of the
Environment's baseline and the Project's EnvironmentProjectOverlay (§10). It is the
1-hour default when the Org no longer has the Environment, and it is re-read on every
tick. The verdict's reason is `"<version> live on <source>/<id>: <label>"` (or `"…was
already live…"`), the source's `failed` label verbatim, `"superseded by <version>"`, or
`"stalled: not live within <deadline>"`. Missing or unreadable records are handled as
follows:

- **The request is missing.** A Run whose request row for the gate is missing fails the
  gate at once. Run creation writes those rows before any Step exists, and they never
  change. A request that is not yet in the Deployment record fails the gate only after an
  **uncached** re-read of the record. Each tick shares one fold per Environment, and that
  fold may predate the request's commit on another replica. The re-read is causal: the
  request commits with the deploy Step's success, and the gate's wait opened after that.
- **The record cannot be read.** The gate takes **no decision** that tick, so one bad row
  cannot fail every Deploy Run in the Environment, but only until the deadline has
  elapsed since the window opened. After that it fails `stalled`, and the read error is
  in the reason. An error reading the deadline itself still means no decision (git-bug
  `eace9a8`).

**"Before" and "after" mean received before and after.** Core stamps every input, request
or report, with its **receipt time**: the caller reads it from the control-plane clock
right before the append, and the store keeps it monotone per Org (the later of it and the
Org's last stamp) in the transaction that appends the input, so receipt order and arrival
order agree. A receipt time more than a minute ahead of the database's own clock is
refused, so one replica's runaway clock cannot push every later stamp into the future.
The request's effective time, the gate Attempt's start and the gate's `now` come from
that same control-plane clock. A DeploymentGate decides only below the store's
**horizon**. Before folding, the oracle fences the Org's stamp clock to after its `now`,
in one statement that waits out any append holding the counter. So every input it did
not read is stamped after the instant it decides for, whatever the appender's clock or
commit latency. A source's own timestamp is display
only. The fold requires non-decreasing time and clamps an input that arrives backdated up
to the last one rather than trusting it. Two inputs can still share a millisecond, so ties
are broken by arrival sequence, and a deadline sorts before every input stamped the same
time.

The cost of ordering by receipt: a stale `failed` that reaches core late counts as
*after* the window and can fail a fresh gate. The DeploymentReporterContract's "source
order, never replay older history" (§11) is what prevents that, and so it is
load-bearing.

The verdict is **journaled per Attempt** when it is first reached, as a `GateResolved`
carrying the Deployment's id, and never re-derived from the log afterwards: a gate tick
can race an append, and a verdict the engine has already acted on must not flip. The
Deployment's status may move on: a Run that went red on `stalled` stays red even if the
Deployment later goes `live`. **Retry is a new Attempt that waits afresh** on the same
Deployment, and its later Attempt start opens a new window. The recovery path is: fix
the secret, Resync (a PluginAction, ADR-0072), Retry the gate. The deploy Step does not
run again. Gate Retry is built; Resync is not (ADR-0072's MLP), so today the source must
report again by itself.

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
- A verdict is final **per Attempt**, journaled when first reached. A Retry of a failed
  gate is a new Attempt that waits afresh: a rejected approval asks again.

As built (MMP slice (b)):

- **A wait is an Attempt.** When a gate is reached (every need `Succeeded`), core opens a
  `running` Attempt for that wait. The gate Step itself stays `Pending` until its verdict.
  The Attempt's `started_at` is the gate's clock start. It is computed once from the event
  log, frozen, and identical across a crash and every re-tick. The start is the latest of:
  - each need's latest finish since that need's own latest re-arm;
  - the gate's own latest re-arm (a Retry, or a Rerun that invalidates it);
  - for a gate with no needs only, the Run's first `→ Running`.

  A need that is `Succeeded` in the rows but has no finish in the log counts as the
  moment the Attempt opened, so a crash can only start a clock late, never early. A timer
  passes at `started_at + gate_after`, and expiry fires at
  `started_at + gate_expires_after`, for every gate kind.
- **Ways to fail.** A gate fails by expiry, by the DeploymentGate oracle (§8), by
  `POST /v1/runs/{id}/gates/{step}/reject` (a reason of 1 to 1024 bytes is required; the
  caller needs Write on the Run), or by an external gate's token
  `POST /v1/runs/{id}/gates/{step}/release` with `{"passed": false, "reason": …}`. A
  reject consults no Environment governance, since it only tightens. Who may reject in a
  governed Environment is open (git-bug `5c86952`). The wait's Attempt fails with the
  failure class `gate`, never auto-retried. Its dependents are Skipped and the Run ends
  Failed.
- **One write.** A verdict closes the open Attempt, moves the gate out of `Pending` and
  journals `GateResolved { passed, reason, by, deployment }` in **one transaction**, fenced
  on that Attempt. A verdict on a wait that was superseded or cancelled meanwhile writes
  nothing.
- **Gates act only when reached.** Approving, releasing or rejecting a gate whose needs
  have not all succeeded is refused (409). Approvals count **per wait**: only those given
  after the gate's latest re-arm count toward the quorum, so a Retry or Rerun asks again.
- **The suspend rule.** The Run suspends only when nothing is `Ready` or `Running` and
  every other non-terminal Step is behind a waiting gate or a per-DeploymentTarget
  concurrency hold (ADR-0071 §9). The Run reopens as soon as anything can run.
- **Known gaps:** the dashboard's "Waiting on you" inbox still reads Run status, so a gate
  waiting inside a Running Run does not appear there (git-bug `c489346`). A waiting gate
  whose dependents are all terminal still keeps its Run suspended (git-bug `2c37802`).

### 10. EnvironmentDeploymentDeadline: the stall clock

How long a Deployment in an Environment may stay `requested` or `in_progress` before core
marks it `stalled`. It is measured from creation and **restarted by every request that
joins**. A later `live` or `failed` report still recovers a stalled Deployment.

It is set on the Environment, and that **baseline deadline alone drives a Deployment's
status**, so one Deployment has one status whoever looks at it. A Project's
EnvironmentProjectOverlay (ADR-0071) may set a tighter deadline, which applies **only to
that Project's DeploymentGates**.

### 11. DeploymentReporterContract: what every reporter promises

Core publishes the contract every `report_deployments` plugin must follow, so that
behaviour is the same across CD tools:

- Report `failed` **only when the source has given up**: it will not progress without
  intervention. For Argo, that means retries are exhausted or the sync was terminated by
  timeout. Until then, report `in_progress` with the reason as the label.
- Report `live` **only when the requested DeploymentVersion is actually serving**:
  applied and healthy at that version, where the source can tell. For Argo, that is
  synced to a target whose version is the requested one, and healthy.
- `live → failed` is allowed.
- A re-apply of the same DeploymentVersion is a report, not a new Deployment.
- Report **transitions only**, never every reconcile. Argo reconciles about every three
  minutes; a report per reconcile adds no fact and grows every Environment's log.
- Report **in source order** and **never replay older history**. Core judges before and
  after by receipt time (§8), so a stale report delivered late reads as new.
- Each report carries an **idempotency key** and a **source sequence** per
  *(installation, DeploymentTarget)*. Core drops a repeat, and a report whose sequence is
  not newer than the last one it accepted, so an at-least-once retry cannot flip `live`
  back to `failed`.
- Reports reach core by push under an identity core verifies, and core sets the report's
  source from that identity, never from the report (ADR-0072 §11).
- Sequences are integers from 1 to **2^53 − 1**, the largest a JSON number holds exactly
  in every reader. (A reporter that sends a huge one freezes its own target: every later
  report is stale until an admin resets the cursor.)
- **Re-send when asked.** Core never fabricates a report from what it last heard. When a
  target routes to an Environment its last status never reached — a grouping an admin just
  mapped, or moved — `GET /targets` flags it `resend`. Poll `GET /targets?resend=true` at
  least once a minute and re-send each flagged target's **current** state as a new report
  (next sequence, new idempotency key): the one exception to "transitions only". Skip a
  target you have no status for. Only a status report that lands clears the flag; an
  ownership report does not.
- **Page `/targets`** (`after` = the previous page's `next_after`, `limit` ≤ 1000), at
  start-up to seed cursors and on every resend poll.
- **Read results per item.** A batch is answered item by item: `accepted`, `duplicate`,
  `stale`, `unmapped`, or `rejected` with a reason — unreadable, breaking a rule, or past a
  per-installation cap (`cap:`). A rejected item stored nothing, not even its key: fix it
  or drop it, never retry it unchanged.
- **A 409 names the key** reused for different content, and means the **whole batch was
  rolled back**. That item is a reporter bug: log it, drop it, and resend the rest
  unchanged.
- **Back off on 429 and 503** by `Retry-After`. 429: a batch from this installation is
  already in flight — send one at a time. 503: core is saturated; nothing was recorded, so
  the same batch is safe to resend.
- **A failure during the deploy Step is history to its gate.** A DeploymentGate's window
  opens at its Attempt's start, after the deploy Step succeeded (§8). A `failed` that the
  deploy Step's own change caused, reported while that Step was still running, therefore
  fails the gate only through the deadline, unless the reporter reports again after the
  gate opened. Whether the first Attempt should open earlier is the owner question in
  git-bug `fce05da`.

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
- **INV-DEP-9** A DeploymentGate looks forward from its window start, the later of its
  request's effective time and its gate Attempt's start. A failure **received before**
  the window opens never fails it; one **received after** may.
- **INV-DEP-10** A DeploymentGate verdict, once reached, never changes. The fold keeps
  this for a reader whose `now` is below the store's horizon (§8). The oracle reads only
  so, and the engine also journals the verdict per Attempt.
- **INV-DEP-11** A late request joins the Deployment the source already saw. It is not a
  rollback.
- **INV-DEP-12** A claim never explains a Deployment, and nothing un-explains one (§2).
- **INV-DEP-13** A gate Attempt's start only ever narrows its window. An input received
  at or before the Attempt started never fails the gate, and the Attempt-start bound never
  makes a verdict arrive sooner or land earlier than it would without it (§8). So the
  Retry of a failed DeploymentGate waits afresh on the same Deployment.

The property suite found three real holes before any persistence existed: an older
version reported live again rejoining its old Deployment (INV-DEP-3), a gate verdict
flipping between two inputs in the same millisecond (INV-DEP-10, hence the total order),
and a late request being treated as a rollback (INV-DEP-11). A second audit found a
fourth: inputs folded in arrival order but judged on the source's clock, so a backdated
report could flip a verdict. Receipt-time stamping and the fold's clamp close it; a
scenario test covers one case, and a property test over backdated and non-monotone inputs
holds INV-DEP-1..11 after the clamp. The same backdated-input property test later found a
fifth, in the Attempt-start bound itself. A window bounded at *(Attempt start, after
every sequence number)* could decide while `now` was still that millisecond, and a
`failed` received later in the same millisecond then withdrew the verdict (INV-DEP-10).
A window bounded this way now opens only once its own millisecond has ended (§8). R3
found a sixth, the same hole as the fifth with a wider trigger. A commit slower than one
millisecond, or a lagging replica clock, stamped an input before a verdict already
reached. It is closed by the store horizon, not by the fold.

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
- New failure classes (`deployment-request-incomplete`, and `gate` for every failed gate
  wait) and a new per-Step result declaration (`results:`) for the Pipeline check.
- A provisional decision (the version scheme) is load-bearing for correlation. It is
  isolated in the reporter and the requester so that revising it does not touch the fold.
  Under the default scheme a DeploymentGate cannot see a config-only change (§4).
- Every board read refolds the Environment's whole input log, and so does every tick
  that has a DeploymentGate waiting in it: the gates of one Environment share one fold per
  tick. "Transitions only" (§11) bounds the log's growth; an incremental fold or a
  snapshot is a known follow-up (git-bug `1f2163f`).
- Ordering by receipt time makes the reporter contract's "source order, never replay"
  load-bearing (§8).
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
  (audit finding 1). It is kept as a per-target option (*last commit touching the path*),
  with the gaps listed in §4.
- **`status.summary.images` as the Argo default.** Rejected. It is read from live Pods:
  the old∪new union during a rollout, injected sidecars, Sync-hook Pods and empty sets for
  scale-to-zero apps each create a spurious Deployment that supersedes the requested one.
- **Source acknowledgement** (the source maps a request or commit to the version it
  rendered). Not rejected: the leading alternative to equality-only correlation, deferred
  until real use gives data to judge it (§4).
- **Ordering inputs by the source's timestamp.** Rejected. Source and control-plane clocks
  skew, and a skewed report turned a late request into a phantom rollback. Core's receipt
  time orders; the source's time is display only.
- **Re-deriving a gate verdict from the log on every tick.** Rejected. A tick can race an
  append, and a backlogged input could flip a verdict already acted on. The verdict is
  journaled per Attempt.
- **Admin-registered DeploymentTargets.** Rejected. A target is born when first named.
  Registration duplicates the source's own inventory and drifts from it.
- **A gate that looks backwards** (fail on any `failed` on the Deployment). Rejected. A
  re-deploy after a fix would be failed by the failure it fixes (audit finding 2).
- **A DeploymentRequest step kind.** Rejected in favour of a field on any Step, because
  the Step that makes the change is the one that knows its version.

## Implementation horizon

- **MMP, in this order.** Each step leaves something usable, and real data arrives before
  the riskiest step depends on it.
  1. **A minimal org-level Environment** (ADR-0071): its name and its
     EnvironmentDeploymentSources. Deployment inputs are keyed by Org and Environment
     name, so this comes first; reads use org Read.
  2. **The first-party Argo reporter (ADR-0072) together with the read side**: the
     Deployment record persisted from the fold, a read API, a Deployments view and an
     Environment board. The DeploymentReporterContract ships here. This is an
     **observed-only board** (every Deployment is Unexplained) that needs no Pipeline
     change, and it gives real evidence on the provisional version scheme.
  3. **The full Environment migration** (ADR-0071): eligibility, overlays, privilege
     grants.
  4. **The Run side** (slice (b), built): `results:` and `deployment_requests`,
     DeploymentGate, per-DeploymentTarget concurrency holds (ADR-0071 §9), and the general
     gate changes: gates can fail and be rejected, per-gate clocks, holding only
     dependents, verdicts journaled per Attempt, and gate Retry with the Attempt-start
     window (§8, §9). The Run page shows each gate's verdict and reason, what a waiting
     DeploymentGate waits on, and why a deploy Step is held; the Deployments view links
     each requester to its Run.
- **MLP:** the Resync PluginAction that makes the gate-Retry recovery path usable.
- **Known follow-ups**, deferred from slice (b) with tickets:
  - the scheme check (§7, `5fc1860`) and the ownership rejection (`9c1173a`);
  - a target first claimed by two Environments at once (`9e66f2c`);
  - a failed results drain on Kubernetes reads as DeploymentRequest incomplete (`0cb5895`);
  - when the first gate Attempt's window should open (owner question, `fce05da`);
  - a wait with no bound when its deadline cannot be read (`eace9a8`);
  - an incremental or snapshot fold (`1f2163f`);
  - a Rerun skips a deploy Step whose inputs are unchanged, so a rollback Rerun fails its
    DeploymentGate (`d880466`);
  - an `invoke` library's target input leaks to the caller's launch inputs (`bbbf618`);
  - a version with surrounding whitespace is stored verbatim (`5f03853`);
  - one Pipeline's refusal aborts the later Pipelines of a webhook event (`66171a0`);
  - who may reject a gate in a governed Environment (`5c86952`);
  - the "Waiting on you" inbox (`c489346`), and a doomed Run kept suspended by its gate
    (`2c37802`);
  - every terminal Step transition and its event in one write (`586187a`).
- **Target state:** an Environment per `invoke` segment; the path-commit version option.
- **Revisit:** the DeploymentVersion scheme, after the first real use, with source
  acknowledgement as the leading alternative.
