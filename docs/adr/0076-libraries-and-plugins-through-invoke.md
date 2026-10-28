# 0076. Libraries and plugins through `invoke`: installed YAML libraries, plugins as Principals that resolve gates

- **Status:** Proposed
- **Date:** 2026-10-28
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0038](0038-invoke-and-local-reuse.md) (`invoke` gains installed libraries
  and plugin gates; library inputs bind at compile time; vendoring gets tooling),
  [0045](0045-source-provisioning.md) (the `clone` Step kind becomes a first-party
  library), [0072](0072-plugins-a-governed-extension-point.md) (plugins act as Principals;
  `offer_actions`, `subscribe_events` and `plugin_action` are dropped),
  [0074](0074-deployments-leave-core.md) §7 (drops `subscribe_events`),
  [0075](0075-policy-engine.md) (adds `forge_token` and `origin` to `step.admit`'s input,
  and PolicyInvariants for credentials)
- **Builds on:** [0043](0043-launch-parameters-and-manual-dispatch.md) (one `interface`
  shape), [0069](0069-identity-links.md) (Principals)
- **Relates:** [0008](0008-step-contract.md) (a Step is any OCI image),
  [0073](0073-step-up-confirmation.md) (step-up), [0049](0049-identity-and-access.md)
  (RBAC), [0021](0021-double-effect-fencing.md) (fences)

## Context

On 2026-10-28 the owner set four directions while closing ADR-0074:

- **Two kinds of reuse.** Work inside a Run is a Step, reused through `invoke`. Work
  outside a Run belongs to a plugin. The polling `plugin_action` Step was built in part
  and dropped, because it added dispatch, lease and timeout machinery to core.
- **`invoke` is the one entry point.** It can name a local YAML file, a library, or a
  plugin.
- **A library is a file, not an image.** In Scarab an OCI reference means "this runs": a
  Step's `image:` and a plugin's image. Libraries stay `.yaml`.
- **First-party code is not bundled into the server.** A zero-day in a first-party
  library, or in the image it pins, must be fixable, or removable, without a server
  upgrade.

What exists today:

- `invoke` takes a repo-relative path (`resolve_lib_path`,
  `crates/scarab-pipeline/src/lib.rs:1408-1444`) and inlines that Pipeline at compile
  time. `with:` is validated against the library's `interface.inputs` and delivered only
  as `SCARAB_PARAM_*` environment variables; after inlining, `${{ inputs.* }}` means the
  calling Run's launch parameters, not the library's inputs (`lib.rs:1695-1745`). ADR-0038
  keeps `invoke` local and rejects remote `invoke`; third-party subgraphs are vendored
  into `.scarab/lib/`. There is no vendoring tool. `scarab validate` compiles with no
  libraries (`crates/scarab-cli/src/main.rs:209`).
- `clone` is a built-in Step kind with special handling: the operator can override its
  image (`SCARAB_CLONE_IMAGE`, `crates/scarab-executor-k8s/src/lib.rs:3615`), the
  credential is minted at each launch, including a re-drive, and its username comes from
  the forge (`executor-k8s/src/lib.rs:2633`, `4113-4117`), secrets and workspace inputs are
  forced empty, and a fork pull request's credential is fixed read-only at Run creation
  (`crates/scarab-server/src/lib.rs:5522-5537`).
- **Checkout credentials differ by forge.** GitHub App mode mints a short-lived token
  scoped to the repository with `contents: read` only (`crates/scarab-forge-github/src/lib.rs:1063-1066`).
  GitHub PAT mode returns the PAT (`lib.rs:1022-1036`), and Forgejo returns the
  connection's long-lived token, where `read_only` is only a flag
  (`crates/scarab-forge-forgejo/src/lib.rs:752-769`). Today these reach only the clone
  container, whose image and command the server sets.
- A plugin authenticates to core with a projected ServiceAccount token checked by
  TokenReview, rotated by the kubelet, with the kill switch checked on every request
  (`crates/scarab-plugin/src/lib.rs:87-98`, `668-690`). An external gate is released only
  with an HMAC token (`release_gate_external`, `crates/scarab-server/src/lib.rs:8059`).

## Decision

### 1. `invoke` names one of four targets, by syntax

