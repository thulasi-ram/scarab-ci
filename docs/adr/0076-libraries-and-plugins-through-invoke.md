# 0076. Libraries and plugins through `invoke`: installed YAML libraries, plugins as Principals that resolve gates

- **Status:** Proposed
- **Date:** 2026-10-28 (simplified 2026-10-29, see the end)
- **Amended:** 2026-10-30 (§1: a library invocation is a scope rendered once at launch, and
  inputs are no longer bound into the text at compile time; see "Amendment (2026-10-30)")
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0038](0038-invoke-and-local-reuse.md) (`invoke` gains a prefix per target
  kind, installed libraries and plugin gates; library inputs bind at compile time, read in a
  scope at launch since the 2026-10-30 amendment),
  [0041](0041-named-results-and-interpolation.md) (2026-10-30: an invoke's outputs are read
  through a view, no longer rewritten at compile time),
  [0045](0045-source-provisioning.md) (the `clone` Step kind becomes a first-party
  library), [0072](0072-plugins-a-governed-extension-point.md) (plugins act as Principals;
  `offer_actions`, `subscribe_events`, `approve_gates` and `plugin_action` are dropped),
  [0074](0074-deployments-leave-core.md) §7 (drops `subscribe_events`),
  [0075](0075-policy-engine.md) (adds `forge_token` and `origin` to the checkpoint inputs,
  and PolicyInvariants for credentials)
- **Builds on:** [0043](0043-launch-parameters-and-manual-dispatch.md) (one `interface`
  shape), [0069](0069-identity-links.md) (Principals)
- **Relates:** [0008](0008-step-contract.md) (a Step is any OCI image),
  [0073](0073-step-up-confirmation.md) (step-up), [0049](0049-identity-and-access.md)
  (RBAC), [0021](0021-double-effect-fencing.md) (fences), [0057](0057-run-provenance-trigger-context.md)
  (provenance kept out of interpolation)

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
  `crates/scarab-pipeline/src/lib.rs:1278-1316`) and inlines that Pipeline at compile
  time. `with:` is validated against the library's `interface.inputs` (`lib.rs:1695-1745`)
  and delivered only as `SCARAB_PARAM_*` environment variables; after inlining,
  `${{ inputs.* }}` means the calling Run's launch parameters, not the library's
  inputs. Inlining
  copies each library Step and changes only its id, `deploy`, `needs`, inputs, matrix
  values and `SCARAB_PARAM_*` variables (`lib.rs:1502-1570`); the invoke Step's own
  `when`, `timeout`, `resources` and `placement_profiles` are not carried over, and no
  diagnostic says so. ADR-0038 keeps `invoke` local and rejects remote `invoke`;
  third-party subgraphs are vendored into `.scarab/lib/`. There is no vendoring tool.
  `scarab validate` compiles with no libraries (`crates/scarab-cli/src/main.rs:226`).
- `clone` is a built-in Step kind with special handling: the operator can override its
  image (`SCARAB_CLONE_IMAGE`, `crates/scarab-executor-k8s/src/lib.rs:3615`), the
  credential is minted at each launch, including a re-drive, and its username comes from
  the forge (`executor-k8s/src/lib.rs:2634`, `4113-4117`), secrets and workspace inputs are
  forced empty, a trigger with no SHA is refused, and a fork pull request's credential is
  fixed read-only at Run creation (`crates/scarab-server/src/lib.rs:5191-5222`).
- **Checkout credentials differ by forge.** GitHub App mode mints a short-lived token
  scoped to the repository with `contents: read` only (`crates/scarab-forge-github/src/lib.rs:1063-1066`);
  the one write mint is `packages: write` for the registry (`lib.rs:1137`). GitHub PAT
  mode returns the PAT (`lib.rs:1022-1036`), and Forgejo returns the connection's
  long-lived token, where `read_only` is only a flag
  (`crates/scarab-forge-forgejo/src/lib.rs:750-769`). Today these reach only the clone
  container, whose image and command the server sets.
- A plugin authenticates to the broker with a projected ServiceAccount token (audience
  `scarab-broker`) checked by TokenReview, rotated by the kubelet, with the kill switch
  checked on every request (`crates/scarab-plugin/src/lib.rs:80-87`, `668-690`). It
  cannot call the public API. A PluginPrincipal is refused every operation that needs a
  step-up (`crates/scarab-server/src/step_up/mod.rs:877-884`). An external gate is released
  only with an HMAC token (`release_gate_external`, `crates/scarab-server/src/lib.rs:7733`).
