# 0074. Deployments leave core: Scarab governs the Run that deploys, the CD tool owns what is live

- **Status:** Proposed
- **Date:** 2026-10-28
- **Deciders:** thulasi.ram (architect)
- **Supersedes:**
  - [0070](0070-deployments-as-a-vendor-neutral-record.md), all of it except §9 (every
    gate can fail, holds only its dependents and has its own clock; its "as built"
    clauses that name the DeploymentGate or the per-DeploymentTarget hold go), the
    step-level `results:` declaration from §7, and the first cut in §13 (one Environment
    per Run);
  - [0071](0071-org-level-environments.md) §4 (DeploymentTargetProjectOwnership), §5
    (EnvironmentDeploymentSources), §9 (concurrency per DeploymentTarget), the
    EnvironmentDeploymentDeadline, and the parts of §1 and §3 that give an Environment
    DeploymentTargets, Deployments, a deadline or a per-target concurrency limit;
  - [0072](0072-plugins-a-governed-extension-point.md) `report_deployments`, §11
    (reports reach core by push), and every part of §4 and §5 that binds a PluginAction
    or a write rule to a DeploymentTarget;
  - [0073](0073-step-up-confirmation.md)'s loosening rules for the deleted settings:
    adding a deployment source (E3), and an overlay's longer gate deadline (E1).
- **Builds on:** [0037](0037-environment-governance.md) (approval is a gate, admission at
  the point of use, scoped secrets), [0072](0072-plugins-a-governed-extension-point.md)
  §4 (every PluginAction is a `plugin_action` Step in a Run) and §5 (CredentialBroker)
- **Relates:** [0008](0008-step-contract.md) (a Step is any OCI image),
  [0021](0021-double-effect-fencing.md) (fence as idempotency key),
  [0032](0032-slices-3-5-implementation-decisions.md) (concurrency groups),
  [0041](0041-named-results-and-interpolation.md) (results),
  [0047](0047-retry-classification-and-attempt-model.md) (timeouts, failure classes)

## Context

ADR-0070 put a vendor-neutral Deployment record in core. A Deployment is a
DeploymentVersion on a DeploymentTarget with six statuses. Core matches Run requests to
source reports, shows changes made outside Scarab as Unexplained, treats a rollback as a
new Deployment, re-homes a target that moves between Environments, and keeps an org-wide
board of what is live. The first-party Argo CD plugin reports into it, and a
DeploymentGate lets a Run wait until its change is live. The minimum version of all of
this is built and merged (PRs #26–#28): `crates/scarab-deployment` (about 1,300 lines
and 13 property-tested invariants), `crates/scarab-server/src/deployment_gates.rs`, the
`/deployments` board, and migrations 0053 to 0058. The roadmap added actions that drive
the CD tool: Resync, Argo Rollouts promote and abort, and UI buttons on
DeploymentTargets.

Reviewing the Deployments screen on 2026-10-28, the owner's concern was scope: Scarab is
becoming a CD tool. Knowing what is live where, tracking it after a Run has ended, and
spotting changes made outside Scarab is what Argo CD, Flux, Kargo and Spinnaker do.
Scarab's position is forge-native CI on a durable control plane
([positioning.md](../positioning.md)).

We looked at three ways to answer that concern:

- **Move the record into the CD plugin.** A DeploymentGate passes when the requested
  version is live and fails when a newer version replaces it (`Deployment::decide`,
  `crates/scarab-deployment/src/lib.rs:543`). Deciding that means matching reports to the
  request by target and version, and that matching is the record, so a plugin that held
  it would decide the gate. Plugins also have no storage, no event subscription and no
  UI tier today: `SubscribeEvents`, `OfferActions` and `ContributeUi` are declared in
  `crates/scarab-plugin/src/lib.rs:105-121` and grep found nothing that uses them. Each
  CD plugin would also re-implement the invariants, in which the property tests found
  six ordering holes while they were being built.
