# 0076. Libraries and plugins through `invoke`: installed YAML libraries, plugins as Principals that resolve gates

- **Status:** Proposed
- **Date:** 2026-10-28
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0038](0038-invoke-and-local-reuse.md) (`invoke` gains installed libraries
  and plugin gates; vendoring gets tooling), [0045](0045-source-provisioning.md) (the
  `clone` Step kind becomes a first-party library), [0072](0072-plugins-a-governed-extension-point.md)
  (plugins act as Principals; `offer_actions`, `subscribe_events` and `plugin_action`
  are dropped)
- **Builds on:** [0074](0074-deployments-leave-core.md) (plugins kept, no CD in core,
  plugin-owned storage), [0075](0075-policy-engine.md) (allow and block rules are
  Policies at `invoke.resolve` and `step.admit`), [0043](0043-launch-parameters-and-manual-dispatch.md)
  (one `interface` shape for inputs), [0069](0069-identity-links.md) (Principals)
- **Relates:** [0008](0008-step-contract.md) (a Step is any OCI image),
  [0073](0073-step-up-confirmation.md) (step-up), [0049](0049-identity-and-access.md)
  (issued API tokens)

## Context

On 2026-10-28 the owner set four directions while closing ADR-0074:

- **Two kinds of reuse.** Work inside a Run is a Step, reused through `invoke`. Work
  outside a Run belongs to a plugin. The polling `plugin_action` Step was built in part
  and dropped, because it added dispatch, lease and timeout machinery to core.
- **`invoke` is the one entry point.** It can name a local YAML file, a library, or a
  plugin.
- **A library is a file, not an image.** In Scarab an OCI reference means "this runs": a
  Step's `image:` and a plugin's image. Shipping YAML as OCI artifacts would make an OCI
  reference mean two things. Libraries stay `.yaml`.
- **First-party code is not bundled into the server.** A zero-day in a first-party
  library, or in the image it pins, must be fixable, or removable, without a server
  upgrade.

What exists today:

- `invoke` takes only a repo-relative path and inlines that Pipeline at compile time,
  validating `with:` against the library's `interface.inputs` (ADR-0038). ADR-0038 keeps
  `invoke` local and rejects remote `invoke`: third-party subgraphs are **vendored** into
  `.scarab/lib/`, so every change is a reviewed diff. There is no vendoring tool.
- `clone` is a built-in Step kind. Core fills the repo, the pinned SHA and the
  depth/submodules/LFS settings, mints a checkout credential at launch (read-only for a
  fork pull request, `crates/scarab-server/src/clone_executor.rs:174-180`), and runs the
  `scarab-clone` image, whose whole contract is environment variables
  (`docker/clone/scarab-clone.sh`).
- A plugin is a long-lived Pod outside any Run. After ADR-0074 no built capability lets
  it act. ADR-0072 §3 already defines a PluginPrincipal and a two-key rule for a plugin
  that approves gates. An external gate is released only with an HMAC token
  (`release_gate_external`, `crates/scarab-server/src/lib.rs:8059`), never by a
  Principal.

## Decision

### 1. `invoke` names one of four targets

| `invoke:` | Target | Inlines |
|---|---|---|
| `.scarab/lib/x.yaml` | a library in the repository, written there or vendored (§3) | its Steps |
| `acme/x` | a library installed in the org (§4) | its Steps |
| `scarab/x` | a first-party library, installed in the org by default (§5) | its Steps |
| `plugin:argocd/wait-live` | a gate a plugin resolves (§7) | one gate node |

`invoke` stays compile-time inlining (ADR-0038). Nothing is fetched while a Pipeline
compiles: a name resolves to content already stored in the repository or in Scarab's
database. Remote `invoke` (a URL in the Pipeline) stays rejected.

Every resolution passes the `invoke.resolve` PolicyCheckpoint (ADR-0075), which is where
an org allows or blocks a library or plugin, per repository if it wants. The resolved
target's kind, name and content hash are recorded on the Run and carried on each inlined
Step as its `origin`, so `step.admit` can still deny a Step whose library was blocked or
yanked after the Run started.

### 2. A library is a YAML file with `kind: library`