- `Event::context()` leaves provenance fields out because spliced into a script they are
  an injection path (`crates/scarab-forge/src/lib.rs:415-424`), but it includes a
  comment's body for `when:` matching. Comment-triggered Runs read their Pipeline from the
  default branch (`config_ref`, `crates/scarab-server/src/lib.rs:4077`).

## Decision

### 1. `invoke` names one of three targets, by prefix

| `invoke:` | Target | Inlines |
|---|---|---|
| `file:<path>` | a library file in the repository, written there or vendored (§3) | its Steps |
| `lib:<publisher>/<name>` | a library installed in the caller's org (§4), first-party ones included (§5) | its Steps |
| `plugin:<installation>/<gate>` | a gate an installed plugin resolves (§7) | one gate node |

Every target has a prefix. A bare path is a compile error that names `file:`, and so is
an unknown prefix. `url:` is reserved and rejected: remote `invoke` stays rejected
(ADR-0038). A `file:` path keeps today's rules: repo-relative, no `..`, no `://` or `@`.
Only `file:` targets are read from the repository while compiling (`invoke_refs`,
`lib.rs:1231-1246`); a `lib:` name resolves to content already stored in Scarab's
database. Nothing is fetched while a Pipeline compiles.

**Inputs bind at compile time.** Inlining substitutes the library's `${{ inputs.* }}` in
each inlined Step's image, command and environment with the caller's `with:` values,
type-checked against the library's `interface`. A reference to an input the library does
not declare is a compile error. Inside a library, `${{ inputs.* }}` never means the
calling Run's launch parameters. `SCARAB_PARAM_*` variables are still set.

