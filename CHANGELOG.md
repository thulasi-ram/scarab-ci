# Changelog

What changed in each Scarab release. The layout follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

Each release has up to four parts: **Upgrading** (what to do before or after you
upgrade), **New**, **Fixes**, and **Development** (changes for people working on
Scarab itself). A pull request with a change users can see adds a line under
`[Unreleased]`. Every entry under New links the docs page that explains it;
`just release <version>` fails on one that doesn't. When a `v*` tag is pushed,
release.yml uses that version's section as the GitHub Release text. See
[docs/RELEASING.md](docs/RELEASING.md).

## [Unreleased]

Adds libraries and plugin gates through `invoke`, a policy engine for Run creation, and
checked forge credentials for Steps. Removes the `clone:` Step kind and the deployment
record. Read Upgrading before you upgrade.

### Upgrading

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
  Cache now has a home replica.

### New

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
  split roles. A replica renews its claim on a Step's launch while it works on it, so two
  replicas no longer drive one Pod's workspace drain at once (git-bug 068cc0b); when a
  replica dies, a peer takes over its Steps within 30 s. The scheduler lease has a term that
  every takeover increments; each leader-only write checks it in its own transaction, so a
  leader paused past its 30 s lease writes nothing once a peer has taken over (git-bug
  aaf4e3e). Tested against Postgres (`replica_claims.rs`, `leader_term.rs`); the fault tier
  adds scenarios that kill a server replica and pause the leader on a local kind cluster.
  [ADR-0077 amendment](https://thulasi-ram.github.io/scarab-ci/tech/adr/0077-end-to-end-fault-and-agent-driven-testing/#amendment-2026-11-01-grill)
- **Caches at more than one Depot replica.** Each Cache key has one home Depot replica, chosen
  by rendezvous hashing over the StatefulSet's ordinals, and a Step saves and restores the
  Cache there. Before, a Cache's saves and restores scattered over the replicas, so a restore
  almost always missed (git-bug 12e2f6b). A home that cannot be reached is a miss, never a
  failed Step. Changing the replica count moves about 1/N of the keys, and each moved key
  misses once. `workspace.homeUrlTemplate` overrides the per-replica address.
  [ADR-0077 amendment](https://thulasi-ram.github.io/scarab-ci/tech/adr/0077-end-to-end-fault-and-agent-driven-testing/#amendment-2026-11-01-grill)

### Fixes

- A Run was visible to the scheduler before its parameters, `when` exclusions and concurrency
  group were written; a Step could start without them (#51). A Run left half-created by a
  server that stopped is failed after 10 minutes.
- A dispatch parameter holding `${{ … }}` was evaluated as an expression (#49).
- A deep or long CEL expression in a pushed Pipeline could overflow the stack and stop the
  server (#44).
- The clone image's askpass gave the token to an LFS server named in `.lfsconfig`, to other
  repositories on the same host through submodules, and over `http://` (#53).
- A value from outside the Pipeline holding `$(SECRET_NAME)` was filled with that secret by
  Kubernetes (#52). Plugin Deployments and probe Pods are escaped the same way.
- Environment re-admission runs before any checkout token or registry credential is minted; a
  timer gate fails when the Environment no longer admits the Run.
- A NUL byte in a dispatch reason, commit subject, PR title or Step result no longer causes a
  500.
- At more than one Depot replica, a Step whose token named a Cache another replica held got a
  500 on its required inputs. A cache root a replica cannot walk is now left out of the grant
  (git-bug 2a79b73).

### Development

- The local stack is one kind cluster, `scarab-dev`, with one Helm release in four modes
  (dev, dogfood, e2e, chaos), each with its own database and bucket; Forgejo runs in it.
  `deploy/local-helm`, `deploy/local-proc` and the colima k3s setup are gone. The chart gains
  `command`, `args`, `extraVolumes` and `extraVolumeMounts` for the server and the workspace
  container, and `service.nodePort`, which the kind stack uses.
- The Justfile is split into modules: `just stack …`, `just test …` and `just check …`.
  `just e2e` is `just test e2e`, `just chaos` is `just test chaos`, and `just up` is
  `just stack up`; `just forgejo-verify`, `just kube-tests` and `just plugin-kube-tests` are
  folded into `just test e2e`. `just release <version>` checks a release before it is tagged.
- The fault tier (`just test chaos`, ADR-0077) runs ranks 1, 2 and 4: a Depot Pod deleted
  mid-drain and mid-feed, two server replicas sharing the work, a server replica killed while
  it drains, and the scheduler leader paused past its lease.
- The ADR-0062 overlay change-set code, its tests and its recipes are deleted; ADR-0066 had
  already dropped the feature.

## [0.2.2] - 2026-10-28

### Upgrading

- **From 0.2.1 or earlier, recreate the workspace StatefulSet once.** Delete it without
  deleting its Pod or PVC, then upgrade:

  ```sh
  kubectl -n <ns> delete sts --cascade=orphan \
    -l app.kubernetes.io/name=scarab-workspace,app.kubernetes.io/instance=<release>
  helm upgrade <release> oci://ghcr.io/thulasi-ram/charts/scarab --version 0.2.2 ...
  ```

  The new StatefulSet re-adopts the existing PVC by name (`cas-<sts>-0`); no data is lost.
  Expect one workspace Pod restart. For Argo CD: run the delete, then sync.

### Fixes

- **Chart upgrades were rejected by the API server.** The workspace StatefulSet's
  `volumeClaimTemplates` carried `helm.sh/chart` and `app.kubernetes.io/version` labels. That
  field is immutable, so every chart upgrade since v0.1.0 failed with
  `StatefulSet.apps "...-workspace" is invalid: spec: Forbidden: updates to statefulset spec for fields other than ...`
  (and Argo CD syncs failed). The template now carries only version-free labels (#38).

### Development

- A CI guard (`scripts/chart_statefulset_immutable.sh`) renders the chart at two versions
  under default and feature-on values and fails if any immutable StatefulSet field differs.

## [0.2.1] - 2026-10-27

### New

- **Versioned docs** (#36). Each release line is published at
  `https://thulasi-ram.github.io/scarab-ci/vX.Y/`, the latest stays at the root, and a version
  dropdown sits in the header. This is the first versioned publish.

No server, chart or API behaviour changes.

## [0.2.0] - 2026-10-27

### New

- **Deployments**: vendor-neutral Deployment requests with deployment gates and concurrency
  holds (#32).
- **Step-up confirmation** (ADR-0073): passkeys, the step-up guard, requests and a security
  log (#32).
- **S3 ambient credentials** (#34). Leave `SCARAB_S3_ACCESS_KEY` and `SCARAB_S3_SECRET_KEY`
  unset to use AWS's credential chain (IRSA, Pod Identity). The chart adds
  `workspace.serviceAccount.annotations`.

### Fixes

- The control plane dials the workspace service on its own URL (#29).
- 27 open bugs fixed in one sweep (#32).

## [0.1.1] - 2026-10-17

### Upgrading

- **GitHub App installs:** this login doesn't use the GitHub App's callback URL. Clear that
  URL, and untick "Request user authorization (OAuth) during installation". Otherwise anyone
  who installs the App gets sent to `/v1/auth/callback` with a GitHub code and sees a 401.

### New

- **Identity links** (ADR-0069, #10). One Principal can hold several provider identities, each
  tied to the provider's immutable account id. Accounts can be linked, merged after proving
  control of both, and unlinked.

### Fixes

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

### Development

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