- **Keep the code and narrow the roadmap** to observing and waiting, with a non-goal
  against driving deploys. This leaves core holding a record of what is live, which is
  the part the owner objects to.
- **Delete the record**, and keep what a CI engine needs to run a deploy.

One need remains after the record is gone. With GitOps, a deploy Step's success means
only that its change merged into the gitops repository. Argo applies it later, and the
rollout can still fail: an image that can't be pulled, a migration hook that fails, Pods
that never become ready, or a sync that Argo stops retrying after about 31 minutes.
Treebo's QA problem came from that gap. A deploy Run must be able to wait, inside the
Run, until its change is live or has failed. The owner's call is that Scarab does not
need to know anything about a deploy after the Run that made it has ended, and that
adding the wait is the pipeline author's responsibility.

## Decision

### 1. Scarab's part in a deploy ends with the Run

Scarab runs and governs the Run that deploys. It can wait, inside that Run, for a CD
tool to report that the change is live. It keeps no record of what is live after the Run
ends, and it does not detect changes made outside Scarab. The CD tool answers "what is
in production".

This becomes a non-goal in CONTEXT.md §3: *Scarab is not a CD tool. It governs and runs
the Run that deploys, and it keeps no state about deployments between Runs.*

### 2. A Deploy Run is a Run whose Pipeline names an Environment

`environment:` stays the mark. A Run that sets it is a Deploy Run, and the mark brings:

- admission against the Environment: EnvironmentProjectEligibility, protection rules,
  approvers and `allowed_refs` (ADR-0037, ADR-0071 §2, §3);
- Environment-scoped secrets (ADR-0071 §6) and per-Project privilege grants (§8);
- no newest-wins cancel: a Deploy Run carries no supersede key, and its concurrency group
  always queues (`crates/scarab-server/src/lib.rs:5444-5466`). A newer push never
  cancels a deploy half-way.

Users find a Project's deploys by filtering its Runs by Environment. The Runs tab has
that filter today, but it filters only the page of Runs already fetched
(`ui/scarab-web-ui/src/routes/RepoView.tsx:185`, `:218`), and `GET` on the runs list
takes only `limit`. The runs list API gains an `environment` filter so older deploys can
be found. There is no separate Deployments screen.

### 3. What is deleted

**Domain and engine**

- The Deployment record: the `scarab-deployment` crate, its six statuses, Unexplained,
  Unconfirmed and Moved, and rollback as a new Deployment.
- DeploymentTarget, DeploymentVersion and the version scheme.
- `deployment_requests` on a Step, `gate: deployment`, the DeploymentGate, the
  `deployment-request-incomplete` failure class, the `DeploymentGates` engine port and
  the code that builds deployment inputs when a Step succeeds.
- EnvironmentDeploymentDeadline, EnvironmentDeploymentSources,
  DeploymentTargetProjectOwnership, and the per-DeploymentTarget concurrency hold,
  including its place in the suspend rule.
- The `deployment` field of the `GateResolved` event stays in the event type as an
  optional field, so events already in the log still decode. Nothing writes it, and the
  UI stops linking it.

**Storage.** One migration drops what migrations 0053 to 0058 added for the record:
the deployment inputs (0053), `environment_deployment_sources` (0054),
`deployment_source_targets`, `deployment_source_unmapped` and `plugin_report_receipts`
(0055, which also holds the plugin tables that stay), the deadline and per-target
concurrency columns (0057), and the deployment requests (0058).

**API and UI.** The `/v1/orgs/{org}/environments/{env}/deployments` routes, the
`/deployments` board, its top-nav link and command-palette entry, the overlay fields for
gate deadline and concurrency in the Environment settings, and their entries in
`openapi.json` and the UI's mock mode.

**Plugins.** The `report_deployments` capability, the broker's report ingest path, the
DeploymentReporterContract and the Argo plugin's reporter loop.

**Step-up.** ADR-0073's loosening rules for adding a deployment source and for an
overlay's longer gate deadline.