| `invoke:` | Target | Inlines |
|---|---|---|
| ends in `.yaml` or `.yml` | a library in the repository, written there or vendored (§3) | its Steps |
| `<org>/<name>` | a library installed in the caller's own org (§4) | its Steps |
| `scarab/<name>` | a first-party library installed in the org (§5) | its Steps |
| `plugin:<installation>/<gate>` | a gate an installed plugin resolves (§7) | one gate node |

The form decides the kind; a target is a repository path only if it ends in `.yaml` or
`.yml`. `<org>` must be the caller's own Scarab org. `scarab` is reserved in Scarab's org
namespace, so no org can be named `scarab`. Forge org names play no part.

`invoke` stays compile-time inlining (ADR-0038). Nothing is fetched while a Pipeline
compiles: a name resolves to content already stored in the repository or in Scarab's
database. Remote `invoke` (a URL in the Pipeline) stays rejected.

**Inputs bind at compile time.** Inlining substitutes the library's `${{ inputs.* }}` in
each inlined Step's image, command and environment with the caller's `with:` values,
type-checked against the library's `interface`. A reference to an input the library does
not declare is a compile error. Inside a library, `${{ inputs.* }}` never means the
calling Run's launch parameters. `SCARAB_PARAM_*` variables are still set.

**Installed libraries are closed.** An installed or first-party library may `invoke`
only installed or first-party libraries and plugin gates, never a repository path, so a
repository cannot replace Steps inside an approved library.

**`origin`.** The compiler records on each inlined Step the whole invoke chain that
produced it: each target's kind, name and content hash. An authored `origin:` is a
compile error. `origin` is in `step.admit`'s input (ADR-0075), so a Policy or a yank can
act on a Step through any level of nesting. Gates never pass `step.admit`, so `origin` is
checked at `gate.resolve` for gates.

**Policy.** Every resolution passes `invoke.resolve` (ADR-0075), where an org allows or
blocks a library or plugin, per repository if it wants. A resolution during a dispatch
catalog compile (`lib.rs:2134`), which has no Run, records its PolicyDecision in the
org's decision log.

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
- The `interface` shape is the one Pipelines already use (ADR-0043).
- A library pins the images it runs by digest. The Pipeline check warns on a library
  image pinned only by tag.
- A library's outputs are read as `${{ outputs.<invoke-id>.<name> }}`, as today
  (`lib.rs:1785-1796`).

### 3. Vendoring gets a tool and a lock file

```
scarab lib vendor github.com/acme/scarab-libs//deploy-helm.yaml@<commit>
```

