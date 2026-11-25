# 0084. One credentialed Step makes every registry write for a multi-arch image

- **Status:** Accepted
- **Date:** 2026-11-25
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0018](0018-image-building.md), amendment of 2026-11-12 (the build library's
  layout path, and the multi-arch shape in its Consequences)
- **Builds on:** [0007](0007-data-passing-model.md) (fan-in merge, `inputs:`),
  [0023](0023-dag-shape.md) (static matrix), [0033](0033-transitive-skip.md) (a skipped
  Step skips its dependents), [0041](0041-named-results-and-interpolation.md) (named
  results), [0055](0055-placement-profiles.md) (placement profiles),
  [0076](0076-libraries-and-plugins-through-invoke.md) (libraries, `invoke`, trust grants)
- **Tickets:** GitHub issue #218

## Context

Each architecture of a multi-arch image builds natively on its own node pool. QEMU is not an
option: a rootless, unprivileged Step cannot register binfmt handlers, and emulated builds of
native extensions are slow and fail at random. On 0.7.6 a Pipeline publishes one such image
like this:

```
amd64/image ─► amd64/push ─► <tag>-amd64 ─┐
arm64/image ─► arm64/push ─► <tag>-arm64 ─┼─► manifest        ─► <tag>
                                          └─► manifest-latest ─► latest
```

Two `build` invokes push one architecture each, then `image-index` joins them, once per tag.
That costs:

- four Pods that hold the push credential (`amd64/push`, `arm64/push`, `manifest`,
  `manifest-latest`);
- tags written by different Steps, so a Run that fails between them leaves a partial set, for
  example `<tag>-amd64` and `<tag>-arm64` with no `<tag>`;
- four Pod starts after the builds, on pools that usually scale to zero;
- a second index Step for a second tag, which reads both manifests again and pushes the same
  index.

The bytes pushed are not the cost. An index can reference only manifests already in the
repository, so both architecture images are uploaded in any design.

### Why a Pipeline cannot do this itself today

1. **Both builds write the same path.** `libs/build.yaml` writes its OCI image layout at
   `/workspace/.scarab-build/oci`. A Step that needs both builds gets the per-path union of
   their Workspace Snapshots, with the last root in declared order winning (0007, amendment
   of 2026-08-26). The blobs survive because they are content-addressed, but `index.json` and
   `oci-layout` collide, so the merged layout names only the last need's image. The collision
   is a `WorkspaceInputCollisions` event, not an error.
2. **The build library has no hand-off.** `push: false` writes no layout. `push: true` runs
   the library's own push Step, which removes the layout once it is done.
3. **A Step the Pipeline writes never gets the registry credential.** Trust is read from the
   innermost library in a Step's `origin`; a Step with no library origin gets none, and a
   `registry_auth_only` placement profile on it refuses the Run. The unmarked-profile route
   in `pushing-to-ecr.mdx` would give a push role to a Step that runs repository code, which
   is what 0018's split into a build Step and a push Step exists to prevent.

So the change belongs in the libraries.

### What a library can express today

Checked in the code at 0.7.6 for this ADR:

- An inlined library Step's id is `<invoke id>/<library step id>`: `amd64/image`, or
  `deploy[svc=api]/image` under a matrix on the invoke. The executor sets that id in
  `SCARAB_STEP` in every Pod (k8s and local). Templates cannot read it: their scope is
  `inputs`, `outputs`, `matrix` and `event`.
- `needs: [<invoke>]` and `outputs.<invoke>.*` resolve through the library's leaf Steps, the
  ones no other library Step needs.
- A `when:`-skipped Step stays in the DAG as Skipped, and every Step that needs it is Skipped
  (0033).
- `registry_auth` is a boolean on the Step, not a template.
- Matrix dimensions are literal lists expanded at compile time, and `exclude` sees only the
  coordinate, so an input cannot choose the values.
- An invoke's `placement_profiles: { <library step id>: [...] }` is keyed by library Step id.
  The lookup cuts the id at the first `/` or `[`, so every coordinate of a matrixed library
  Step gets the same list.
- Library inputs are `string`, `boolean`, `number` or `choice`, and a choice value must be a
  string. Lists are newline-separated strings.
- A Step with `registry_auth` may read only inputs that are boolean, number, choice, or a
  string with `validate:`, and may hold `${{ }}` only in `SCARAB_*` env values.

## Decision

### 1. Three libraries, no engine change