A Pipeline that still uses `deployment_requests` or `gate: deployment` fails the
Pipeline check with a message that names the replacement (§5).

**What stays.** The step-level `results:` declaration from ADR-0070 §7 stays. Its only
check today is that a `deployment_requests` entry reads a declared result; it moves to
every `${{ outputs.<step>.<name> }}` read of a Step that declares `results:`, so a typo
in a wait Step's input fails the Pipeline check. The older `deployments` table, which
records gate approvals and predates ADR-0070, stays as approval history.

### 4. What is kept from ADR-0070

§9 applies to every gate kind and stays: a gate can fail with a reason, a waiting gate
holds only the Steps that depend on it, each gate has its own clock, and a verdict is
journaled per Attempt. An external gate can still be released with
`{"passed": false, "reason": …}`.

### 5. Waiting until the change is live is a PluginAction Step

The Argo CD plugin offers a PluginAction, `wait-live`. A Deploy Run runs it as a
`plugin_action` Step (ADR-0072 §4) after the Step that makes the change:

```yaml
environment: production
steps:
  - id: bump
    # merges the image change into the gitops repository
    results: [images]
  - id: live
    needs: [bump]
    plugin_action:
      plugin: argocd
      action: wait-live
      with:
        application: payments/payments-api
        images: ${{ outputs.bump.images }}
    timeout: 1800   # seconds
```

**What `wait-live` decides.** It reads the Application on each poll and decides from
its current state, using the rules `assess.rs` and `version.rs` already implement:

- **Passes** when the images in the Application's target manifests (what Argo rendered
  from git, `version.rs`) include every image in `images`, the Application is Synced,
  and it is Healthy. It does not pass on Healthy alone, and not on a sync revision: in a
  shared gitops repository the revision moves with unrelated commits.
- **Fails** when the latest sync operation for the current target revision has failed
  and Argo has stopped retrying, or a sync hook failed. The reason carries Argo's
  message (for example "hook migrate: secret missing"). A failed operation for an older
  revision does not fail it (`operation_is_current`, `assess.rs`). Because this test is
  by revision, a failure that happened between the `bump` merge and the start of the wait
  Step still fails it, and a Retry of the wait Step fails again while that failure is
  current.
- **Times out** otherwise. The reason names the last state the plugin reported.

It cannot tell, from current state, a newer change that replaced the requested images
from one that has not been rendered yet. Both look like "the target does not contain
`images`". That case ends in the timeout, and the reason shows the image set the target
has.

**Polling actions.** A PluginAction may answer an invocation with `pending` (and a
label, such as "sync retry 2/5: hook migrate failed"), `succeeded` (with results) or
`failed` (with a reason). On `pending`, core offers the same invocation again after the
action's declared poll interval.

- **Invocations come from durable rows.** An invocation is offered while its
  `plugin_action` Step has a Running Attempt, so a control-plane restart loses none.
- **Lease.** An invocation a plugin takes is leased for a fixed time. If no answer
  arrives before the lease ends, core offers it again. Several replicas of one plugin
  can therefore pull from the same queue.
- **Fencing.** Every invocation carries its Attempt's fence (ADR-0021). Core drops an
  answer whose fence is no longer current: after a cancel, a Retry or a superseded Take.
- **Labels.** The latest label is stored on the Attempt and shown on the Step; it is
  overwritten, not journaled. State changes (`succeeded`, `failed`) are journaled.
- **Results.** `succeeded` results obey the size limits of a Step's results and are
  read by `${{ outputs }}` like any other.
- **The plugin keeps no state between answers.** It computes each one from the action's
  inputs and the vendor's current state.

**Delivery.** The plugin pulls invocations from the broker with a long poll, under the
projected ServiceAccount token it uses for the broker today. Core maps the token to the
PluginInstallation, so a plugin cannot answer for another installation, and no plugin
has to be a server. This replaces the report push in ADR-0072 §11. The identity
mechanism is unchanged.