```yaml
# deploy-helm.yaml
kind: library
interface:
  inputs:
    - { name: release, type: string }
    - { name: chart,   type: string, required: false, default: ./chart }
steps:
  - id: upgrade
    image: alpine/helm:3.16.2@sha256:…
    command: [helm, upgrade, --install, --wait, "${{ inputs.release }}", "${{ inputs.chart }}"]
```

- `kind: library` marks the file. A library has no `on:` triggers, and a file marked as a
  library cannot be used as a triggered Pipeline.
- The `interface` shape is the one Pipelines already use (ADR-0043). `with:` is checked
  against it at compile time, as today.
- A library pins the images it runs by digest. The Pipeline check warns on a library
  image pinned only by tag.
- Library outputs keep today's namespacing: `${{ outputs.<invoke-id>/<step>.<name> }}`.

### 3. Vendoring gets a tool and a lock file

```
scarab lib vendor github.com/acme/scarab-libs//deploy-helm.yaml@<commit>
```

copies the file into `.scarab/lib/deploy-helm.yaml` and adds an entry to
`.scarab/lib/lock.yaml`: the source repository, path, commit and the file's content
hash. `scarab lib update` fetches a newer commit and shows the diff; the change lands as
a pull request like any other. The Pipeline check fails when a vendored file no longer
matches its lock entry, so a hand edit is either recorded (`scarab lib unlock`) or
reverted. Vendoring stays the answer for a repository that wants a fork or full control.

### 4. Org-installed libraries

An org admin installs a library from a git source pinned to a commit:

- Scarab fetches the file at install time, checks it (it compiles as a library), and
  stores it in the database keyed by content hash as a **LibraryVersion**. The
  installation names the version an org uses: a **LibraryInstallation** `acme/deploy-helm
  → <hash>`.
- Pipelines name it: `invoke: acme/deploy-helm`. Compilation resolves the name to the
  installed hash. The Run records the hash, and a rerun uses the same hash.
- Upgrading installs a newer commit. Scarab shows the diff between the two versions
  before the admin confirms. Installing, upgrading and removing are `settings.change`
  decisions (ADR-0075), and the step-up rules apply.
- Names are namespaced by org (`acme/…`). `scarab/…` is reserved (§5).
- An installation can be **yanked** (§6).

Projects that need a different version vendor it (§3).

### 5. First-party libraries are installed by default, not bundled

First-party libraries live as YAML in the Scarab repository under `libs/`, for example
`libs/clone.yaml`, and are published with each release tag.

- A new org gets them installed from the release's commit. From then on they are ordinary
  LibraryInstallations: an org admin can upgrade one, keep an older version, or remove it,
  independently of the server version.
- An air-gapped installation points the install source at a mirror of the repository.
- Scarab shows when a newer version of an installed `scarab/…` library exists, and marks
  it when the newer version is a security fix.
- Removing one is the admin's call. Pipelines that `invoke` it then fail to compile with
  "`scarab/clone` is not installed in this org", never in the middle of a Run.

### 6. Yanking

A **yanked** LibraryVersion refuses:

- new compilations that resolve to it, at `invoke.resolve`;
- Steps whose `origin` is that version, at `step.admit`, including Reruns and Retries of
  Runs that already used it.

The refusal names the version and the reason the admin gave. Yanking is a PolicyInvariant
(ADR-0075 §7): no Policy can let a yanked version run. Yanking tightens, so it needs no
step-up; un-yanking loosens and does.

Most first-party security fixes are to an **image** a library pins (such as
`scarab-clone`), not to the YAML. The fix is a new LibraryVersion that pins the patched
digest, plus a yank of the old one.

### 7. Plugins act as Principals and resolve gates they are named on

A plugin is a non-human member of the org. It acts through the same public API people
use, under its **PluginPrincipal** (`plugin:argocd`, ADR-0072 §3) and an issued API token
scoped to that Principal (ADR-0049). Core never calls a plugin.

**A plugin declares gate definitions** in its manifest:

```yaml
# PluginManifest (excerpt)
gates:
  wait-live:
    description: Pass when Argo CD reports the Application live at the given image.
    interface:
      inputs:
        - { name: app,   type: string }
        - { name: image, type: string }
      outputs:
        - { name: revision, type: string }
```

