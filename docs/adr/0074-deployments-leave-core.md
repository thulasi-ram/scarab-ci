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
  the point of use, scoped secrets), [0008](0008-step-contract.md) (a Step is any OCI
  image)
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

### 2. A Pipeline or a Step marks itself as a deploy; an Environment only governs

Naming an Environment does not make a Run a deploy. Many things happen inside an
Environment: a smoke test that needs staging secrets, a backup that needs prod
credentials, a deploy. So the two are separate.

**`environment:` governs.** A Run whose Pipeline names an Environment gets:

- admission against the Environment: EnvironmentProjectEligibility, protection rules,
  approvers and `allowed_refs` (ADR-0037, ADR-0071 §2, §3);
- Environment-scoped secrets (ADR-0071 §6) and per-Project privilege grants (§8);
- no newest-wins cancel: it carries no supersede key, and its concurrency group always
  queues (`crates/scarab-server/src/lib.rs:5444-5466`).

**`deploy: true` marks a deploy.** It can be set on the Pipeline, or on a Step:

```yaml
environment: production
deploy: true            # every Run of this Pipeline is a deploy
```

```yaml
steps:
  - id: test
  - id: bump
    needs: [test]
    deploy: true        # this Step is the deploy; the Run is a Deploy Run
```

A **Deploy Run** is a Run whose Pipeline is marked, or which has a marked Step. This is
decided at Run creation from the Pipeline and stored on the Run. A **Deploy Step** is a
marked Step. A gate cannot be marked. Marking a deploy needs no Environment.

A Deploy Run is exempt from newest-wins cancel in the same way as a Run that names an
Environment: it carries no supersede key, and its concurrency group queues even when the
Pipeline declares `cancel-in-progress`. A newer push never cancels a deploy half-way.
The mark changes nothing else.

The Runs list API and the Runs tab filter by `deploy` and by `environment`, in the
database query, and together. Today the Runs tab filters only the page of Runs already
fetched (`ui/scarab-web-ui/src/routes/RepoView.tsx:185`, `:218`), and `GET` on the runs
list takes only `limit`. The Run list marks Deploy Runs, and the Run page marks the
Deploy Step. There is no separate Deployments screen.

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

### 5. Waiting until the change is live is a blocking Step

A Step is any OCI image (ADR-0008), so waiting for the CD tool is an ordinary Step that
blocks until the change is live or has failed. Scarab adds nothing for it. For Argo CD:

```yaml
environment: production          # ARGOCD_AUTH_TOKEN is a production-scoped secret
steps:
  - id: bump
    deploy: true
    results: [image]              # the image reference the change merged
    # ...merges the image change into the gitops repository
  - id: live
    needs: [bump]
    image: quay.io/argoproj/argocd:v2.13.0
    secrets: [ARGOCD_AUTH_TOKEN]
    env:
      - [ARGOCD_SERVER, argocd.example.com]
      - [APP, payments-api]
      - [IMAGE, "${{ outputs.bump.image }}"]
    timeout: 1800
    command:
      - sh
      - -c
      - |
        set -eu
        # 1. Wait until Argo has rendered our change. Right after the merge the app is
        #    often still Synced and Healthy at the previous commit.
        until argocd app manifests "$APP" --source git | grep -qF "$IMAGE"; do
          argocd app get "$APP" --refresh >/dev/null; sleep 10
        done
        # 2. Wait for the sync to finish and the app to be healthy at that revision.
        argocd app wait "$APP" --sync --health --operation --timeout 1700
```

The Step's exit code is the outcome, its log holds Argo's messages, and its `timeout`
bounds the wait. The documented recipe (docs site, "Deploying with Argo CD") covers the
two points this example shows and where plain `argocd app wait` goes wrong: check that
the target contains your image before trusting Synced and Healthy, because in a shared
gitops repository Argo can be Synced and Healthy at an earlier commit; and let the wait
fail when Argo's sync operation fails, rather than waiting out the timeout.

`helm upgrade --wait` and `kubectl rollout status` need no separate wait Step: the
deploy Step itself blocks until the rollout finishes.

The cost of a Step: the Argo token is an Environment secret available to the Steps that
name it, and the wait holds a Pod for its duration. A plugin-resolved gate, where the
Argo plugin watches Argo and passes or fails a gate with Argo's reason, removes both and
is the subject of a later ADR (§7).

### 6. Concurrency on a deploy is a Run-level concurrency group

