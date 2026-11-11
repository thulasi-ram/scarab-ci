# 0018. Container image building: rootless BuildKit

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

> **Status note (2026-11-12):** the `build` Step kind is to be replaced by a library,
> `lib:thulasi-ram/build`. See [Amendment (2026-11-12)](#amendment-2026-11-12-build-becomes-a-library).
> Not built yet.

## Context

Image building is the single most common CI task, and on k8s it must happen **without a
privileged Docker socket**. Options: rootless BuildKit (daemonless, efficient cache), Kaniko
(simpler, slower, weaker cache, fading momentum), or "bring your own builder" (punts the #1
task and its rootless/cache footguns onto every user).

## Decision

**Provide a built-in `build` capability backed by rootless BuildKit**: no `docker.sock`, no
privileged pods (privilege via user namespaces). Layer cache to a registry / object store.
Secure-by-default for the most common job.

## Consequences

- A safe build experience out of the box; good DX.
- BuildKit integration + cache plumbing to build/maintain.
- Works with keyless cosign signing ([0015](0015-supply-chain-oidc.md)).

## Alternatives considered

- **Bring-your-own builder** — zero builder code, but punts the #1 task + security footguns.
- **Kaniko** — simpler/daemonless, but slower, weaker cache, fading.

## Amendment (2026-07-17) — registry authentication

> **Status note (2026-07-24 sweep):** the gap below is closed — `build_pod`
> is wired and `ensure_registry_secret` implements both the explicit-secret
> and forge-derived auth paths in scarab-executor-k8s. The decision text
> stands as the record of *how* it was designed.

At the time of writing, `build_pod_for_build` existed but was unwired, with
**no registry auth** (no push/pull credentials). Decision:

- **Registry credentials are a generic scoped secret** (ADR-0037) — a
  `dockerconfigjson`-shaped secret in the Project/Environment scope, injected
  into BuildKit's auth path for both push and private-`FROM` pull. No new
  dedicated `registry:` concept (that would reinvent secret-scoping); reuses
  scope inheritance, fork-PR lockout, and log redaction as-is.
- **Zero-config convenience:** the forge adapter may *derive* a registry
  credential for pushing to the forge's **own** registry (GHCR via the GitHub
  installation token; the Forgejo package registry via its token) from the
  `ForgeConnection` (ADR-0046) — the common "push to my forge" case with no
  secret to configure.
- The push stays fenced via the existing `push:{image}@{digest}` idempotency key
  (ADR-0021).

## Amendment (2026-11-12): `build` becomes a library

Decided by the owner on 2026-11-12 (git-bug 736e397). It replaces the built-in `build`
capability in the Decision above with a library, the way
[0076](0076-libraries-and-plugins-through-invoke.md) §9 replaced the `clone` kind. Rootless
BuildKit, registry credentials from a scoped secret or the forge, and the layer cache stay
decided; what changes is where the code lives and how the BuildKit sandbox is granted.

### Why

A pilot on 0.3.2 (EKS, ECR in another AWS account) hit four limits of the `build` kind:

- no layer cache: this ADR decided one in July and it was never built (e8fdb5e);
- no build arguments (6231918);
- no registry credential that outlives ECR's 12-hour tokens without being stored (ace8135);
- `build.image` is never rendered, so `${{ event.sha }}` reaches buildctl as text (9061495).

Each of the first two is a `BuildSpec` field, a `BuildConfig` field, a buildctl flag and a
server release. BuildKit has more options a team will ask for next: secrets, SSH, platforms,
SBOM and provenance attestations, other outputs. In core, each one is another release.

The BuildKit version is not pinned by a release either. The executor runs
`moby/buildkit:rootless` (`BUILDKIT_IMAGE`, `crates/scarab-executor-k8s/src/lib.rs:5675`), a
tag, so the version follows whatever the node pulls.

`clone` left core for the same reasons: a fix or a new option ships as a library version,
and an org can pin or replace it without a server upgrade.

A plugin ([0072](0072-plugins-a-governed-extension-point.md)) is the wrong home. Plugins now
only resolve gates (0076 §8). A plugin is one long-lived Pod per org whose only egress is the
CredentialBroker. A build needs the Run's workspace, the base-image registries, the cache
store and the push registry, and it needs what a Step has per Attempt: fencing, retries, logs
and results.