**A Pipeline uses one through `invoke`:**

```yaml
- id: live
  needs: [bump]
  invoke: plugin:argocd/wait-live
  with:
    app: payments-api
    image: ${{ outputs.bump.image }}
  gate_expires_after: 1800
```

This inlines **one gate node**, `gate: plugin`, whose releaser is `plugin:argocd`. `with:`
is checked against the gate definition's `interface` at compile time, as for a library,
and the resolved inputs are stored on the gate. The gate uses the existing machinery: its
own clock, expiry, holding only its dependents, failing with a reason (ADR-0070 §9, kept
by ADR-0074).

**The plugin finds its work by asking.** A new endpoint lists the gates waiting on the
calling Principal, with each gate's stored inputs:
`GET /v1/orgs/{org}/gates?pending=true&releaser=me`. It reads current state, so a
restarted plugin catches up with nothing to replay, and there is no event cursor to get
wrong. The same query serves a person's "waiting on you" inbox.

**The plugin resolves the gate** with the gate API, authenticated as its Principal:
`POST /v1/runs/{id}/gates/{step}/release` with `{"passed": true, "outputs": {...}}`, or
`{"passed": false, "reason": "..."}`. An external gate today accepts only the HMAC token;
it now also accepts a bearer Principal, when that Principal is the gate's named releaser.
The release is journalled as "released by plugin:argocd". Outputs obey the size limits of
a Step's results and are read downstream as `${{ outputs.live.revision }}`.

**Two keys, as ADR-0072 §3 requires:**

1. An org admin installed the plugin with the `resolve_gates` capability, which names its
   gate definitions.
2. The `invoke.resolve` Policy for the Environment (or the org, for Runs with no
   Environment) allows that plugin's gates. A default Policy denies `plugin:` targets
   until an Environment admin or org admin allows them; the typed setting "plugins whose
   gates may run here" compiles into that Policy (ADR-0075 §12).