| File | What it is |
|---|---|
| `libs/build-layout.yaml` (new) | One Step: the BuildKit build Step of `build`, with no credential and no push Step. It writes the layout and publishes where it is. |
| `libs/image-index.yaml` (changed) | `images` becomes optional. New inputs `layouts`, `extra_tags` and `arch_tags`. It pushes each layout by digest, then the index and every tag. |
| `libs/build-multiarch.yaml` (new) | The shape Pipelines should use. Steps `amd64` and `arm64` build with no credential; `push` is its only `registry_auth` Step. |

`libs/build.yaml` keeps `push` a boolean and its `with:` shape. Its only change is the layout
path (§2).

With `build-multiarch` a Pipeline writes one invoke:

```yaml
- id: app
  needs: [checkout, meta]
  invoke: lib:thulasi-ram::scarab-ci::build-multiarch
  placement_profiles: {amd64: [amd64], arm64: [arm64], push: [amd64, ecr-push]}
  with:
    image: "<registry>/app:${{ outputs.meta.tag }}"
    extra_tags: latest
    context: .
    dockerfile: Dockerfile
# deploy by ${{ outputs.app.push.digest }}
```

```
app/amd64 (rootless BuildKit, amd64 pool, no credential) ─┐
app/arm64 (rootless BuildKit, arm64 pool, no credential) ─┴─► app/push (registry_auth):
                                     images by digest, arch tags, index, extra tags
```

`needs: [app]` reaches `app/push`, the only leaf. The per-step placement map works against
these fixed Step ids today.

The same work composed from the pieces, for a Pipeline that needs another shape:

```yaml
- id: amd64
  needs: [checkout]
  invoke: lib:thulasi-ram::scarab-ci::build-layout
  placement_profiles: [amd64]
  with: {context: ., dockerfile: Dockerfile}
- id: arm64
  needs: [checkout]
  invoke: lib:thulasi-ram::scarab-ci::build-layout
  placement_profiles: [arm64]
  with: {context: ., dockerfile: Dockerfile}
- id: image
  needs: [amd64, arm64, meta]
  invoke: lib:thulasi-ram::scarab-ci::image-index
  placement_profiles: [amd64, ecr-push]
  with:
    image: "<registry>/app:${{ outputs.meta.tag }}"
    extra_tags: latest
    layouts: |
      <amd64 layout> <amd64 digest>
      <arm64 layout> <arm64 digest>
# each line is one build's `layout` and `digest` results; the library header shows the
# has(...) form that renders a blank line when a build published none (§3).
# build-layout takes build's inputs except image, push and insecure_push.
```

### 2. The layout path is keyed by a hash of the build Step's id

Every build script (in `build`, `build-layout` and `build-multiarch`) writes its layout at
`/workspace/.scarab-build/<h>/oci`, where `<h>` is the first 32 hex characters of the
SHA-256 of the build Step's own `SCARAB_STEP`. The build Step adds two keys to the `image`
result BuildKit's metadata fills: `digest` (a copy of `containerimage.digest`) and, when it
wrote a layout, `layout` (the path). Consumers (`build`'s push Step, `image-index`,
`build-multiarch`'s push Step) read the path from results. One exception: when the server
collects no results, `build`'s push Step derives the path from its own `SCARAB_STEP`
(`<invoke>/push` → `<invoke>/image`) and logs a `scarab: warning:` line.

- **Why the Step id.** It is unique in the Run and Scarab sets it in every Pod, so two
  builds fanned into one Step write different paths without any input from the caller.
- **Why a hash.** The raw id holds `/`, and under a matrix `[`, `=` and `,`. A `,` inside
  buildctl's `--output type=oci,dest=…` adds output options. 32 hex characters are safe in a
  path and in that CSV value.

### 3. Hand-off is a one-Step library, not a mode on `build`

A hand-off mode on `build` (`push: layout`, or a `handoff:` input) would still run `build`'s
push Step. `needs: [amd64]` and `outputs.amd64.*` resolve through the invoke's leaf, which is
`amd64/push`. A `when:` that skipped the push Step would skip every Step that needs it,
`image-index` included (0033). So the push Step would have to run and pass the layout
through: a Pod start, with the registry credential mounted, that writes nothing. Mounting
the credential only in some modes would need `registry_auth` to follow an input, which
touches the guarded-input rules, the Run-creation checks, admission and the executor's
`registry_auth_only` gate. And `push` cannot become `choice [true, false, layout]`: a choice
value must be a string, so a caller whose `push:` template yields a boolean would break.

`build-layout` has one Step, the build Step, so that Step is the leaf and nothing
credentialed runs. Its result carries `layout`, `digest` (BuildKit's `containerimage.digest`
under a key with no dot, so a template reads it without brackets) and the metadata `build`'s
build Step already publishes. Its source needs only the `build_sandbox` grant.

On a fork pull request, or a Run locked out of its Environment's secrets (`SCARAB_FORK_PR`
not `false`, unset included), `build-layout` still builds, keeps the image in BuildKit, exports
no layer cache, writes no layout and exits 0. Its result has `digest` and no `layout` key. The
caller's `has(...)` form renders a blank line, and `image-index` exits 0 at its fork check,
before it reads any input.