> **Amended 2026-10-30: superseded, see [Amendment (2026-10-30)](#amendment-2026-10-30-scoped-rendering).**
> Inputs are not substituted into the text at compile time. Each library invocation is a scope,
> and a library Step's `${{ inputs.* }}` reads that scope when the Step launches. The compile
> error for an undeclared input stays, and `inputs` inside a library still never means the
> calling Run's launch parameters.

**What the invoke Step carries.** The invoke Step's `timeout`, `resources` and
`placement_profiles` apply to each non-gate Step it inlines, only where the library Step
leaves that field unset; the library's own value wins. Its `when` is ANDed with the
`when` of every Step and gate it inlines. `env` never carries, and `placement_profiles` never carries onto a
Step with `forge_token` (§9): a placement profile is an arbitrary Pod patch, so a caller
could otherwise place a trusted Step next to its own. Any other Step field on an invoke
Step is a compile error, where today it is dropped without a word.

> **Amended 2026-10-30.** The `when` values are not joined as text. Each is kept as a separate
> guard and evaluated in the scope it was written in; the Step runs only if all of them hold, so
> the AND stands. See [Amendment (2026-10-30)](#amendment-2026-10-30-scoped-rendering).

**Installed libraries are closed.** An installed library may `invoke` only `lib:` and
`plugin:` targets, never `file:`, so a repository cannot replace Steps inside an approved
library.

**`origin`.** The compiler records on each inlined Step and gate the whole invoke chain
that produced it: each target's kind, name, content hash and, for a `lib:` target, its
source (§4). An authored `origin:` is a compile error. `origin` is in the `run.create`
input and in `step.admit`'s (ADR-0075 §2), so a Policy or a yank can act on a Step through
any level of nesting. Gates never pass `step.admit`, so a gate's `origin` is checked at
`run.create` and `gate.resolve`. A Policy that allows libraries matches `origin` by
source, since the name is chosen at install.

**Policy.** Library and plugin rules are `run.create` Policies over `origin`
(ADR-0075 §2). The typed settings "libraries allowed here" and "plugins whose gates may
run here" write them, at the org, Environment and Project levels; a list left unset adds no rule, and an
installed library is usable until a list narrows it (ADR-0075 §7). `scarab validate
--server` reports a refusal before the push.

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
- An input that reaches a Step with `forge_token` must be a `boolean`, a `number`, a
  `choice`, or a `string` with a `validate:` rule. A free string would let the caller
  write into that Step's environment or command.
- A library's outputs are read as `${{ outputs.<invoke-id>.<name> }}`, as today.

### 3. Vendoring is copying a file

A repository that wants a fork, or full control, copies the library into its tree and
names it with `file:`. `origin` records the file's content hash, so a Run shows what it
ran. There is no vendoring tool and no lock file; they can be added when someone needs
them.

### 4. Org-installed libraries

An org admin installs a library from a git source pinned to a commit:

- Scarab fetches the file at install time, anonymously over HTTPS for a public
  repository, or through a ForgeConnection the admin picks for a private one. It checks
  that the file compiles as a closed library (§1) and stores it, keyed by content hash,
  as a **LibraryVersion**. A **LibraryInstallation** names the version the org uses and
  where it came from: `acme/deploy-helm → <hash>`, source
  `github.com/acme/scarab-libs//deploy-helm.yaml`.
- **The name is `<publisher>/<name>`.** The publisher is the owner of the git source by
  default (`github.com/acme/…` gives `acme`), so an org can install another
  publisher's library under that publisher's name. The admin may change the publisher at
  install. Within an org, each publisher and name pair names one installation. No name
  is reserved. A name proves nothing about where a library came from: first-party
  status comes from the content hash (§5), and trust with forge credentials from the
  source (§9).
- An installation is usable by every Project in the org. A Policy narrows it to some
  Projects (§1).
- Pipelines name it: `invoke: lib:acme/deploy-helm`. Compilation resolves the name to the
  installed hash, and the Run records it in `origin`. A Rerun does not recompile: the
  inlined Steps are stored in the Run's IR.
- Upgrading installs a newer commit, and Scarab shows the diff between the two versions
  before the admin confirms. Installing and upgrading loosen and need a step-up; removing
  tightens. These are classifications in the step-up route table (ADR-0073), not Policy
  decisions (ADR-0075 §10).
- `scarab validate` resolves installed names through the server API when given one
  (`--server`); offline it reports them as unresolved and checks the rest.

### 5. First-party libraries: offered by default, not bundled

First-party libraries live as YAML in the Scarab repository under `libs/`, for example
`libs/clone.yaml`. Each release publishes them as a release asset with a manifest that
lists each library's name, publisher, source and content hash. A LibraryVersion whose
hash is in a Scarab release manifest is first-party, whether it was fetched from the
release, a mirror or an uploaded copy.

- The publisher is the owner of Scarab's repository, as for any other library:
  `lib:thulasi-ram/clone` today. If the repository moves, say to a `scarab-ci` org, new
  installs are named `lib:scarab-ci/clone`. An admin can install under the old name to
  keep existing Pipelines compiling.
- A new org gets them installed from the release's source. If the source cannot be
  reached, the org is still created, and the admin sees which libraries are missing and
  can retry, or point the source at a mirror or an uploaded copy of the release asset
  (air-gapped installs). The manifest keeps the publisher, so a mirror does not change
  the name.
- An existing org is offered new first-party libraries; nothing is installed into it
  without an admin, including at an upgrade.
- From then on they are ordinary LibraryInstallations. An org admin can upgrade one, keep
  an older version, or remove it, independently of the server version.
- Scarab shows when a newer version of an installed first-party library exists, and marks
  it when the newer version is a security fix.
- Removing one is the admin's call. Pipelines that `invoke` it then fail to compile with
  "`lib:thulasi-ram/clone` is not installed in this org", never in the middle of a Run.

### 6. Yanking

An org admin can **yank** a LibraryVersion in that org. A yanked version refuses:

- new Runs whose `origin` includes it, at `run.create`;
- Steps whose `origin` includes it, at `step.admit`, and gates whose `origin` includes it,
  at `gate.resolve`, including Reruns and Retries of Runs that already used it.

The refusal names the version and the reason the admin gave. Yanking is a PolicyInvariant
(ADR-0075 §7): no Policy can let a yanked version run, and a Policy in `audit` mode or with
`except_projects` cannot either. Yanking tightens and needs no step-up; un-yanking loosens
and does. A yank applies to one org. Other orgs on the same install keep running that
version until their admins yank it or upgrade.

Most first-party security fixes are to an **image** a library pins (such as
`scarab-clone`), not to the YAML. The fix is a new LibraryVersion that pins the patched
digest, plus a yank of the old one.

### 7. Plugins act as Principals and resolve their own gates

A plugin acts through the same public API people use, under its **PluginPrincipal**
`plugin:<installation-name>`, unique within its org (installation names are unique per
org, migration `0055`). Core never calls a plugin.

**Credential.** The plugin calls the API with a projected ServiceAccount token for the
audience `scarab-api`, beside the `scarab-broker` token it already has: rotated by the
kubelet, verified by TokenReview, and refused once the installation is killed. It holds
no issued API token. Its RBAC role, **PluginGateResolver**, allows exactly two things in
its own org: listing the gates of kind `plugin` that name it, and resolving them. A plugin
never approves a manual gate.

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
resolve fails the gate with the reason. `gate_expires_after` is **required** on a plugin
gate, since gates otherwise wait forever (`scarab-pipeline/src/lib.rs:408`); it carries to
the gate node. The gate uses the existing machinery: its own clock, expiry, holding only
its dependents, failing with a reason (ADR-0070 §9, kept by ADR-0074).

**The plugin finds its work by asking:**
`GET /v1/orgs/{org}/gates?pending=true&releaser=me` lists the open waits naming the
calling plugin, with their interpolated inputs, paged by a cursor over wait ids. A
plugin asking about another org gets 404. The query reads current state, so a restarted
plugin catches up with nothing to replay. The same query, for a person, serves the
"waiting on you" inbox.

**The plugin resolves the gate:** `POST /v1/runs/{id}/gates/{step}/release` with
`{"passed": true, "outputs": {...}}` or `{"passed": false, "reason": "..."}`, fenced on
the wait's Attempt (ADR-0021), so an answer to a superseded or retried wait is dropped.
The release is journalled with `by: plugin:argocd-prod`. Outputs obey the size limits of
a Step's results and are read downstream as `${{ outputs.live.revision }}`.

**Two keys, as ADR-0072 §3 requires:**

1. An org admin installed the plugin with the `resolve_gates` capability, which lists its
   gate definitions.
2. The typed setting "plugins whose gates may run here", at the org or on the Run's
   Environment, lists that installation. Its default is empty, so a `run.create` deny
   refuses `plugin:` targets until an admin lists them (ADR-0075 §7).

**When the plugin does not answer,** the expiry fails the gate, and anyone with write
access on the Project can reject it sooner with a reason. A killed or uninstalled plugin
changes nothing: its gate waits for one of the two. Nobody can pass a plugin gate in
the plugin's place. A fallback release by named people, and progress posts from the
plugin, can be added if users need them.

**Where the plugin's knowledge lives.** For Argo CD, `scarab-plugin-argocd` keeps the code
that reads an Application and judges its state (`assess.rs`, `version.rs`). Its
`wait-live` decides on the target's images, Synced and Healthy, and on whether the sync
of the current target failed: the same checks as the documented wait Step
(ADR-0074 §5). It reads Argo through the CredentialBroker; the Pipeline never holds the
Argo token.

**Gate plumbing this needs:** `plugin` in `GATE_KINDS`; the release endpoint accepting a
PluginPrincipal for gates of kind `plugin` that name it (today it accepts only `external`,
and returns 404 when no gate token secret is set); `plugin` in the reject allow-list; a
`by` on gate release; an outputs write path for gates; typed `Interface.outputs`; the
`scarab-api` token audience.

### 8. What the plugin system keeps and drops

- **Kept:** the plugin runtime and isolation, the CredentialBroker with read rules,
  PluginConnections, install with digest pinning, the kill switch, plugin-owned storage
  (ADR-0074 §7), PluginPrincipal.
- **New capability:** `resolve_gates`, for gates of kind `plugin` only.
- **Dropped:** `offer_actions`, `plugin_action`, `subscribe_events` and `approve_gates`. A
  plugin is never an approver on a manual gate. A plugin that wants to react to Runs reads
  the API like any client. An event feed can come later for every API client.
- **Later:** `contribute_ui`, and write actions through the broker (sync, promote), which
  need a Policy-based answer to "which Project may act on which vendor object" first.

### 9. `clone` becomes a first-party library

The `clone` Step kind is replaced by `lib:thulasi-ram/clone`:

```yaml
# libs/clone.yaml
kind: library
interface:
  inputs:
    - { name: depth,      type: string,  required: false, default: "1",
        validate: 'value == "full" || value.matches("^[0-9]+$")' }
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
  invoke: lib:thulasi-ram/clone
  with: { depth: full, lfs: true }
  timeout: 1800                     # carried: the library leaves it unset
```

Core keeps two primitives.

**`forge_token: read | write`** on a Step. Core mints the credential at each launch of an
Attempt, including a re-drive, and mounts it as two files, `/scarab/secrets/forge-token`
and `/scarab/secrets/forge-username` (Forgejo's username is `scarab`; GitHub's is
`x-access-token`). It is never an environment variable. The clone script reads both. Two
PolicyInvariants (ADR-0075 §7) bound it:

- **Which Steps may receive it.** A Step receives a forge credential only when the forge
  mints a short-lived token scoped to the Run's repository (GitHub App mode), or when the
  Step's `origin` is an installation the org trusts with long-lived credentials.
  Otherwise `run.create` refuses the Run. Trust is keyed by the installation's source, not by
  its name: it carries across upgrades from the same source, each confirmed with its
  diff (§4). By default the org trusts its installation of the first-party clone library
  (§5), wherever that copy was fetched from. On Forgejo and GitHub PAT connections, which hand out a long-lived token, only
  the trusted library's Step receives it, and its image and command come from the
  library, not from the repository. This keeps today's guarantee that a repository's own
  code never reads that token.
- **Fork pull requests always get `read`.** The decision is fixed at Run creation, as
  today.

On Forgejo and GitHub PAT connections, `read` and the fork pull request rule are only as
narrow as the token the admin configured: Scarab returns the same token either way, and
`read_only` is the contract that token must meet (`scarab-forge-forgejo/src/lib.rs:750-769`).
Only GitHub App mode mints a narrower token.

`write` needs the forge to mint a write token: GitHub App mode gains a `contents: write`
mint. Forgejo and GitHub PAT connections have no mint, so `run.create` refuses
`forge_token: write` there. A default denies `forge_token: write` unless the Run is Environment-governed and the
Environment restricts refs (non-empty effective allowed refs); the forge port has no
protected-ref concept. A Step with `forge_token: write` uses the Environment's authority
(ADR-0075 §5), so it needs an approved gate where the Environment has approvers.
Policies may tighten this further. A token from a mint is revoked when the Step's Attempt
ends. `forge_token` is part of `step.admit`'s input.

**`${{ event.* }}` in interpolation.** Interpolation reads `inputs`, `outputs` and
`matrix` today; the trigger's `event` is visible only to `when:`. It becomes readable in
`${{ }}` through an allowlist of fields whose values Scarab can check:

| Field | Value | Readable |
|---|---|---|
| `kind` | trigger kind | yes |
| `sha` | 40 or 64 hex characters, where the trigger has one | yes |
| `number`, `issue` | integer | yes |
| `fork` | boolean | yes |
| `run` (upstream) | a Scarab Run id | yes |
| `repo.clone_url` | computed by Scarab from the connection registry | yes |
| `repo.owner`, `repo.name` | checked against `[A-Za-z0-9._-]`, no leading `-` | yes |
| `ref`, `branch`, `tag` | chosen by whoever pushes; checked against `[A-Za-z0-9._/@-]`, no leading `-`, no `..` or `//` | yes, if the check passes |
| `actor` | a forge login; GitHub bots end in `[bot]`, a shell glob | no |
| `body` (comment) | free text from anyone who can comment | no |
| `schedule` | from the Pipeline itself | no |

Provenance fields stay out, as ADR-0057 decided. `event` fields are substituted once, at
Run creation, from the Event the Run is created from, so `run.create` knows each value.
The compiler finds the `event` fields a Pipeline reads in `${{ }}`; `${{ event }}` on its
own is a compile error. When a Pipeline reads a field the trigger does not have, or one
whose value fails its check, `run.create` refuses the Run with the reason ("branch name
contains `;`, which `${{ event.branch }}` does not accept"). A Pipeline that does not
read the field is not affected. `event.repo.clone_url` is computed at Run creation and
stored with the Run (today it is computed at launch,
`crates/scarab-server/src/clone_executor.rs:21-36`).

**What else the `clone` kind does, and where it goes:**

| `clone` kind today | With `lib:thulasi-ram/clone` |
|---|---|
| operator image override `SCARAB_CLONE_IMAGE`, air-gapped mirror | the org installs a LibraryVersion that pins the mirror's image; `just local-helm local` installs one that pins its locally built image |
| credential minted per launch, refreshed on re-drive | `forge_token` mints per Attempt launch |
| username from the forge | the `forge-username` file |
| secrets and workspace inputs forced empty | the library declares none, and a repository cannot change an installed library |
| `timeout`, `resources`, `placement_profiles`, `when` on the clone Step | set on the invoke Step and carried (§1); `placement_profiles` is not carried onto the `forge_token` Step |
| `env` on the clone Step (a proxy, for example) | not carried; a proxy becomes a library input or cluster configuration |
| a trigger with no SHA is refused (`lib.rs:5191`) | reading `event.sha` on such a trigger refuses the Run |
| fork pull request credential fixed read-only (`lib.rs:5222`) | the `forge_token` invariant |
| `ref:` (parsed, never used) | dropped |
| missing-clone lint (`is_clone`) | checks for a Step whose `origin` is the first-party clone library (§5) |
| local executor refuses clone | it refuses `forge_token` |
| Step id `checkout` | `checkout/git`; `needs: [checkout]` still works across the seam |

**Removal.** The release that ships `lib:thulasi-ram/clone` removes the `clone` kind, with
no deprecation window. A `clone:` Step fails the Pipeline check with a message naming
`invoke: lib:thulasi-ram/clone`. Existing orgs are not given the library automatically
(§5), so until an admin installs it after upgrading, every Pipeline that checks out fails
to compile. A bare `invoke:` path becomes a compile error in the same release (§1). The
release notes say both.

## Consequences

- One way to reuse: `invoke`. Authors learn one form, and the three prefixes say where
  the content comes from.
- Libraries and their pinned images can be fixed, yanked or removed without a server
  release.
- An org has one approval point for shared libraries, and per-repository allow and block
  through Policies.
- Plugins get a job that needs no core dispatch: resolving the gates they define. A
  plugin uses the public API, with its projected token and a narrow role.
- Library inputs now bind to `with:`. A library that relied on `${{ inputs.* }}` meaning
  the Run's launch parameters changes meaning; the compile error for undeclared inputs
  catches it.
- A forge token can reach Steps beyond the clone Pod only where the forge mints
  short-lived, repository-scoped tokens. Forgejo and PAT connections keep today's
  limit.
- The release that ships `lib:thulasi-ram/clone` breaks every Pipeline with `clone:` or a
  bare `invoke:` path, and checkouts stay broken in each org until its admin installs the
  library.
- First-party library names follow the repository: if it moves, new installs get a new
  name.
- Scarab stores library content, versions, sources and yanks, which is more to back up and
  migrate.

## Alternatives considered

- **Libraries as OCI artifacts.** Rejected by the owner: an OCI reference would mean "runs"
  for Steps and plugins, and "inline this YAML" for libraries.
- **Remote `invoke` by URL.** Still rejected (ADR-0038): code enters a Run with no
  approval and no review. `url:` is reserved so the form stays unambiguous.
- **First-party libraries bundled in the server binary.** Rejected by the owner: a
  security fix would need a server upgrade.
- **A new file extension (`.scrl`).** Rejected: `.yaml` keeps editor support and schema
  validation; `kind: library` marks the file.
- **Targets told apart by their shape** (a path ends in `.yaml`; `<org>/<name>` is
  installed; `scarab/<name>` is first-party). Rejected in the 2026-10-29 pass: a prefix
  says the kind, leaves room for new kinds, and needs no reserved names.
- **`<org>` as the caller's own org.** Rejected in the 2026-10-29 pass: the segment could
  only ever have one value, and reserving `scarab` would block a forge account of that
  name, since a Scarab org is a forge owner. The segment names the publisher.
- **A reserved `scarab` publisher for first-party libraries.** Rejected by the owner: the
  publisher is the repository's owner, as for every other library.
- **`plugin_action` Steps.** Dropped in ADR-0074: dispatch, leasing and timeouts in core.
- **Plugins as approvers on manual gates.** Rejected in the 2026-10-29 pass: approvals in
  an Environment with approvers need a step-up, which a plugin cannot give, and a plugin
  has its own gate kind.
- **An issued API token for each plugin.** Rejected: issued tokens belong to a person,
  expire and need manual rotation, and are not refused by the kill switch. The projected
  token has none of those problems.
- **An event cursor for plugins.** Rejected as the primary mechanism: `events.seq` is
  allocated before commit, so a strict cursor skips late commits. The pending-gates query
  reads current state.
- **A fallback release by named people when a plugin is silent.** Left out in the
  2026-10-29 pass: the required expiry bounds the wait, and a reject ends it sooner.
- **A vendoring tool and lock file.** Left out in the 2026-10-29 pass: `origin` records the
  content hash without one.
- **The whole `event` readable in `${{ }}`.** Rejected in the 2026-10-29 pass: a comment's
  body and a branch name are chosen by other people and become part of a shell command
  when substituted.
- **`forge_token` for any Step on any forge.** Rejected: on Forgejo and PAT connections it
  would hand a long-lived bot credential to repository code.
- **Keeping `clone` as a Step kind, or as sugar for the library.** Rejected: the kind
  exists to mint one credential and fill a few variables; `forge_token` and `event.*` do
  that for every Step that is allowed one. The owner chose removal in one release over a
  deprecation window.

## Implementation horizon

1. **Library format and binding:** `kind: library`, the three prefixes, compile-time
   binding of `inputs.*`, the fields an invoke Step carries, closed installed libraries,
   `origin` with the invoke chain.
2. **Org-installed and first-party libraries:** LibraryVersion and LibraryInstallation,
   install from git with publisher and source, diff on upgrade, yank, `libs/` and its
   release asset with a manifest, default installation for new orgs with retry and
   mirror, update notices; `run.create` and `step.admit` with `origin` (needs ADR-0075's
   engine).
3. **Clone as a library:** `forge_token` with its two invariants and the default write
   rule, the GitHub App write mint and revocation, `event.*` in interpolation with its
   allowlist and `event.repo.clone_url`, `lib:thulasi-ram/clone`, removal of the kind.
4. **Plugin gates:** `gates:` in the manifest, typed outputs, `invoke: plugin:…`, `gate:
   plugin` with its releaser column, required expiry and interpolation at the wait, the
   pending-gates query, PluginGateResolver with the `scarab-api` token, release with
   outputs, and Argo's `wait-live`.

Decided by the owner on 2026-10-29, in a simplification pass:

- Three prefixes, `file:` required, `url:` reserved and rejected (§1).
- `lib:<publisher>/<name>`; the publisher is the source's owner, first-party libraries
  included; no reserved names (§4, §5).
- Vendoring needs no tool (§3). Installations are org-wide (§4). Yank is per org (§6).
- Plugin gates require an expiry; no fallback and no progress posts; plugins never
  approve manual gates (§7, §8).
- An allowlist of checked `event` fields (§9).
- `clone:` is removed in the release that ships the library, with no deprecation window;
  admins install the library after upgrading (§9).
- The invoke Step's `timeout`, `resources`, `placement_profiles` and `when` carry; `env`
  does not (§1).

## Amendment (2026-10-30): scoped rendering

Decided by the owner on 2026-10-29 and 2026-10-30, and built on the `feat/libraries` branch.
It replaces "Inputs bind at compile time" in §1 and the text join of `when`, and amends
[0041](0041-named-results-and-interpolation.md) §2: an invoke's outputs are read through a
view instead of being rewritten at compile time.

### Why

§1 bound inputs by pasting the caller's `with:` text into each library Step's `${{ }}` at
compile time. That had three faults:

- A caller value could join the library text around it into a new expression. A red team
  showed a value ending in `$` joining the `{{ … }}` that followed it, and the expression this
  made read a Run parameter at launch.
- Caller text was evaluated in the library's scope, so a name in it meant the library's
  value, not the caller's.
- Output reads were rewritten as text at each level of inlining, and once invokes were nested
  the rewrite no longer kept each level's outputs apart.

Go templates, Jinja macros, Helm subcharts and Terraform modules each give a reused unit its
own scope and pass values into it, never text. GitHub's guidance for Actions is to pass an
untrusted value through an environment variable instead of writing it into a script. Scarab
now does both.

### Decision

**Templates are code; values are data.** Every Step's `${{ }}` text is rendered once, when
the Step launches, by one renderer. The debug Pod and the image preview at Run creation use
the same renderer. A rendered value is never rendered again, and nothing is pasted or spliced
into a template at compile time.

**Each library invocation is a scope.** A Step inlined from a library renders in its
invocation's scope:

- `inputs` is the invocation's bound `with:` values. Inside a library, `inputs` never means
  the Run's launch parameters.
- `outputs` is a view from the library's own Step ids to the inlined Step ids, plus the
  exposed outputs of the invokes inside the library.
- `matrix` is the Step's own coordinate only.

A library sees neither the caller's `matrix` nor `event`. A caller that wants a library to
have one of them passes it through `with:`.

A `with:` value is a literal or a template. A literal is converted to the input's type and
checked against `validate:` at compile time. A template renders in the caller's scope when
the library Step launches; the value is then converted to the input's type and checked
against `validate:`, and a value that fails either fails the Step.

`SCARAB_PARAM_*` on a library Step holds only that library's own bound inputs. They are added
after rendering, so a parameter whose value contains `${{` is not evaluated.

**`when` on an invoke Step is a guard.** The invoke Step's `when` and the library Step's
`when` are kept apart, each evaluated in the scope it was written in, innermost out, when the
Run is created. A Step runs only if all of them hold. No results exist yet at that point, so a
library's `when` may not read `outputs`, and neither may an input bound to a template that
reads them.

**An invoke's outputs are read through a view.** A read of `outputs.<invoke>.<name>`,
in the Pipeline file or in a library, resolves through the reader's `outputs` view when the
Step launches. Its text is kept as written. The reader must `needs` the invoke (inside a
library, the local Step it reads), and the compiler adds an edge to the backing Step, which
need not be a leaf. Every Step a view points to is among the reader's `needs`, so a read
never creates a hidden edge.

**Escaping depends on where a value goes, and happens while rendering.** Nothing is escaped
after rendering.

- A `command` argument or an `env` value receives the value as it is, with no escaping.
- A value from outside the Pipeline may not start a `command` argument with `-`. A value is
  from outside when it comes from a Run parameter, a Step's results, an `event` field, or a
  `with:` template that reads one of these through any number of callers. The Step fails as
  a config error. Writing `--name=${{ … }}`, or a literal `--` argument before the value,
  avoids it.
- A rendered `image` must parse as an OCI reference, or the Step fails as a config error
  before launch.
- `${{ }}` inside a script that a shell or interpreter runs (`sh -c`, `python -c`, `node -e`,
  `perl -e`, `ruby -e`, also behind wrappers such as `env`, `timeout` and `busybox`) is a
  lint warning for the Pipeline's own values and a compile error for a value from outside.
  Pass the value through `env:` and read `"$VAR"` in the script.

**Limits.** 1 MiB per Pipeline or library source file, 16 MiB of compiled IR, and 1,024
compiled Steps and gates. Each is a compile error that names the limit.

**IR version 3** marks a Pipeline with a scoped Step. A server refuses to launch a Step of a
Run whose stored IR version is higher than the highest it knows, or is not a whole number. A
server built before this amendment neither checks the version nor knows about scopes, so it
would render a scoped Step without its scope. Upgrades therefore replace the server with the
chart's `Recreate` strategy (the default since PR #50), not a rolling update.

### What this changes elsewhere in this ADR

- §1, "Inputs bind at compile time": replaced by the scope above. The compile error for an
  undeclared input stays.
- §1, "What the invoke Step carries": the AND of `when` stands, as separate guards.
- §2: a library's outputs are still read as `${{ outputs.<invoke-id>.<name> }}`, through the
  view.
- §9: `libs/clone.yaml` reads `${{ event.* }}` inside a library, which a scope does not
  allow. `event` in `${{ }}` is not built yet; how the clone library receives the repository
  URL and SHA is left to that work.
- Implementation horizon, item 1: "compile-time binding of `inputs.*`" now means binding in a
  scope that is read at launch.