**Credentials.** The plugin reads Argo through the CredentialBroker under a credential
issued for that invocation and carrying its fence (ADR-0072 §5 gives write actions such a
credential; this extends it to reads made for an action). The broker chooses the
PluginConnection from the invoking Run's Environment, not from a slot the plugin names,
and applies the action's read rules. The Pipeline never holds the Argo token. This
needs Environment-scoped PluginConnections, which ADR-0072 placed in its MLP and which
are not built: today `plugin_connections` has no Environment column
(`0056_plugin_connections.sql`), and the plugin picks a connection by slot
(`crates/scarab-server/src/broker_proxy.rs`, `/connections/{slot}/…`). A Run with no
Environment gets no connection, so a `plugin_action` that reads through the broker
needs a Deploy Run, and the Pipeline check says so.

**Who may read what.** Any Project eligible for the Environment can read any Application
its Environment's connection can see. The Argo token's own RBAC is the bound; the
connection for `production` should hold a token scoped to the production Argo project.

**Timeouts and an absent plugin.** A `plugin_action` Step launches no Pod, so kubelet
does not enforce its `timeout`. Core enforces it, at the deadline, for this Step kind.

- Run creation refuses a `plugin_action` whose plugin is not installed or does not offer
  the action.
- When an installation is killed (ADR-0072 §10's kill switch) or uninstalled, its
  pending invocations fail at once with the failure class `plugin-unavailable`.
- A plugin that is installed but not answering leaves the Step pending until its
  timeout. The reason says when the plugin last answered.

**Budget and suspension.** A pending `plugin_action` holds no Pod. For the Run budget and
the suspend rule it counts like a waiting gate: the wait is not billed, and a Run whose
only unfinished work is pending actions and waiting gates suspends.

**The verdict.** The plugin decides the Step's outcome, as a container decides its exit
code. Core does not interpret it. The plugin is a digest the org admin approved at
install, so the logic is pinned once for the org rather than per Pipeline.

**A plain container Step remains possible.** A Step is any OCI image (ADR-0008), so a
team can write its own wait Step, or use `helm upgrade --wait` or
`kubectl rollout status`, where the deploy Step blocks until the rollout finishes.

### 6. Concurrency on a deploy is a Run-level concurrency group

The per-DeploymentTarget hold is deleted. A Pipeline that must not deploy one service
twice at once sets a concurrency group (ADR-0032), for example
`concurrency: { group: deploy-prod-payments-api }`. Deploy Runs always queue on their
group (§2).

Two limits follow. A group holds the whole Run rather than one Step. And group keys are
prefixed with the repository (`crates/scarab-server/src/lib.rs:5440-5443`), so two
repositories that deploy the same service no longer serialise against each other, which
the per-DeploymentTarget hold did across the org. An org-scoped group, or a Step-level
group, is later work if a Pipeline needs it.

### 7. The plugin system after this

- **CD knowledge lives in CD plugins.** `scarab-plugin-argocd` holds everything that is
  specific to Argo CD: reading an Application, deciding live or failed, and any future
  Argo action. Support for another CD tool is another plugin next to it. Core keeps no
  CD concept.
- `report_deployments` is removed from the PluginCapability vocabulary.
  `subscribe_events` covers Run events only.
- `offer_actions`, `plugin_action` and Environment-scoped PluginConnections move from
  ADR-0072's MLP into the next slice, because `wait-live` needs them.
- **Write actions** (sync, promote, abort, rollback) are not offered until a new ADR says
  which Project may act on which vendor object. DeploymentTargetProjectOwnership answered
  that before, and it is deleted here.
- UI buttons that invoked actions on a DeploymentTarget go with the DeploymentTarget.

## Consequences

- Core no longer holds deployment state. CONTEXT.md §4.6 shrinks to the Deploy Run,
  and the board goes away.
- A Deploy Run's green means what its Steps checked. A GitOps Pipeline without a wait
  Step is green when the merge lands, as it was before ADR-0070. The docs and examples
  show the wait Step.
- Scarab no longer sees what happens after a Run ends: a deploy that fails an hour later,
  a hotfix applied by hand, or Argo's selfHeal reverting a change. The CD tool's own UI
  and alerts cover those.
- Without the record, a rollback is a Deploy Run like any other.
- `wait-live` decides from Argo's state at each poll, not from an ordered log of
  reports. A state that appears and clears between two polls is not seen, and a newer
  change that replaces the requested images ends in a timeout rather than a named
  failure (§5).
- `plugin_action`, `offer_actions`, Environment-scoped PluginConnections and
  per-invocation read credentials were on ADR-0072's roadmap. Polling answers are new.
- Two repositories deploying one service no longer serialise (§6).
- Pipelines on v0.2.x that use `deployment_requests` or `gate: deployment` stop passing
  the Pipeline check, and the migration drops the deployment tables and their history.
- The first-party Argo plugin changes from a reporter into an action provider.

## Alternatives considered

- **A separate CD product (`scarab-cd`) that holds the record.** Rejected. It builds the
  CD tool this ADR keeps Scarab out of, in another repository, and its gate verdicts
  would make it a second trusted system in the Run path. The deleted code is kept at the
  tag `archive/deployment-record` (`ebafe41`, the last main commit with the record) in
  case a CD product is ever justified.
- **Keep the record and narrow the roadmap** (a non-goal against driving deploys, the
  board moved under Environments). Rejected by the owner: holding a record of what is
  live is the CD scope in question.
- **Move the record into the CD plugin.** Rejected: see Context. It needs plugin storage,
  event subscription and a plugin UI tier, and every CD plugin would rebuild the
  invariants.
- **A first-party container image as the default wait Step.** Rejected as the default. The
  Argo token would be an Environment secret in the Step's environment, each wait would
  hold a Pod, and each Pipeline would pin its own image version. It remains possible
  (§5).
- **The plugin releases an external gate.** Rejected. It needs `subscribe_events` so the
  plugin knows a gate is waiting, and it makes a plugin a gate approver outside the
  two-key rule (ADR-0072 §3). A Step whose outcome the plugin reports fits ADR-0072 §4
  as written.
- **Core marks the deploy live when the deploy Step succeeds** (the GitLab model: one
  record per Deploy Run, version = commit SHA). Rejected: it keeps a record, and with
  GitOps the Step's success is the merge, not the rollout.

## Implementation horizon

1. **Polling `plugin_action`:** `offer_actions`, the `plugin_action` Step kind with
   polling answers, leases and core-enforced timeouts, the broker's invocation long
   poll, Environment-scoped PluginConnections with per-invocation read credentials, and
   the Argo plugin's `wait-live` built on `assess.rs` and `version.rs`. Verified live with
   `just plugin-verify` against the fake Argo it uses today.
2. **The Runs list's `environment` filter** in the API and the Runs tab.
3. **Delete** what §3 lists, as one change with one full gate run, with the Pipeline
   check error for `deployment_requests` and `gate: deployment`. Grep found neither in
   `.scarab/dogfood.yaml` or the example Pipelines, so no Pipeline in the repository
   needs switching.
4. **Docs:** CONTEXT.md §3 (the non-goal), §4.1 and §4.6, and the docs site's
   `environments.mdx`, `overview.mdx`, `deploy-helm.mdx` and `index.mdx`, which mention
   the board and DeploymentGates.

Open:

- Argo's `?refresh=` query asks Argo to refresh the Application before answering. A
  normal refresh shortens the wait; a hard refresh regenerates manifests. The broker's
  query allowlist is per key, not per value, so allowing `refresh` would allow both.
  Decide whether `wait-live` refreshes, and if it does, whether the read rule needs a
  value allowlist.
- The default poll interval and lease length.
- Whether the Pipeline check should warn when a GitOps Deploy Run has no wait Step. It
  cannot tell a GitOps Step from another Step, so this would need a hint the author adds.
