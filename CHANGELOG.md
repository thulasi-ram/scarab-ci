# Changelog

What changed in each Scarab release. The layout follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

Each release sorts its entries under these headings, in this order, and leaves
out the empty ones (the headings follow
[Woodpecker CI's changelog](https://github.com/woodpecker-ci/woodpecker/blob/main/CHANGELOG.md)):
**Breaking changes** (what to do before or after you upgrade), **Security**,
**Features**, **Enhancement**, **Bug Fixes**, **Tests**, **Documentation**,
**Dependency** and **Misc** (other changes for people working on Scarab itself).
A pull request with a change users can see adds a line under `[Unreleased]`.
Every entry under Features links the docs page that explains it;
`just release <version>` fails on one that doesn't. When a `v*` tag is pushed,
release.yml uses that version's section as the GitHub Release text. See
[docs/RELEASING.md](docs/RELEASING.md).

## [Unreleased]

Adds libraries and plugin gates through `invoke`, a policy engine for Run creation, and
checked forge credentials for Steps. Removes the `clone:` Step kind and the deployment
record. Read Breaking changes before you upgrade.

### Breaking changes

- **Upgrade with `Recreate`.** Old and new servers must not run at the same time. A 0.2.x
  server ignores the scope this release stores on library Steps, and would fill a library
  Step's `${{ inputs.* }}` from the Run's parameters. The chart's server Deployment now
  defaults to `strategy.type: Recreate` (#50). If you set `strategy` to `RollingUpdate`, set
  it back for this upgrade. The API is down while the server restarts.
- **The `clone:` Step kind is removed.** A Pipeline with a `clone:` Step fails its check, and
  the error names the replacement:

  ```yaml
  - id: checkout
    invoke: lib:thulasi-ram/clone
    with: { depth: "1" }      # or a number, or "full"; also submodules, lfs
  ```

  `lib:thulasi-ram/clone` is a first-party library. The server reads the list of first-party
  libraries from this release's `libs-manifest.json` asset and installs them in an org when a
  repository is first bound to it. For an org that existed before the upgrade, open
  **Settings › Libraries**: when the defaults are missing, use **Retry**. A server that
  cannot reach github.com can use a mirror, pasted files, or `SCARAB_LIBS_MANIFEST` (a file
  path, an https URL, or `off`).
- **A bare `invoke:` path is an error.** Write `invoke: file:<path>` for a library in the same
  repository. The file must be marked `kind: library`.
- **Values from outside the Pipeline go through `env:`.** Run parameters, `event` fields, Step
  outputs and library `with:` values that read them may appear in `command` only as a listed
  `choice`, `true`/`false`, a non-negative number or a hex commit SHA. This is checked when
  the Pipeline compiles, in the preview, and again at launch. Pass any other value through
  `env:` and read it as a variable in your script.
- **Private submodules no longer get the clone token.** The token is offered only for the
  repository being cloned, over https. A private submodule fails the clone with
  `SubmoduleUnavailable`. Make the submodule public or vendor it.
- **The deployment record is removed** (ADR-0074). Migration `0067_drop_deployment_record.sql`
  drops the deployment tables, and the `/deployments` board, DeploymentGate and
  `report_deployments` are gone. **Deployment records stored by 0.2.x are deleted.** Mark a
  Deploy Run with `deploy: true` on the Pipeline or a Step; the Runs list filters by it and by
  Environment. Waiting until a change is live is a Step in the Pipeline (see the Argo CD page
  in the docs).
- **`$` reaches the container as written.** Kubernetes expanded `$(VAR)` in `command`, `args`
  and `env` values, and turned `$$` into `$`. Scarab now escapes every `$`, so
  `sh -c 'echo $$'` prints the shell's PID on Kubernetes, as it does under the local
  executor. A script that relied on `$(VAR)` expansion by Kubernetes must read the variable
  itself (`$VAR`).
- **Step Pods no longer mount the namespace's ServiceAccount token.** Step, service and debug
  Pods set `automountServiceAccountToken: false`. A Step that calls the Kubernetes API gets a
  ServiceAccount through a placement profile.
- **New limits.** A CEL expression is at most 4096 bytes with brackets nested at most 64 deep.
  A Pipeline source file is at most 1 MiB, its compiled form at most 16 MiB, with at most
  1,024 Steps and gates. `event.sha` must be lower-case hex.
- **`SCARAB_WORKSPACE_CONTROL_URL` is no longer read.** The control plane reaches the Data
  Depot on `SCARAB_WORKSPACE_URL`, which it already did when the variable was unset.
- **At `workspace.replicaCount` above 1,** the chart renders one Service per Depot replica
  (`<fullname>-workspace-<ordinal>`), and Caches saved before the upgrade miss once: each
  Cache now has a home replica. Change the replica count with `workspace.replicaCount` and an
  upgrade, not `kubectl scale`: the server reads the count from `SCARAB_WORKSPACE_REPLICAS`,
  which the chart copies from that value.
- **Upgrade the Data Depot before the server, or accept a short window.** During a rolling
  upgrade an older Depot refuses a newer server's workspace token when the token carries cache
  roots, so a Step with a Cache restore can fail with a Transient error until both are
  upgraded.
- **A forge connection serves one org** (git-bug 19b0a45). Every repository bound through a
  connection must be owned by its org.
  - `POST /v1/connections` needs an `org` field. Only a global admin creates an org's first
    connection; after that, the org's admins can add more. The server checks the org name when
    a connection is created or assigned, and at boot for the `connections:` config: not empty,
    at most 100 characters, no `/`, spaces or control characters, not `.` or `..`, not starting
    with `_`, and not `settings`, `orgs`, `account` or `security`, which are the first part of
    web UI addresses (400, or a refused boot).
  - `GET /v1/connections` lists only the connections of orgs you administer (a global admin
    sees every one) and takes `?org=<org>` to list one org's.
  - Nothing assigns an org to a connection as a side effect. Opening Add repository, a re-sync
    or a bind leaves an unassigned connection unassigned, and a re-sync through it binds
    nothing: every repository the forge reports is listed in `skipped`. The UI does not offer
    Re-sync on it.
  - New: `PUT /v1/connections/{id}/org` with `{ "org": "<name>" }` assigns an unassigned
    connection, for global admins only (403 for anyone else, API tokens included). It answers
    409 with the reason when a repository bound through the connection is owned by another
    account, when the org is served by another forge or host, when the org exists with no
    connection, or when the connection is managed by configuration. It asks for step-up
    confirmation (`connection.assign_org`) and is recorded in the org's event log. In the UI it
    is **Assign to org** on the Unassigned connections panel.
  - A repository moves only off a connection that does not serve its owner: an unassigned one,
    or one of another org that holds it. Binding it through a second connection of the same org
    is refused: 409 on Add repository, listed in `skipped` on re-sync. Unbind it from the
    connection that holds it first. Add repository can now move a repository off such a
    connection, as re-sync already could.
  - Removing a connection that holds a repository of another org is refused (409, naming the
    repositories), with or without `unbind_repos=true`. That org takes them back by re-syncing
    or adding them on its own connection; then the removal goes through.
  - A connection declared in the `connections:` config needs `org:`, unless all its `repos:`
    have one owner. A repo of another owner, repos of several owners without `org:`, no repos
    and no `org:`, or a repo already bound through another connection that serves an org fail
    the boot. So does an `org:` that differs from the org the connection already serves, a
    change of case included: config does not move a connection to another org. To move one,
    remove it from the config, unbind its repositories, and add it again.
  - In GitHub App mode (`SCARAB_GITHUB_APP_ID` set), a `kind: github` connection in the
    `connections:` config needs `installation_id:`, unless its id is `github-install-<id>`;
    the boot is refused without it. The connection mints tokens from that installation and
    lists only its repositories.
  - A private library is read only through a connection of the installing org, and only for
    repositories that org owns.

  **Upgrade note.** Migration `0076_org_connections.sql` gives each existing Forgejo or token
  connection the org of its repositories when they all have one owner. A connection whose
  repositories have several owners, or none, stays unassigned: only a global admin sees it or
  acts on it, and its repositories keep working as before. **Every GitHub App installation
  connection (`github-install-<id>`) comes up unassigned**, whatever its repositories: the
  database does not hold the installation's account, and an earlier re-sync could bind another
  installation's repositories to it. It keeps looking up each repository's installation, so its
  repositories keep getting tokens and their Runs keep working. It gets an org in one of two
  ways: a global admin assigns it, or its next `installation` delivery does, when every
  repository bound through it is the account's and the org of that name is new or is already
  served by a GitHub connection with the same account id. After the upgrade no connection has
  an account id yet, so for an existing org the global admin assigns it. The migration also
  records every org that has members, API tokens, secrets, Environments, Policies, Plugins or
  libraries, not only those with a repository. After the upgrade, as a global admin, open
  Settings › Connections:
  1. Assign each unassigned connection to the org that owns its repositories with **Assign to
     org**.
  2. For a connection whose repositories belong to several accounts, re-sync each account's
     own connection (for GitHub, its installation connection; for Forgejo, add one for that
     org first). Its repositories move there with their Projects. Then
     assign the old connection or remove it. Do not remove a connection that still has bound
     repositories: that removes their Projects.

  During a rolling upgrade, `/v1/me` `admin_orgs` and the connection routes also count orgs
  that only a repository binding names, so admins of an org bound by an older server are not
  refused. A server built before this release does not start against a database where 0076 has
  run (sqlx refuses an unknown migration). To roll back, restore the database from before the
  upgrade, or delete row 76 from `_sqlx_migrations`; the added columns and table do not affect
  the older server.

- **Each pipeline posts its own commit status.** The status context was `scarab` for every
  Run, so two pipelines on one commit overwrote each other and the commit showed whichever
  finished last. It is now `scarab/<pipeline>` (the pipeline's `name:`, or its file name
  without `.scarab/` and the extension). If a branch protection rule requires the `scarab`
  check, require `scarab/<pipeline>` for each pipeline instead before you upgrade, or the rule
  waits for a status that is no longer posted.

- **Deploys into an Environment with approvers now pause for approval when the pipeline has no
  gate** (git-bug cd43bf3). Before, a Run that named such an Environment and had no `gate:
  manual` gave its Steps the Environment's secrets and grants without asking anyone. Now the
  Environment adds one `manual` gate, `environment-approval`, in front of the earliest Steps that
  use its authority (declared `secrets:`, a governed grant or `k8s_overlay`, `forge_token: write`,
  or `deploy: true`). Runs that deployed unasked start waiting for an approver on the day you
  upgrade. An org admin can set an Environment to refuse such a Run instead (missing-gate mode
  `refuse`), which names the gate to add. See
  [Environments](https://thulasi-ram.github.io/scarab-ci/usage/environments/#the-approval-gate).
- **`manual` gates that guard no authority Step stop needing the approvers.** Only a gate with a
  Step after it that uses the Environment's authority needs the Environment's approvers and a
  step-up; any other `manual` gate in the Run releases on the first approval by anyone with write.
- **The Environment's wait timer is enforced** (git-bug 2b94989): the approval gate releases when
  its approvals are in and the timer has run out, counted from when its `needs` finished. An
  approval given early counts. The API refuses a non-zero wait timer on an Environment with no
  approvers; one stored before does nothing.
- **A stale approval is refused at launch** (git-bug 220917f). Every launch of a Step that uses
  the authority of an Environment with approvers, reruns and retries included, fails `Config`
  when no released approval gate is upstream of it, the Run's ref is no longer allowed, the
  approvers changed since the gate's approvals, or the gate released more than the Environment's
  maximum approval age ago (24 hours by default). An external gate no longer hands over the
  Environment's authority by its token alone. A Run created before its Environment had approvers
  has no approval gate, so its authority Steps are refused once the Environment has approvers.
  In such an Environment, Steps before the approval gate get the `env=none` OIDC subject.

### Security

- An admin of one org could list, delete, re-sync and bind repos through another org's forge
  connections, and an API token scoped to one org could do the same (git-bug 19b0a45). Each
  connection now belongs to an org, and its routes need `Administer` on that org; another
  org's connection answers 404. A GitHub App connection that serves an org mints tokens only
  from its own installation and lists only that installation's repositories, so a re-sync no
  longer binds repos of other installations; an unassigned one keeps looking up each
  repository's installation. A bind no longer moves a repo bound in another org.
  A connection with no org gets one from an `installation` delivery only when every repository
  already bound through it is owned by the installation's account, and the org of that name
  either does not exist or is already served by a GitHub connection to the same host with the
  same account id (`installation.account.id`, which the delivery now records). A GitHub login
  can be renamed and registered again by another account; the id cannot, so the new holder of a
  login does not join the org. An org exists once anything is kept for it (members, an API
  token, a secret, an Environment, a Policy, a Plugin, a library), so an org with no connection
  is not joined by name either. Otherwise only a global admin assigns one, and the assignment is
  checked and written under a lock on the org, so a concurrent assign or bind cannot slip in
  between. An App connection that serves an org but is not pinned to an installation lists only
  that org's repositories, not every installation's. `connection.assign_org` cannot be raised
  as a step-up request by an API token.
  No read, re-sync or bind assigns one. Through a connection that has an org, tokens are
  minted, libraries read and repositories unbound only for repositories that org owns
  (unbinding another owner's repository answers 404). A library source naming a connection
  that does not exist and one naming another org's connection get the same answer (403). A
  repository moves between connections only off a connection that does not serve its owner,
  and a connection's removal never takes another org's repository with it.
- The clone image's askpass gave the token to an LFS server named in `.lfsconfig`, to other
  repositories on the same host through submodules, and over `http://` (#53).
- A value from outside the Pipeline holding `$(SECRET_NAME)` was filled with that secret by
  Kubernetes (#52). Plugin Deployments and probe Pods are escaped the same way.

### Features

- **Environment display name and Duplicate.** An Environment's name stays fixed: pipelines and
  the OIDC subject (`…/env/<name>/…`) use it. It now has an optional display name, which lists,
  headings and pickers show with the name beside it; changing it needs no step-up. **Duplicate**
  (`POST /v1/orgs/{org}/environments/{env}/duplicate`, step-up every time) creates a new
  Environment with the old one's baseline, eligibility, every project's overlay and privilege
  grant, and its secrets with their values, and lists what it did not copy: the projects whose
  Runs used it, the policies that name it, and the OIDC subject cloud trust policies match. The
  old Environment is not changed. Migration 0080 adds the display name column (git-bug d002a0c).
  [Environments](https://thulasi-ram.github.io/scarab-ci/usage/environments/#duplicate-an-environment)
- **Approval gate settings on the Environment page.** The edit page has an **Approval gate**
  section: what to do when a pipeline has no approval gate (**Add an approval gate** or **Refuse
  the Run**), the added gate's expiry, how many approvers must approve, and the maximum approval
  age. The wait timer is disabled while the Environment has no approvers, and an Environment
  with a timer and no approvers shows a warning (git-bug cd43bf3, 2b94989).
  [Environments](https://thulasi-ram.github.io/scarab-ci/usage/environments/#the-rules-it-holds)
- **The Run page marks Environment approval gates.** Their gate card, DAG node and step pane say
  **Environment approval gate** and name the Environment by its display name; the gate the
  Environment added says **added by** it. After the approvals are in, a gate with a wait timer
  shows when it releases, and approving it says the approval was recorded and when the gate
  releases. `GET /v1/runs/{id}` gains `environment` (`name`, `display_name`) (git-bug cd43bf3,
  2b94989).
  [Environments](https://thulasi-ram.github.io/scarab-ci/usage/environments/#the-approval-gate)
- **Environment page wording.** "Who may deploy" is now "Projects that may use <Environment>":
  the list admits every Run that names the Environment, not only deploys. The "Projects" panel
  is now "Project overlays and grants" (git-bug c2f97b4).
  [Environments](https://thulasi-ram.github.io/scarab-ci/usage/environments/#view-and-edit-an-environment)
- **Settings › Policies.** One read-only page per org lists its CEL Policies, with what each
  applies to and how often it matched, denied and (in audit mode) would have denied in the
  newest 200 decisions, the newest decisions, and the typed settings (each Environment's rules,
  the library and plugin lists, API token scopes) with links to where each is edited (git-bug
  c2f97b4). [Environments](https://thulasi-ram.github.io/scarab-ci/usage/environments/#every-rule-in-one-place)
- **Runs after another pipeline.** `on: upstream: { pipeline: ci }` starts a Run when a Run
  of `ci` in the same repository finishes (`status:` takes `succeeded`, the default, and
  `failed`). The follow-up builds the same commit and runs on the same ref, so an
  Environment's allowed refs apply to it. A newer follow-up on the same branch cancels an older
  one, as a push does. A fork pull request's Run starts none, and a pipeline already in the
  chain of follow-ups is a refused Run. This lets a build and its deploy be two pipelines, so
  newest-wins still cancels stale builds and a rejected deploy gate no longer fails the build.
  Other repositories cannot be followed. Before, `on: upstream` passed the Pipeline check and
  never fired (git-bug 149b4db).
  [Pipeline syntax](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#after-another-pipeline-on-upstream)
- **Scheduled Runs.** `on: cron: { schedule: "0 2 * * *" }` (one expression or a list, UTC,
  at most every 5 minutes) starts a Run from the head of the default branch. At each slot the
  server reads the default branch, its head and the Pipeline from the forge again, so a schedule
  removed or a default branch renamed takes effect even when its webhook was lost. A slot the
  forge could not be read for is listed with the refused runs. The repository page lists each
  schedule's next and last fire. Forge reads use conditional requests, and a rate-limited
  forge is left alone until it resets. Migration 0078 adds two tables.
  [Pipeline syntax](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#scheduled-runs-on-cron)
- **Libraries** (ADR-0076). `invoke: file:<path>` for a library in the repository,
  `invoke: lib:<publisher>/<name>` for one installed in the org. An org admin installs a
  library from a git source pinned to a commit: fetch, review the diff, then install by
  content hash (step-up required). Templates render once, at launch, from each invocation's
  own scope: `inputs` are its `with:` values. The Run DAG draws a box around each
  invocation's Steps, and the Step pane shows its origin.
  [Libraries](https://thulasi-ram.github.io/scarab-ci/usage/libraries/)
- **Yank.** An org admin can yank a library version. Run creation, every Step launch (Reruns
  and Retries included), gate approval and release, and the debug Pod refuse it, with the
  yank's reason. Lifting a yank needs step-up.
  [Yank a version](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#yank-a-version)
- **`forge_token: read | write`** on a Step: a credential minted for each Attempt and mounted
  as files under `/scarab/secrets/`, never as an environment variable. On a connection with
  one long-lived token (a Forgejo or GitHub PAT), only Steps from the first-party clone
  library or a source the org trusts receive it; a repository's own Steps never do. Fork pull
  requests get `read`. `write` needs an Environment that restricts the refs it deploys from.
  [Forge credentials](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#forge-credentials-forge_token)
- **Plugin gates.** `invoke: plugin:<installation>/<gate>` compiles to a gate that the plugin
  releases with typed outputs, such as the Argo CD plugin's `wait-live`.
  `gate_expires_after` is required.
  [Plugin gates](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#plugin-gates)
- **Policy engine** (ADR-0075). Org and Project Policies checked when a Run is created, an
  audit mode, a decision log, a policy test API, and `scarab validate --server`.
  [ADR-0075](https://thulasi-ram.github.io/scarab-ci/tech/adr/0075-policy-engine/)
- **Refused Runs.** A trigger that does not become a Run (a compile error, a yanked library, a
  Policy, an Environment, an `event` value that fails its check, a refused `forge_token`) is
  recorded and listed on the Project's Runs page with the reason.
  [ADR-0075 §9](https://thulasi-ram.github.io/scarab-ci/tech/adr/0075-policy-engine/#9-decisions-are-recorded-and-use-the-current-policies)
- **Checked `event` values.** `${{ event.* }}` reads only allowlisted fields, each checked at
  Run creation; a Pipeline that reads a value that fails its check is not run. For example, a
  branch named with `;` refuses only the Pipelines that read `event.branch`.
  [Trigger values](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#trigger-values-event)
- **More than one server replica.** `replicaCount` above 1 is supported, with converged or
  split roles. A replica renews its claim on a Step's launch every 10 s while it works on it,
  and a replica stops driving a launch within one renewal period after it loses its claim
  (git-bug 068cc0b); when a replica dies, a peer takes over its Steps within 30 s. The
  scheduler lease has a term that every takeover increments. A leader write that has no
  compare-and-set of its own checks the term inside its own transaction, bounded at 5 s, so a
  leader that lost its lease cannot commit one after a peer took over (git-bug aaf4e3e). Tested against Postgres, and on a local kind
  cluster the fault tier's `two-replicas`, `kill-server` and `stop-leader` scenarios passed
  (`just test chaos all 3`, 2026-11-01).
  [ADR-0077 amendment](https://thulasi-ram.github.io/scarab-ci/tech/adr/0077-end-to-end-fault-and-agent-driven-testing/#amendment-2026-11-01-grill)
- **Event log page** (git-bug fa23028). An org's security event log moves from Settings →
  Security to its own section, Settings → Event log, one page of 50 at a time. It filters by
  event kind, actor, target text (a Run id, a project or a secret name) and time range, and
  the filters are in the URL. The event log API takes `cursor`, `actor`, `target`, `from` and
  `to`; `before` still works.
  [ADR-0073 §7](https://thulasi-ram.github.io/scarab-ci/tech/adr/0073-step-up-confirmation/#7-orgsecurityeventlog)
- **Settings sidebar** (git-bug 734ffa1). Settings is one page with a sidebar in up to three
  groups, and each section has its own address: Your account under `/settings/<section>`
  (Profile, Security, Appearance), an org's settings under `/orgs/<org>/settings/<section>`, and
  Server administration under `/settings/server/<section>` for global admins, where Connect a
  forge creates a new org's first connection. You see only the groups you may use, and an admin
  of several orgs switches org in the sidebar. `/settings`, `/account`, `/account/security` and
  `/settings/libraries/review` redirect to the new addresses, keeping their query.
  [Settings](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#settings)
- **Mono palette.** Settings › Appearance picks the palette, Emerald or Mono (greys), in light
  and dark mode. The choice is kept per browser.
  [Appearance](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#appearance)
- **Environment pages** (git-bug c11abc5). An Environment opens read-only at
  `/orgs/<org>/settings/environments/<name>`, and its **Edit** button opens a separate edit
  screen for the description, baseline, eligibility, privilege grants and Delete. One Save sends
  only the parts that changed. Delete and withdrawing a grant ask you to type the name. An
  Environment cannot be renamed: its name is the key pipelines use.
  [View and edit an Environment](https://thulasi-ram.github.io/scarab-ci/usage/environments/#view-and-edit-an-environment)
- **Personal API tokens** (git-bug a4cb94f). You mint, list and revoke your own tokens under
  Settings › Your account › API tokens (`/v1/me/tokens`). Anyone with a role in an org or
  project, viewers included, may mint a token there up to that role; minting still needs a
  passkey confirmation, and a token still cannot mint tokens or revoke them through
  `/v1/me/tokens`. Organization settings › API tokens lists every token issued in the
  org with its owner, last use and call count, sorted by last use (the default), calls or
  creation, and an org admin can revoke any of them; it no longer mints.
  `POST /v1/orgs/{org}/tokens` still works and mints the same personal token. The call count is
  approximate: calls are written at most once a minute per token. An org policy for who may
  issue tokens, with which scopes and lifetimes, is not available yet.
  [API tokens](https://thulasi-ram.github.io/scarab-ci/usage/api-tokens/)
- **`scarab.uiUrl` (`SCARAB_UI_URL`)**, for when people open the web UI at another address
  than the one forges reach (`publicUrl`). The login callback, the passkey origin and the
  step-up confirmation link use it; webhook URLs and commit-status links stay on `publicUrl`.
  Empty means `publicUrl`. A sign-in started on `127.0.0.1` while the UI URL says `localhost`
  is sent to `localhost` first, so it keeps its cookie.
  [The UI URL](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#the-ui-url)
- **`scarab-server admin grant --org --subject --role [--if-absent] [--reason]`** gives a
  person a role in an org straight in the database, for an install's first members. It needs
  only the database URL, never migrates, and records `binding.operator_grant` in the org's
  event log with the host, OS user and reason, in the same transaction as the binding. It sets
  the role exactly: a higher role is lowered and a revoked one restored. With `--if-absent` it
  writes nothing when the person already has a binding in the org, a revoked one included. A
  person who has never signed in is refused (exit 3).
  [Grant the first roles](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#grant-the-first-roles)

### Enhancement

- **Caches at more than one Depot replica.** Each Cache key has one home Depot replica, chosen
  by rendezvous hashing over the StatefulSet's ordinals, and a Step saves and restores the
  Cache there. Before, a Cache's saves and restores scattered over the replicas, so a restore
  almost always missed (git-bug 12e2f6b). A home that cannot be reached is a miss, never a
  failed Step. Changing the replica count moves about 1/N of the keys, and each moved key
  misses once. `workspace.homeUrlTemplate` overrides the per-replica address.
  [ADR-0077 amendment](https://thulasi-ram.github.io/scarab-ci/tech/adr/0077-end-to-end-fault-and-agent-driven-testing/#amendment-2026-11-01-grill)
- **One install flow for plugins and libraries** (git-bug 3741861). Every catalog plugin
  installs through the same form, with its publisher shown as a field. Every library installs
  through the review page, first-party or not, and the libraries in Scarab's release manifest
  are offered there as sources. The one-click **Install first-party libraries** button is gone:
  an existing org installs each first-party library on its own, with a review and a passkey
  confirmation.
  [Install a library](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#install-a-library)
- `GET /v1/me` adds `global_admin`: whether you hold a global `Administer` role, which the UI
  uses to show Server administration. It is always `false` for an API token.
- A connection's re-sync result adds `skipped`: the repositories the forge reports that it did
  not bind, because another account owns them, another connection that serves an org holds
  them, or the connection serves no org (then every repository is listed).
- The server and CLI now link to `/settings/profile` and `/settings/security?enroll=1`
  instead of `/account` and `/account/security?enroll=1`. The UI still redirects the old
  addresses.
- **Inherited secret keys on a repository's Secrets tab.** The list now shows, in grey after
  the repository's own keys, the org's keys the repository can use ("from org") and keys set
  only in some environments ("only in production"). A repository key that replaces an org key
  reads "overrides org". Names only; no value is shown. The secret matrix response adds
  `overrides`, which names the broader scope a set cell hides.
  [Secrets](https://thulasi-ram.github.io/scarab-ci/usage/secrets/#where-you-set-them-one-grid-by-scope)

### Bug Fixes

- A Run was visible to the scheduler before its parameters, `when` exclusions and concurrency
  group were written; a Step could start without them (#51). A Run left half-created by a
  server that stopped is failed after 10 minutes.
- **Caches never saved or restored.** No Run created from a repository had the project the
  scheduler keys Caches by, so every Step's `cache:` was off. Runs now carry `<org>/<repo>`.
  The per-project cap (ADR-0011) keys by the same value. It never applied before, and it
  stays off unless you set it: `scarab.projectRunCap` (`SCARAB_PROJECT_RUN_CAP`) is the most
  Runs of one repository that may be Running at once. A Run waiting on a gate is not
  counted.
- A dispatch parameter holding `${{ … }}` was evaluated as an expression (#49).
- A deep or long CEL expression in a pushed Pipeline could overflow the stack and stop the
  server (#44).
- Environment re-admission runs before any checkout token or registry credential is minted; a
  timer gate fails when the Environment no longer admits the Run.
- A NUL byte in a dispatch reason, commit subject, PR title or Step result no longer causes a
  500.
- At more than one Depot replica, a Step whose token named a Cache another replica held got a
  500 on its required inputs. A cache root a replica cannot walk is now left out of the grant
  (git-bug 2a79b73), and a later refused read of that token is answered without walking its
  inputs again.
- **The Depot did not start with named login providers.** With `scarab.oauth.providers` set,
  the chart gave the providers' client secrets to the server Deployment only, and the Depot
  StatefulSet refused to boot for want of them. Both get them now.

### Tests

- The fault tier (`just test chaos`, ADR-0077) runs ranks 1, 2 and 4: a Depot Pod deleted
  mid-drain and mid-feed, two server replicas sharing the work, a server replica killed while
  it drains, and the scheduler leader paused past its lease. All six scenarios passed on
  2026-11-01 (`just test chaos all 3`). `pause-node`, last in `all`, freezes the kind node
  mid-drain for longer than the lease TTL and shorter than the Pod's grace period; every Run
  must still succeed. C1 checks that a Cache restores from its home replica when no fault
  hit that replica.

### Documentation

- The Installation page describes forge connections per org (Settings › Connections, Connect a
  forge, `org:` in `scarab.connections`, a connection with no org) and the settings page with
  its addresses. The Libraries, Secrets and Environments pages name the new settings addresses,
  the per-org connection rule for private library sources, inherited secret keys, and the
  Environment view and edit pages.

### Misc

- `just stack up dogfood` signs in with GitHub instead of running dev-insecure: identity store,
  role bindings, API tokens and passkey step-up, as a real install. It needs
  `SCARAB_OAUTH_CLIENT_ID` and `SCARAB_OAUTH_CLIENT_SECRET` in `dogfood.env` and
  `SCARAB_ADDR` on `127.0.0.1`, grants `DOGFOOD_OWNER` and `DOGFOOD_MEMBER` their roles when
  they have none in the org, and prints the fresh-database steps that now need a passkey, or
  the clone library step when the installed copy differs. `just stack seed dogfood` runs the
  grants again. `just stack demo` refuses dogfood mode. `SCARAB_DOGFOOD_DEV_INSECURE` is gone.
  See `deploy/kind/README.md`.
- The local stack is one kind cluster, `scarab-dev`, with one Helm release in four modes
  (dev, dogfood, e2e, chaos), each with its own database and bucket; Forgejo runs in it.
  `deploy/local-helm`, `deploy/local-proc` and the colima k3s setup are gone. The chart gains
  `command`, `args`, `extraVolumes` and `extraVolumeMounts` for the server and the workspace
  container, and `service.nodePort`, which the kind stack uses.
- The Justfile is split into modules, `just stack …`, `just test …` and `just check …`, and
  needs just 1.43.0 or later. Renamed recipes:
  - `just up`, `down`, `reload`, `logs`, `demo` and `demo-chain` are `just stack up`,
    `stack down`, `stack reload`, `stack logs`, `stack demo` and `stack demo chain`;
    `just demo-oracle` is `just stack up demo-oracle`.
  - `just workspace-logs` is `just stack logs depot`; `just workspace-status` is
    `just stack status`.
  - `just test-one <filter>` is `just test rust <filter>`; `just test` runs the Rust suite and
    then the UI no-DOM suite.
  - `just e2e` is `just test e2e`, and `just plugin-verify`, `just plugin-kube-tests`,
    `just forgejo-verify` and `just kube-tests` are folded into it.
  - `just chaos` is `just test chaos`; `just coverage` is `just test coverage`.
  - `just ui-mock` is `just ui mock`; `just ui-test` and `just ui-e2e` are `just test ui`.
  - `just docs-build` is `just docs build`.
  - `just pr-gate <n>` is `just pr <n>`; `just sweep` and `just disk` are `just clean`.
  - `just openapi-drift` and `just chart-immutable` run in `just check all`;
    `just policy-conformance` is `just check conformance`.
  - The overlay recipes (`just adr0062-substrate`, `just overlay-tests`,
    `just overlay-tests-colima`) are deleted.

  `just release <version>` checks a release before it is tagged.
- The ADR-0062 overlay change-set code, its tests and its recipes are deleted; ADR-0066 had
  already dropped the feature.

## [0.2.2] - 2026-10-28

### Breaking changes

- **From 0.2.1 or earlier, recreate the workspace StatefulSet once.** Delete it without
  deleting its Pod or PVC, then upgrade:

  ```sh
  kubectl -n <ns> delete sts --cascade=orphan \
    -l app.kubernetes.io/name=scarab-workspace,app.kubernetes.io/instance=<release>
  helm upgrade <release> oci://ghcr.io/thulasi-ram/charts/scarab --version 0.2.2 ...
  ```

  The new StatefulSet re-adopts the existing PVC by name (`cas-<sts>-0`); no data is lost.
  Expect one workspace Pod restart. For Argo CD: run the delete, then sync.

### Bug Fixes

- **Chart upgrades were rejected by the API server.** The workspace StatefulSet's
  `volumeClaimTemplates` carried `helm.sh/chart` and `app.kubernetes.io/version` labels. That
  field is immutable, so every chart upgrade since v0.1.0 failed with
  `StatefulSet.apps "...-workspace" is invalid: spec: Forbidden: updates to statefulset spec for fields other than ...`
  (and Argo CD syncs failed). The template now carries only version-free labels (#38).

### Misc

- A CI guard (`scripts/chart_statefulset_immutable.sh`) renders the chart at two versions
  under default and feature-on values and fails if any immutable StatefulSet field differs.

## [0.2.1] - 2026-10-27

### Features

- **Versioned docs** (#36). Each release line is published at
  `https://thulasi-ram.github.io/scarab-ci/vX.Y/`, the latest stays at the root, and a version
  dropdown sits in the header. This is the first versioned publish.

No server, chart or API behaviour changes.

## [0.2.0] - 2026-10-27

### Features

- **Deployments**: vendor-neutral Deployment requests with deployment gates and concurrency
  holds (#32).
- **Step-up confirmation** (ADR-0073): passkeys, the step-up guard, requests and a security
  log (#32).

### Enhancement

- **S3 ambient credentials** (#34). Leave `SCARAB_S3_ACCESS_KEY` and `SCARAB_S3_SECRET_KEY`
  unset to use AWS's credential chain (IRSA, Pod Identity). The chart adds
  `workspace.serviceAccount.annotations`.

### Bug Fixes

- The control plane dials the workspace service on its own URL (#29).
- 27 open bugs fixed in one sweep (#32).

## [0.1.1] - 2026-10-17

### Breaking changes

- **GitHub App installs:** this login doesn't use the GitHub App's callback URL. Clear that
  URL, and untick "Request user authorization (OAuth) during installation". Otherwise anyone
  who installs the App gets sent to `/v1/auth/callback` with a GitHub code and sees a 401.

### Features

- **Identity links** (ADR-0069, #10). One Principal can hold several provider identities, each
  tied to the provider's immutable account id. Accounts can be linked, merged after proving
  control of both, and unlinked.

### Bug Fixes

- **Login through Authentik and other strict OIDC providers** (#13). The OAuth token request
  now includes the same `redirect_uri` sent on the authorize redirect, as RFC 6749 §4.1.3
  requires. Without it, Authentik rejected the client (`invalid_client`) and every login ended
  in `401 authentication required` at `/v1/auth/callback`. GitHub doesn't enforce the rule,
  which is why GitHub login worked. `POST /v1/auth/login` also accepts an optional
  `redirect_uri` for CLIs that run their own authorize.
- **Login failures are logged** (#13). Scarab used to discard the reason for a failed
  callback. It now logs the provider's error (e.g. `invalid_client: …`) and which callback
  step failed: missing state cookie, state mismatch, code exchange, or session store.
- **Helm: `existingSecret` no longer drops the results sidecar** (#11). The results-sidecar
  image and URL now stay set when the chart is deployed with `existingSecret`.

### Dependency

- CI uses the digest-pinned `pgsty/minio` and `pgsty/mc` images, replacing the archived MinIO
  images (#12).

## [0.1.0] - 2026-10-16

The first release: the `scarab-server`, `scarab-clone`, `scarab-wsfetch` and
`scarab-results-sidecar` images and the Helm chart at
`oci://ghcr.io/thulasi-ram/charts/scarab`. It had no GitHub Release notes.

[Unreleased]: https://github.com/thulasi-ram/scarab-ci/compare/v0.2.2...HEAD
[0.2.2]: https://github.com/thulasi-ram/scarab-ci/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/thulasi-ram/scarab-ci/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/thulasi-ram/scarab-ci/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/thulasi-ram/scarab-ci/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/thulasi-ram/scarab-ci/releases/tag/v0.1.0