The per-DeploymentTarget hold is deleted. A Pipeline that must not deploy one service
twice at once sets a concurrency group (ADR-0032), for example
`concurrency: { group: deploy-prod-payments-api }`. A Run that names an Environment
always queues on its group (§2).

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
- **Plugins bring their own storage.** A PluginInstallation may name a
  PersistentVolumeClaim that its operator created in the plugins namespace, and the
  plugin runtime mounts it into the plugin's Pod. A plugin that needs a database reaches
  it through a `delivered` PluginConnection (ADR-0072 §6, not built). Scarab does not
  provision, migrate, back up or read that storage, and it is outside Scarab's durability
  claims. A CD plugin that wants a record of what is live, or a board of it, keeps it
  there and shows it through `contribute_ui` once that tier is built.
- `report_deployments` is removed from the PluginCapability vocabulary.
  `subscribe_events` covers Run events only.
- **Plugins act as Principals, outside Runs.** The direction for the next ADR: a plugin
  is a non-human member of the org that uses the public API under its own Principal and
  RBAC. It reads state, resolves gates it is named on, and keeps its own storage. Core
  never calls a plugin. Work done inside a Run stays a Step, reused through `invoke`.
  Until that ADR, no built capability lets a plugin act; the plugin runtime, broker and
  PluginConnections stay, unused.
- **Write actions** (sync, promote, abort, rollback) are not offered until a new ADR says
  which Project may act on which vendor object. DeploymentTargetProjectOwnership answered
  that before, and it is deleted here.
- UI buttons that invoked actions on a DeploymentTarget go with the DeploymentTarget.

## Consequences

- Core no longer holds deployment state. CONTEXT.md §4.6 shrinks to the Deploy Run
  and Deploy Step, and the board goes away.
- A Deploy Run's green means what its Steps checked. A GitOps Pipeline without a wait
  Step is green when the merge lands, as it was before ADR-0070. The docs and examples
  show the wait Step.
- Scarab no longer sees what happens after a Run ends: a deploy that fails an hour later,
  a hotfix applied by hand, or Argo's selfHeal reverting a change. The CD tool's own UI
  and alerts cover those.
- Without the record, a rollback is a Deploy Run like any other.
- Every team writes, or copies, its wait Step. Its correctness is the author's: a wait
  that trusts Synced and Healthy alone can pass at an earlier commit (§5).
- The Argo token for a Deploy Run is an Environment secret, and each wait holds a Pod.
- After this ADR no built plugin capability does anything. The plugin system stays,
  unused, until the plugin-as-Principal ADR (§7).
- Two repositories deploying one service no longer serialise (§6).
- Pipelines on v0.2.x that use `deployment_requests` or `gate: deployment` stop passing
  the Pipeline check, and the migration drops the deployment tables and their history.
- The first-party Argo plugin loses its reporter. Its code for reading Argo and judging
  an Application's state stays in `scarab-plugin-argocd` for the plugin-resolved gate.

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
- **A polling `plugin_action` Step** (`wait-live` run by the Argo plugin: core offers the
  invocation, the plugin answers pending, succeeded or failed, under leases, fences and
  core-enforced timeouts, with Environment-scoped broker credentials). Rejected on
  2026-10-28 after it was partly built: it adds dispatch, leasing and timeout machinery
  to core to avoid one Pod and one secret, which works against the minimal-core goal. The
  partial build is on branch `impl/adr-0074-plugin-action`, unmerged.
- **The plugin releases an external gate.** Deferred to the plugin-as-Principal ADR
  (§7), with the plugin named as the gate's releaser so the two-key rule (ADR-0072 §3)
  holds.
- **Core marks the deploy live when the deploy Step succeeds** (the GitLab model: one
  record per Deploy Run, version = commit SHA). Rejected: it keeps a record, and with
  GitOps the Step's success is the merge, not the rollout.

## Implementation horizon

All in one PR (#41):

1. **`deploy: true`** on Pipelines and Steps, stored on the Run, its exemption from
   newest-wins cancel, and the Runs list's `deploy` and `environment` filters in the API
   and the Runs tab.
2. **Delete** what §3 lists, as one change with one full gate run, with the Pipeline
   check error for `deployment_requests` and `gate: deployment`. Grep found neither in
   `.scarab/dogfood.yaml` or the example Pipelines, so no Pipeline in the repository
   needs switching.
3. **Plugin storage:** the PersistentVolumeClaim reference on a PluginInstallation and
   its mount in the plugin runtime (`crates/scarab-plugin-k8s`).
4. **Docs:** CONTEXT.md §3 (the non-goal), §4.1, §4.6 and §4.7; the docs site's
   `environments.mdx`, `overview.mdx`, `deploy-helm.mdx` and `index.mdx`; and a
   "Deploying with Argo CD" page with the wait Step from §5.

Next, in its own ADR: plugins as Principals, starting with a gate the Argo plugin
resolves (§7).

Open:

- Whether the Pipeline check should warn when a Deploy Run has no Step after its Deploy
  Step. It cannot tell a GitOps Step from another Step, so the warning would be a hint.