The review of this amendment found two holes in the `build` kind as it is today. They are
fixed in the kind first and must not come back in the library:

- a fork pull request's build pushes and receives the registry credential when the Run has
  no Environment (a75eacb);
- `context` and `dockerfile` are not checked, so `context: /scarab/registry` lets a
  Dockerfile read the mounted credential, and an `image` with a `,` adds buildctl output
  options (df660b6).

### Decision

**`build` is a library file, `libs/build.yaml`, published as `lib:thulasi-ram/build`.** An
org admin installs it like any library (0076 §4; §5 as amended 2026-11-10: nothing is
installed by default, and Scarab's own libraries get no special case). Its Step runs a
rootless BuildKit image pinned by digest, calls buildctl, and writes buildctl's
`--metadata-file` to `/scarab/results/image.json`. The Step is named `image` and the library
exposes that output ([0041](0041-named-results-and-interpolation.md) §2), so
`${{ outputs.<invoke>.image }}` reads it, as `outputs.<step>.image` does today.

```yaml
# libs/build.yaml (sketch)
kind: library
interface:
  inputs:
    - { name: image,      type: string, validate: '…an OCI reference with no ","…' }
    - { name: context,    type: string, required: false, default: ".",
        validate: '!value.matches("^/|(^|/)[.][.](/|$)")' }
    - { name: dockerfile, type: string, required: false, default: Dockerfile,
        validate: '!value.matches("^/|(^|/)[.][.](/|$)")' }
    - { name: push,       type: boolean, required: false, default: false }
    # later inputs: build args, cache import/export, platforms, secrets
  outputs: [image]
steps:
  - id: image
    image: moby/buildkit:<version>-rootless@sha256:…
    security: { sandbox: rootless-buildkit }
    registry_auth: true
    env:
      - [BUILD_IMAGE,      "${{ inputs.image }}"]
      - [BUILD_CONTEXT,    "${{ inputs.context }}"]
      - [BUILD_DOCKERFILE, "${{ inputs.dockerfile }}"]
      - [BUILD_PUSH,       "${{ inputs.push }}"]
    command: [sh, -c, 'buildctl-daemonless.sh build … --output "type=image,name=$BUILD_IMAGE,push=$BUILD_PUSH"']
```

The inputs reach the command through environment variables. A caller binds `image` to a
template that reads `event.sha`, which makes it a value from outside the Pipeline, and such a
value may not sit in a `command` (0076, amendment of 2026-10-30). A Pipeline calls the library
like any other:

```yaml
- id: image
  invoke: lib:thulasi-ram/build
  with:
    image: 123456789012.dkr.ecr.ap-south-1.amazonaws.com/app:${{ event.sha }}
    push: true
```

Because the library's Step renders like any Step, `${{ event.sha }}` in `image` works.

Core keeps two primitives. Neither is carried from an invoke Step's `placement_profiles`
(0076 §1 already excludes Steps with `forge_token`): a placement profile is an arbitrary Pod
patch, so a caller could otherwise change the Pod a sandboxed or credentialed Step runs in.

**1. `security: { sandbox: rootless-buildkit }`.** It sets what `build_security_context`
(`lib.rs:5732`) and the AppArmor annotation (`lib.rs:4797`) set today: uid 1000, non-root,
not privileged, `allowPrivilegeEscalation: true` (for the setuid `newuidmap`), seccomp
`Unconfined` and AppArmor `unconfined`. It cannot be combined with `run_as_root`,
`add_capabilities` or `privileged` ([0039](0039-privileged-images.md)).

Which Steps may have it is decided by `origin`. Policies are deny-only
([0075](0075-policy-engine.md) §5), so the rule is a PolicyInvariant, written the way
`forge_token`'s is (0076 §9):

- A Step gets the sandbox only when the library that defines it (the last link of its
  `origin`) is an installation whose source the org trusts for the sandbox. A trusted library
  that invokes an untrusted one does not pass on its trust. Otherwise `run.create` refuses
  the Run and names the missing trust.
- The trust is checked again at each Attempt launch, as `forge_token`'s is, so revoking it
  stops retries and re-drives.
- An org admin trusts a source for the sandbox through the library-trust route, with a
  step-up. The trust row records what it grants: trusting a source with long-lived forge
  credentials does not trust it with the sandbox, and the reverse. Trust is keyed by the
  source without its commit, so it carries across upgrades from that source, each
  confirmed with its diff (0076 §4).
- A Step written in the Pipeline file, or reached through `invoke: file:`, never gets the
  sandbox. Installed libraries cannot `invoke: file:` and their inputs are typed, so a
  repository cannot change the sandboxed Step's command. The repository still controls the
  Dockerfile, as it does today.
- Org and Environment Policies over `origin`, the Project or the Environment may deny it
  further.

Two other ways to govern it were weighed. An ADR-0039 grant needs an Environment, and
pull request CI has none, so pull requests could no longer build. A grant by image digest
says nothing about the command, so any repository could run any command in that image with
seccomp off.

**2. `registry_auth: true`.** Core resolves the Run's registry credential at each Attempt
launch from today's sources (the `REGISTRY_AUTH` secret at Repo scope, then Org scope, else the
forge-derived credential for the forge's own registry) and mounts it as a dockerconfigjson on
tmpfs, with `DOCKER_CONFIG` pointing at it. It is never an environment variable. The
dockerconfigjson names only the registry the credential is for, so core needs no image-host
check: the credential authenticates nowhere else.

- The forge-derived credential can write packages, so it reaches only a Step whose defining
  library the org trusts with forge credentials, whether the forge mints it with a short
  lifetime or returns a long-lived token.
- A `REGISTRY_AUTH` secret follows the rules of any scoped secret
  ([0037](0037-environment-governance.md)).
- A fork pull request never gets a registry credential, governed or not (key on the event,
  as `forge_token` does). This is ADR-0075 §7's "no build push" for fork pull requests.
- The library must keep the credential away from the Dockerfile: inputs checked as in
  df660b6, and whatever the follow-up in df660b6 finds about `RUN` steps reaching the outer
  container decides whether the credential may sit in the build container at all.

How ECR's 12-hour tokens are handled is decided in ace8135.

**What the `build` kind does today, and where it goes:**

| `build` kind today | With `lib:thulasi-ram/build` |
|---|---|
| BuildKit image fixed in the executor, a floating tag | the LibraryVersion pins it by digest; an org pins or replaces it by installing another version |
| sandbox granted to every `build` Step | `security: { sandbox: rootless-buildkit }`, admitted by `origin` |
| registry credential mounted when one resolves | `registry_auth: true` |
| `image` result from `--metadata-file` | the same file, from the library Step named `image` |
| `context`, `dockerfile`, `image`, `push` | library inputs, validated |
| fork pull request: `push` forced off only in a governed Run (a75eacb) | no registry credential for any fork pull request |
| `push:{image}@{digest}` fence (2026-07-17 amendment) | was never wired: `push_fence` is called only from a test (`lib.rs:5778`). Pushing one digest twice is one effect at the registry; moving a tag is at-least-once, as before |
| compile check: a `build` Step sets no image, command or privilege | the `origin` invariant |
| local executor | refuses the sandbox |

**Removal.** As with `clone`, the release that ships `lib:thulasi-ram/build` removes the
`build` kind with no deprecation window. A `build:` Step fails the Pipeline check with a
message naming `invoke: lib:thulasi-ram/build`. Existing orgs are not given the library, so
until an admin installs it, every Pipeline that builds an image fails to compile; until the
admin trusts its source for the sandbox, `run.create` refuses those Runs. Pushing to the
forge's own registry also needs the source trusted with forge credentials. The release notes
say all three.

### Consequences

- BuildKit upgrades and new options ship as library versions. A fix is a new LibraryVersion
  plus a yank (0076 §6).
- The layer cache this ADR decided, and build arguments, become library inputs (e8fdb5e,
  6231918) instead of `BuildSpec` fields.
- A new org cannot build images until an admin installs the library and trusts it for the
  sandbox. Today `build:` works with no setup. This follows the 2026-11-10 decision that
  Scarab's own code gets no default trust.
- Teams can write their own build library (Kaniko, Buildah, BuildKit with other flags). It
  gets the sandbox only if an admin trusts its source, the same as Scarab's.
- Multi-arch builds can be a library that fans out over placement profiles and merges the
  per-arch images; a `platforms` input can come later.