### 4. The push Step checks everything before its first registry write

`image-index`'s Step, and `build-multiarch`'s `push` Step that runs the same script, work in
this order:

1. **Fork check.** `SCARAB_FORK_PR` not `false` (unset included): exit 0 before any input is
   read.
2. **Grammar of every env value**: image, extra tags, arch tags, insecure, images, layouts
   (`<path> sha256:<64 hex>` per line) and the result name. `build-multiarch` builds its
   layouts value from the results of Steps that ran repository code, with no `validate:` in
   between, so the script holds the whole grammar. At least one of `images` and `layouts`
   must name a source, and a mix of sources and empty lines is refused.
   `build-multiarch` renders a build that published no `layout` or `digest` as the line
   `(<arch> wrote no layout)`, not as an empty line, so on a Run that is not a fork the grammar
   check refuses it (exit 2) and the index never leaves an architecture out. It also sets
   `SCARAB_INDEX_LAYOUT_COUNT=2`, so a build result whose `layout` holds a line break cannot add
   an image.
3. **Every layout, read locally:** the path is under `/workspace/.scarab-build/`; no symlink
   at any level, and nothing but directories and regular files; `oci-layout` and `index.json`
   have the expected shape and `index.json` names one manifest; the manifest blob hashes to
   its name and to the line's digest; the config blob hashes to the manifest's
   `config.digest`, and the platform (os, architecture, variant) is read from it. A layout
   that is itself an index, or whose config names no os or architecture, is refused.
   `images` sources are read from the registry by digest, as today.
4. **Every tag, computed:** for each layout source, `<tag>-<arch>`, or `<tag>-<arch>-<variant>`
   when the platform has a variant (`arm/v7` gives `<tag>-arm-v7`). Two sources of one
   platform are refused, and so is any tag over 128 characters, and an arch tag that another
   source or an extra tag would also write. An extra tag equal to `image`'s tag, or named twice,
   is written once.
5. **Only then, registry writes:** each layout pushed by digest
   (`crane push <layout> <repository>@sha256:…`), the arch tags, `crane index append` onto
   `image` (the first tag), and `crane tag` on the index digest for each extra tag. Then the
   result, then `.scarab-build` is removed.

A refusal exits 2 with nothing written. Before this ADR, `image-index` read platforms from the
registry, which for layouts would have meant after the images were uploaded.

The checks exist because a build Pod runs repository code. A Dockerfile `RUN` shares
buildkitd's PID namespace (0018) and can write any path in the build's workspace, another
architecture's layout directory and its own result included. A planted symlink to
`/scarab/registry/config.json` would have crane upload the credential. The manifest hash
check means no layout can be swapped in under a digest; config and layer blobs are uploaded
under their digests, and the registry checks each. What is left is a refusal caused by the
author who controls the image content anyway.

### 5. `inputs:` keeps other Steps' files out of the credentialed Pod

An inlined library Step's `needs` gets the invoke's own `needs` appended (`checkout` and
`meta` in §1's example). The workspace replays in declared order, the last root wins, and
deletions do not survive a fan-in (0007). Without a limit, `build-multiarch`'s push Step
would replay `meta`'s snapshot after the builds', and a `meta` that runs repository code
could plant files under `.scarab-build/` that override the real layouts. §4 would refuse
them, so this fails closed, but the files would still be in the Pod that holds the
credential.

`build-multiarch`'s `push` Step declares `inputs: [amd64, arm64]` (library `inputs:` are
namespaced to the invocation). Its workspace is built from the two build Steps' snapshots
only. Each build Step removes `.scarab-build` before it writes, so files an earlier Step
planted there do not reach its snapshot. Results of other Steps stay readable; only their
files are kept out. It also cuts the Pod's disk (§9).