**When the plugin is silent.** The gate shows when its releaser last called the API
(the token's `last_used_at`). The gate's expiry bounds the wait. An Environment can set a
fallback: after a set time with no answer from the releaser, the gate also accepts
release by the Environment's approvers. Without a fallback the gate fails at expiry with
"plugin:argocd did not answer".

**Where the plugin's knowledge lives.** For Argo CD, `scarab-plugin-argocd` keeps the code
that reads an Application and judges its state (`assess.rs`, `version.rs`). Its
`wait-live` decides on the target's images, Synced and Healthy, and on whether the sync
of the current target failed, the same checks the documented wait Step makes
(ADR-0074 §5). The plugin reads Argo through the CredentialBroker; the Pipeline never
holds the Argo token.

### 8. What the plugin system keeps and drops

- **Kept:** the plugin runtime and isolation, the CredentialBroker with read rules,
  PluginConnections, install with digest pinning, the kill switch, plugin-owned storage
  (ADR-0074 §7), PluginPrincipal.
- **New capability:** `resolve_gates` (replaces `approve_gates`: a plugin resolves the
  gates it is named on; approving human approval gates as an extra approver is the same
  mechanism, with the plugin listed among a gate's approvers).
- **Dropped:** `offer_actions`, `plugin_action` and `subscribe_events`. A plugin that wants
  to react to Runs polls the API, like any client. An event feed can come later for
  every API client, not as a plugin capability.
- **Later:** `contribute_ui`, and write actions through the broker (sync, promote), which
  need a Policy-based answer to "which Project may act on which vendor object" before
  they exist.

### 9. `clone` becomes a first-party library

The `clone` Step kind is replaced by `scarab/clone`:

```yaml
# libs/clone.yaml
kind: library
interface:
  inputs:
    - { name: depth,      type: string,  required: false, default: "1" }
    - { name: submodules, type: boolean, required: false, default: false }
    - { name: lfs,        type: boolean, required: false, default: false }
steps:
  - id: git
    image: ghcr.io/thulasi-ram/scarab-clone:<version>@sha256:…
    forge_token: read
    env:
      - [SCARAB_CLONE_URL, "${{ trigger.repo.clone_url }}"]
      - [SCARAB_CLONE_SHA, "${{ trigger.sha }}"]
      - [SCARAB_CLONE_DEPTH, "${{ inputs.depth }}"]
      - [SCARAB_CLONE_SUBMODULES, "${{ inputs.submodules }}"]
      - [SCARAB_CLONE_LFS, "${{ inputs.lfs }}"]
      - [SCARAB_CLONE_TOKEN_FILE, /scarab/secrets/forge-token]
```

```yaml
- id: checkout
  invoke: scarab/clone
  with: { depth: full, lfs: true }
```

Core keeps two primitives, usable by any Step:

- **`forge_token: read | write`.** Core mints a forge credential scoped to the Run's
  repository at launch, as `mint_checkout_credential` does for clone today, and mounts it
  as a file. A fork pull request always gets `read`, whatever the Step asks for, as a
  PolicyInvariant. `write` passes `step.admit`, so a Policy decides which Steps may push
  tags, comment or release. The token is never in the Step's environment variables.
- **`${{ trigger.* }}` in interpolation**: the repository's clone URL, the pinned SHA, the
  symbolic ref and the trigger kind. Interpolation reads `inputs`, `outputs` and `matrix`
  today; trigger data is visible only to `when:`. Reruns keep the same SHA.

`clone: {}` keeps compiling during a deprecation window, rewritten to
`invoke: scarab/clone` with the same settings, and is then removed.

## Consequences

- One way to reuse: `invoke`. Authors learn one form, and the four targets differ only in
  where the content comes from.
- Libraries and their pinned images can be fixed, yanked or removed without a server
  release.
- An org has one approval point for shared libraries, and per-repository allow and block
  through Policies, instead of per-repository vendoring for everything.
- Plugins get a job that needs no core dispatch: resolving gates they are named on. A
  plugin is an API client, so the plugin runtime adds isolation and credentials, not a
  second protocol.
- The forge token can reach any Step that asks for it, not only the clone Pod. Policy
  decides who may ask for `write`.
- Removing the `clone` kind is a breaking change to every Pipeline, handled by the
  compile-time rewrite during the deprecation window.
- Scarab now stores library content, versions and yanks, which is more to back up and
  migrate.

## Alternatives considered

- **Libraries as OCI artifacts.** Rejected by the owner: an OCI reference would mean "runs"
  for Steps and plugins, and "inline this YAML" for libraries.
- **Remote `invoke` by URL.** Still rejected (ADR-0038): code enters a Run with no
  approval and no review.
- **First-party libraries bundled in the server binary.** Rejected by the owner: a
  security fix would need a server upgrade.
- **A new file extension (`.scrl`).** Rejected: `.yaml` keeps editor support and schema
  validation; `kind: library` marks the file.
- **`plugin_action` Steps.** Dropped in ADR-0074: dispatch, leasing and timeouts in core.
- **An event cursor for plugins.** Rejected as the primary mechanism: `events.seq` is
  allocated before commit, so a strict cursor skips late commits. The pending-gates query
  reads current state.
- **Keeping `clone` as a Step kind.** Rejected: the kind exists to mint one credential and
  fill a few variables; two general primitives do that for every Step.

## Implementation horizon

1. **Library format and vendoring:** `kind: library`, the lock file, `scarab lib vendor`,
   `update`, `unlock`, the Pipeline check for lock mismatches.
2. **Org-installed libraries:** LibraryVersion and LibraryInstallation, install from git at
   a commit, diff on upgrade, yank, the `invoke.resolve` and `step.admit` hooks with
   `origin` (needs ADR-0075's engine).
3. **First-party libraries:** `libs/` in the repository, default installation for new
   orgs and on upgrade for existing ones, update notices.
4. **Clone as a library:** `forge_token`, `${{ trigger.* }}`, `scarab/clone`, the
   compile-time rewrite of `clone:`, then removal of the kind.
5. **Plugin gates:** `gates:` in the manifest, `invoke: plugin:…`, `gate: plugin` with a
   named releaser and stored inputs, the pending-gates query, Principal release with
   outputs, the releaser fallback, and Argo's `wait-live`.

Open:

- Whether an org can install a library from a repository the org's forge connection
  cannot read (a public repository) without a connection.
- How long the `clone:` deprecation window is.