copies the file into `.scarab/lib/deploy-helm.yaml` and adds an entry to
`.scarab/lib/lock.yaml`: source repository, path, commit and **content hash** (SHA-256 of
the file's bytes). `scarab lib update` fetches a newer commit and shows the diff; the
change lands as a pull request. Both `scarab validate` and the server's Pipeline check
fail when a vendored file no longer matches its lock entry; `scarab lib unlock` records a
deliberate local edit. Vendoring stays the answer for a repository that wants a fork or
full control.

### 4. Org-installed libraries

An org admin installs a library from a git source pinned to a commit:

- Scarab fetches the file at install time, anonymously over HTTPS for a public
  repository, or through a ForgeConnection the admin picks for a private one. It checks
  that the file compiles as a closed library (§1) and stores it, keyed by content hash,
  as a **LibraryVersion**. A **LibraryInstallation** names the version the org uses:
  `acme/deploy-helm → <hash>`.
- Pipelines name it: `invoke: acme/deploy-helm`. Compilation resolves the name to the
  installed hash, and the Run records it in `origin`. A Rerun does not recompile: the
  inlined Steps are stored in the Run's IR.
- Upgrading installs a newer commit, and Scarab shows the diff between the two versions
  before the admin confirms. Installing, upgrading and removing are `settings.change`
  decisions (ADR-0075), with its step-up rules.
- `scarab validate` resolves installed names through the server API when given one
  (`--server`); offline it reports them as unresolved and checks the rest.

### 5. First-party libraries: offered by default, not bundled

First-party libraries live as YAML in the Scarab repository under `libs/`, for example
`libs/clone.yaml`, and each release publishes them, with their hashes, as a release asset.

- A new org gets them installed from the release's source. If the source cannot be
  reached, the org is still created, and the admin sees which libraries are missing and
  can retry, or point the source at a mirror or an uploaded copy of the release asset
  (air-gapped installs).
- From then on they are ordinary LibraryInstallations. An org admin can upgrade one, keep
  an older version, or remove it, independently of the server version. An existing org
  is offered new first-party libraries; nothing is installed into it without an admin.
- Scarab shows when a newer version of an installed `scarab/…` library exists, and marks
  it when the newer version is a security fix.
- Removing one is the admin's call. Pipelines that `invoke` it then fail to compile with
  "`scarab/clone` is not installed in this org", never in the middle of a Run.

### 6. Yanking

A **yanked** LibraryVersion refuses:

- new compilations that resolve to it, at `invoke.resolve`;
- Steps whose `origin` includes that version, at `step.admit`, and gates whose `origin`
  includes it, at `gate.resolve`, including Reruns and Retries of Runs that already used
  it.

The refusal names the version and the reason the admin gave. Yanking is a PolicyInvariant
(ADR-0075 §7): no Policy can let a yanked version run. Yanking tightens and needs no
step-up; un-yanking loosens and does.

Most first-party security fixes are to an **image** a library pins (such as
`scarab-clone`), not to the YAML. The fix is a new LibraryVersion that pins the patched
digest, plus a yank of the old one.

### 7. Plugins act as Principals and resolve gates they are named on

A plugin acts through the same public API people use, under its **PluginPrincipal**
`plugin:<installation-name>`, unique within its org (installation names are unique per
org, migration `0055`). Core never calls a plugin.

**Credential.** The plugin calls the API with its projected ServiceAccount token
(audience `scarab-api`), the credential it already uses for the broker: rotated by the
kubelet, verified by TokenReview, and refused once the installation is killed. It holds
no issued API token. Its RBAC role, **PluginGateResolver**, allows exactly two things in
its own org: listing the gates that name it as releaser, and resolving them.

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

Gate outputs are typed; `Interface.outputs` gains types to match.

**A Pipeline uses one through `invoke`:**

```yaml
- id: live
  needs: [bump]
  invoke: plugin:argocd-prod/wait-live
  with:
    app: payments-api
    image: ${{ outputs.bump.image }}
  gate_expires_after: 1800
```

This inlines one gate node, `gate: plugin`, whose releaser is `plugin:argocd-prod`,
stored in its own column. `with:` is checked against the gate definition's `interface`
at compile time. Because inputs can read upstream outputs, they are **interpolated when
the gate is reached** and stored on that wait's Attempt; a reference that does not
resolve fails the gate with the reason. `gate_expires_after` on the `invoke` Step carries
to the gate node. The gate uses the existing machinery: its own clock, expiry, holding
only its dependents, failing with a reason (ADR-0070 §9, kept by ADR-0074).

**The plugin finds its work by asking:**
`GET /v1/orgs/{org}/gates?pending=true&releaser=me` lists the open waits naming the
calling plugin, with their interpolated inputs, paged by a cursor over wait ids. A
plugin asking about another org gets 404. The query reads current state, so a restarted
plugin catches up with nothing to replay. The same query, for a person, serves the
"waiting on you" inbox.

**The plugin resolves the gate:** `POST /v1/runs/{id}/gates/{step}/release` with
`{"passed": true, "outputs": {...}}` or `{"passed": false, "reason": "..."}`, fenced on
the wait's Attempt (ADR-0021), so an answer to a superseded or retried wait is dropped.
It may also post `{"pending": "sync retry 2/5"}`, which updates the gate's label and
records that the plugin is working on it. The release is journalled with
`by: plugin:argocd-prod`. Outputs obey the size limits of a Step's results and are read
downstream as `${{ outputs.live.revision }}`.

**Two keys, as ADR-0072 §3 requires:**

1. An org admin installed the plugin with the `resolve_gates` capability, which lists its
   gate definitions.
2. The `invoke.resolve` Policy allows that installation's gates for the Run's Environment,
   or for the org when the Run has none. A default Policy denies `plugin:` targets until
   an Environment admin or org admin allows them; the typed setting "plugins whose gates
   may run here" compiles into that Policy (ADR-0075 §12).

**When the plugin does not answer.** Silence is counted per gate, from when the wait
opened or from the plugin's last `pending` post for that gate. The gate shows it. The
expiry bounds the wait. An Environment, or the org for Runs with no Environment, can set
a **PluginGateFallback**: after a set silence, the gate also accepts release by the
approvers the setting names, through `gate.resolve` with its Policies, journalled as
"released by `<user>`, fallback: `plugin:argocd-prod` silent since T". A gate whose
definition declares outputs accepts a fallback release only with those outputs supplied
by the person releasing it. A killed or uninstalled plugin opens the fallback at once,
or fails the gate when no fallback is set.

**Where the plugin's knowledge lives.** For Argo CD, `scarab-plugin-argocd` keeps the code
that reads an Application and judges its state (`assess.rs`, `version.rs`). Its
`wait-live` decides on the target's images, Synced and Healthy, and on whether the sync
of the current target failed: the same checks as the documented wait Step
(ADR-0074 §5). It reads Argo through the CredentialBroker; the Pipeline never holds the
Argo token.

**Gate plumbing this needs:** `plugin` in `GATE_KINDS`; the release endpoint accepting a
Principal and gates of kind `plugin` (today it accepts only `external`, and returns 404
when no gate token secret is set); `plugin` in the reject allow-list; a `by` on gate
release; an outputs write path for gates; typed `Interface.outputs`.

### 8. What the plugin system keeps and drops

- **Kept:** the plugin runtime and isolation, the CredentialBroker with read rules,
  PluginConnections, install with digest pinning, the kill switch, plugin-owned storage
  (ADR-0074 §7), PluginPrincipal.
- **New capability:** `resolve_gates`. It replaces `approve_gates`; a plugin listed among
  a manual gate's approvers uses the same release path.
- **Dropped:** `offer_actions`, `plugin_action` and `subscribe_events`. A plugin that wants
  to react to Runs reads the API like any client. An event feed can come later for every
  API client.
- **Later:** `contribute_ui`, and write actions through the broker (sync, promote), which
  need a Policy-based answer to "which Project may act on which vendor object" first.

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
      - [SCARAB_CLONE_URL, "${{ event.repo.clone_url }}"]
      - [SCARAB_CLONE_SHA, "${{ event.sha }}"]
      - [SCARAB_CLONE_DEPTH, "${{ inputs.depth }}"]
      - [SCARAB_CLONE_SUBMODULES, "${{ inputs.submodules }}"]
      - [SCARAB_CLONE_LFS, "${{ inputs.lfs }}"]
```

```yaml
- id: checkout
  invoke: scarab/clone
  with: { depth: full, lfs: true }
```

Core keeps two primitives.

**`forge_token: read | write`** on a Step. Core mints the credential at each launch of an
Attempt, including a re-drive, and mounts it as two files, `/scarab/secrets/forge-token`
and `/scarab/secrets/forge-username` (Forgejo's username is `scarab`; GitHub's is
`x-access-token`). It is never an environment variable. The clone script reads both. Two
PolicyInvariants (ADR-0075 §7) bound it:

- **Which Steps may receive it.** A Step receives a forge credential only when the forge
  mints a short-lived token scoped to the Run's repository (GitHub App mode), or when the
  Step's `origin` is an installed library the org has marked as trusted for long-lived
  credentials (`scarab/clone` by default). Otherwise `run.create` refuses the Run. On
  Forgejo and GitHub PAT connections, which hand out a long-lived token, only the trusted
  library's Step receives it, and its image and command come from the library, not from
  the repository. This keeps today's guarantee that a repository's own code never reads
  that token.
- **Fork pull requests always get `read`.** The decision is fixed at Run creation, as
  today.

`write` needs the forge to mint a write token (GitHub App mode gains a `contents: write`
mint), and a default Policy denies `forge_token: write` except on protected refs or in a
Run governed by an Environment. Policies may tighten that further. A token from a mint
is revoked when the Step's Attempt ends. `forge_token` is part of `step.admit`'s input.

**`${{ event.* }}` in interpolation.** Interpolation reads `inputs`, `outputs` and
`matrix` today; the trigger's `event` is visible only to `when:`. It becomes readable in
`${{ }}` too, with one added field, `event.repo.clone_url`, which Run creation computes
from the connection registry and stores with the Run (today it is computed at launch,
`crates/scarab-server/src/clone_executor.rs:21-36`). A Run that invokes `scarab/clone`
on a trigger with no SHA is refused at Run creation, as `clone:` is today
(`lib.rs:5503`).

**What else the `clone` kind does, and where it goes:**

| `clone` kind today | With `scarab/clone` |
|---|---|
| operator image override `SCARAB_CLONE_IMAGE`, air-gapped mirror | the org installs a LibraryVersion that pins the mirror's image |
| credential minted per launch, refreshed on re-drive | `forge_token` mints per Attempt launch |
| username from the forge | the `forge-username` file |
| secrets and workspace inputs forced empty | the library declares none, and a repository cannot change an installed library |
| missing-clone lint (`is_clone`) | checks for a Step whose `origin` is `scarab/clone` |
| local executor refuses clone | it refuses `forge_token` |
| Step id `checkout` | `checkout/git`; `needs: [checkout]` still works across the seam |

`clone:` stays the built-in kind during a deprecation window, unchanged. It is not
rewritten to `invoke: scarab/clone`, so an org that has not installed the library is not
broken. When the window ends, the kind is removed.

## Consequences

- One way to reuse: `invoke`. Authors learn one form, and the four targets differ in
  where the content comes from.
- Libraries and their pinned images can be fixed, yanked or removed without a server
  release.
- An org has one approval point for shared libraries, and per-repository allow and block
  through Policies.
- Plugins get a job that needs no core dispatch: resolving gates they are named on. A
  plugin uses the public API, with its projected token and a narrow role.
- Library inputs now bind to `with:`. A library that relied on `${{ inputs.* }}` meaning
  the Run's launch parameters changes meaning; the compile error for undeclared inputs
  catches it.
- A forge token can reach Steps beyond the clone Pod only where the forge mints
  short-lived, repository-scoped tokens. Forgejo and PAT connections keep today's
  limit.
- Removing the `clone` kind is a breaking change to every Pipeline, after a deprecation
  window.
- Scarab stores library content, versions and yanks, which is more to back up and
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
- **An issued API token for each plugin.** Rejected: issued tokens belong to a person,
  expire and need manual rotation, and are not refused by the kill switch. The projected
  token has none of those problems.
- **An event cursor for plugins.** Rejected as the primary mechanism: `events.seq` is
  allocated before commit, so a strict cursor skips late commits. The pending-gates query
  reads current state.
- **`forge_token` for any Step on any forge.** Rejected: on Forgejo and PAT connections it
  would hand a long-lived bot credential to repository code.
- **Keeping `clone` as a Step kind.** Rejected: the kind exists to mint one credential and
  fill a few variables; `forge_token` and `event.*` do that for every Step that is allowed
  one.

## Implementation horizon

1. **Library format and binding:** `kind: library`, target syntax, compile-time binding of
   `inputs.*`, closed installed libraries, `origin` with the invoke chain.
2. **Vendoring:** the lock file, `scarab lib vendor`, `update`, `unlock`, lock checks in
   `scarab validate` and the server.
3. **Org-installed and first-party libraries:** LibraryVersion and LibraryInstallation,
   install from git, diff on upgrade, yank, `libs/` and its release asset, default
   installation for new orgs with retry and mirror, update notices; `invoke.resolve` and
   `step.admit` with `origin` (needs ADR-0075's engine).
4. **Clone as a library:** `forge_token` with its two invariants and the default write
   Policy, the GitHub App write mint and revocation, `event.*` in interpolation with
   `event.repo.clone_url`, `scarab/clone`; then the deprecation window and removal of the
   kind.
5. **Plugin gates:** `gates:` in the manifest, typed outputs, `invoke: plugin:…`, `gate:
   plugin` with its releaser column and interpolation at the wait, the pending-gates
   query, PluginGateResolver with the projected token, release with outputs, `pending`
   posts, PluginGateFallback, and Argo's `wait-live`.

Open:

- How long the `clone:` deprecation window is.
- Whether a LibraryInstallation can be scoped to some Projects, or Policies at
  `invoke.resolve` are enough for that.