### 6. Trust: `image-index` now uploads workspace content under an existing grant

Orgs trusted `image-index`'s source with `registry_credential` on its header's statement that
it "runs no code from the repository and reads nothing from the workspace". A grant is keyed
by source, not by version, so it carries to the upgrade. With `layouts`, the upgraded library
uploads content from the workspace under that grant. It still runs no repository code.

- An upgrade install names the grants its source holds (`carry_grants`), so the admin sees
  the credential carry over.
- The header comment is corrected, and the CHANGELOG entry and the libraries docs say what
  changed.
- `with: {image, images}` callers see no change: no workspace read and no new tag writes.

Grants per source:

| Source | Grants |
|---|---|
| `libs/build-layout.yaml` | `build_sandbox` |
| `libs/build-multiarch.yaml` | `build_sandbox`, `registry_credential` (plus `forge_credentials` to push to the forge's own registry) |
| `libs/image-index.yaml` | `registry_credential`, unchanged |

The `ecr-push` policy example in `pushing-to-ecr.mdx`, which lists the library sources whose
Steps may name that profile, adds the new sources.

### 7. Arch tags are on by default

`arch_tags` is a boolean input, default `true`, that applies only to images that came from
layouts. Each such image gets its arch tag (§4) before the index is written. Without a tag,
an architecture image is reachable only through the index, and a registry lifecycle rule that
expires untagged images (ECR has one; other registries' garbage collection has similar rules)
could delete it and break the index. We have not confirmed how ECR treats the children of a
tagged index, so the docs say "not confirmed", not "safe". `arch_tags: false` gives the tag
set the issue drew: `image` and the extra tags. `images`-only callers get no new tag writes
either way.

> Taken by default while the owner was away (2026-11-25); reversible until the release that ships it.

### 8. `build-multiarch` builds a fixed amd64 and arm64

Choosing the architectures needs two compiler features that do not exist (Context): matrix
dimension values taken from an input, and a placement map keyed by matrix coordinate. Without
them, `build-multiarch` has two fixed build Steps, and the caller routes each through the
existing per-step map. A build Step placed on the wrong architecture yields two images of one
platform, which §4 refuses before any write.

Deferred, each a change to `crates/scarab-pipeline`:

- matrix dimension values from an input, set at Run creation;
- a placement map keyed by matrix coordinate (`{"build[arch=arm64]": [arm64]}`), resolved
  after matrix expansion instead of at inlining;
- a `platforms` input on `build-multiarch`, built on the two above, defaulting to amd64 and
  arm64 so today's callers keep working.

> Taken by default while the owner was away (2026-11-25); reversible until the release that ships it.

### 9. Retry and disk

- **Retry.** A retried `push` Step runs every write again, both architectures included;
  per-architecture retry of the push is gone. Each write is idempotent: a push by digest, the
  same index bytes from the same sources in the same order, and `crane tag` to the same
  digest. The builds are separate Steps and retry on their own. A registry with immutable
  tags refuses a moved `latest` with or without this ADR.
- **Disk.** The `push` Pod receives both build Steps' snapshots, with what they received
  (the checkout included), and both layouts. It needs room for all of them in one Pod. The
  total transfer is the same as before. `outputs: [.scarab-build]` on the build Steps would
  cut it to the layouts; that is a follow-up, because what a missing output path does on a
  fork Run (no layout written) is not checked.

### 10. The scripts are copied, and a test holds the copies equal

The BuildKit script is a verbatim copy in `build.yaml`, `build-layout.yaml` and
`build-multiarch.yaml`. The index push script is a verbatim copy in `image-index.yaml` and
`build-multiarch.yaml`. Both scripts read everything from `SCARAB_*` env, so the same text
runs in each file. A workspace-suite test asserts that the copies and the pinned BuildKit and
crane image digests are equal across files.

Inside `build-multiarch.yaml`, `arm64`'s `env` and `command` are YAML aliases of `amd64`'s
(`env: &build_env [...]`, `command: &build_command [...]`, then `*build_env` and
`*build_command`), so an admin reviewing the file at install reads the build script once. Merge keys (`<<: *x`) are not used: serde_yaml 0.9 does not apply them
when it deserialises into the IR's structs.

## Consequences

- The issue's Pipeline goes from six Pods after checkout (two builds, two pushes, two index
  Steps), four of them credentialed, to three, one credentialed.
- Every registry write for the image is in one Step. A refusal writes nothing (§4). A failure
  in the middle of the writes leaves a partial tag set until the Step is retried, and the
  order puts the index after the images it names.
- `build.yaml`'s layout moves from `.scarab-build/oci` to `.scarab-build/<h>/oci`. Its push
  Step reads the path from the build Step's result (from its own Step id when results are not
  collected, §2); `with:` is unchanged. Its `image` result gains `digest` and `layout`.
- 0018's amendment of 2026-11-12 said multi-arch builds are one invoke per architecture plus
  a later index Step. That shape still works; `build-multiarch` is the one the docs lead with.
- Four test crates read `libs/*.yaml` with `include_str!`, so `.github/path-filters.yml` adds
  `libs/**` to `test_and_coverage`. A PR that edits only a library file now runs the Rust
  suite.
- The docs and CHANGELOG say: an upgraded `image-index` uploads workspace content under an
  existing grant (§6); a retried push Step pushes every architecture again (§9); the push Pod
  needs disk for every layout (§9).
- The kind stack's node has one architecture, so both build Steps there would produce one
  platform and the push Step would refuse them. `build-multiarch` end to end needs two node
  architectures; its wiring is covered at compile and admission, and its scripts are covered
  live through `image-index` and `build`.

## Alternatives considered

- **A hand-off mode on `build`.** Runs a credentialed pass-through Pod per architecture (§3).
- **`build-multiarch` invoking `build` twice and `image-index` once.** With `build`, each
  nested push Step stays a leaf, so three credentialed Pods run. Nesting `build-layout`
  instead would avoid that, but a nested `lib:` target is resolved by the name the org
  installed it under. `build-multiarch`'s Steps would depend on which versions the org
  installed under two other names, the admin reviewing it would not see the script that holds
  the credential, and an org that installed those libraries under other names could not use
  it.
- **A caller-given layout name** (`layout: amd64`). An extra input with its own grammar, and
  a collision class: two invokes given one name and fanned into one Step are a last-wins
  union. The checks would make that fail closed, but `SCARAB_STEP` removes the class and the
  input.
- **Exposing the invoke id in the template scope.** A new name in a scope that credentialed
  Steps read, with its tests and docs, for nothing `SCARAB_STEP` does not already give.
- **A generator for the shared scripts, or a script shipped in an image.** A library is
  installed and reviewed as one file and cannot include another file's text. A generator adds
  a build step and a freshness check; a script in an image is one the admin cannot read at
  install. The equality test keeps the copies the same without either.
- **The engine change now** (matrix values from an input, placement by coordinate). Two
  compiler features in the file that also holds the credential rules, a change to the stored
  IR and to Run creation. The invoke shape in §1 does not need them (§8).
- **A `tags:` list in place of `image`** (the issue's sketch). `image` plus `extra_tags`
  keeps `image`'s meaning for existing callers.
- **A push Step the Pipeline writes, with an unmarked placement profile.** Gives a push role
  to a Step that runs repository code (Context).

## Test plan

- **`image-index` script** (workspace suite, real `/bin/sh`, stub crane): layouts pushed by
  digest, then arch tags, index and extra tags, in that order; each refusal in §4 (a symlink
  at each level, a path outside `.scarab-build/`, a non-regular file, two manifests in
  `index.json`, a manifest or config that does not hash to its name, a digest that differs
  from the line's, two sources of one platform, a tag over 128 characters) writes nothing;
  a fork Run with layouts present exits 0 and writes nothing; `images`-only callers make the
  same crane calls as before.
- **Libraries at compile and admission** (workspace suite): `build-multiarch` compiles to
  `app/amd64`, `app/arm64`, `app/push`; only `push` has `registry_auth` and `inputs: [amd64,
  arm64]`; only the build Steps have the sandbox; the credential reaches `app/push` only with
  the `registry_credential` grant and never a build Step; `needs: [app]` resolves to
  `app/push`; `outputs.app.push.digest` compiles; old `with: {image, images}` compiles
  unchanged.
- **Copies equal** (workspace suite): the script and image-digest equality of §10.
- **Live** (`just test e2e`, a local registry): `image-index` joins two single-arch layout
  fixtures with the real crane, which proves `crane push … @sha256:` and that a second run
  of the Step gives the same index digest.
