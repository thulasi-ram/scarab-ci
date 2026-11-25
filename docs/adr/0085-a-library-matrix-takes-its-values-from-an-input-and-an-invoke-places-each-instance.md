# 0085. A library matrix takes its values from an input, and an invoke places each instance

- **Status:** Accepted
- **Date:** 2026-11-25
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0084](0084-one-credentialed-step-makes-every-registry-write-for-a-multi-arch-image.md)
  (§8: the deferred items are decided here), [0076](0076-libraries-and-plugins-through-invoke.md)
  (the per-step `placement_profiles` map, a library scope's `outputs`, IR version 4)
- **Builds on:** [0023](0023-dag-shape.md) (static matrix), [0041](0041-named-results-and-interpolation.md)
  (named results), [0055](0055-placement-profiles.md) (placement profiles),
  [0022](0022-upgrades-and-versioning.md) (self-describing Runs)
- **Tickets:** GitHub issues #218 (follow-up) and #219

## Context

ADR-0084 shipped `build-multiarch` with two fixed build Steps, `amd64` and `arm64`, because a
library could not choose how many Steps it makes. Three things were missing in the compiler:

- **Matrix values are literal lists.** A dimension is expanded when the Pipeline compiles, and
  `exclude` sees only the coordinate, so an input could not choose the values.
- **Placement is keyed by library Step id.** An invoke's `placement_profiles` map cuts the
  inlined id at the first `/` or `[`, so every instance of a matrix Step got the same list. A
  build of arm64 and a build of amd64 cannot share one list: each must reach its own node pool.
- **A Step cannot read every instance.** `outputs.<step>` names one Step. A push Step that
  joins all builds had to name each build Step by its id, which a library that does not know
  its own platform list cannot do.

Inputs are `string`, `boolean`, `number` or `choice`; there is no list type. A library is
installed and reviewed as one file, and an org admin trusts its source, not the Pipelines that
invoke it. Placement profiles are named by the operator in the chart's values, and a profile
marked `registry_auth_only` must stay off any Step that runs repository code (0055, 0084).

Separately, `build`'s push Step pushed without comparing the layout to the build Step's digest
when the server collected no results (no results token secret): it found the layout from its
own Step id and logged a warning (issue #219).

## Decision

### 1. A library matrix dimension may be one input

In a library Step, a matrix dimension may be a YAML string that is exactly one
`${{ inputs.<name> }}`:

```yaml
interface:
  inputs:
    - name: platforms
      type: string
      default: "linux/amd64,linux/arm64"
      validate: 'value.matches("…")'
steps:
  - id: build
    matrix:
      dimensions:
        arch: "${{ inputs.platforms }}"
```

- **Resolved when the invoke is inlined**, before the matrix expands, from the invocation's
  binding: the caller's literal `with:` value, or the input's default. The value is split on
  commas and whitespace, empty pieces are dropped, and the order is kept.
- **A template is refused.** A `with:` value that holds `${{ }}` has a value only when a Step
  launches, after the matrix has expanded. The compiler refuses it with a message that says so.
  This also means a library that invokes this one cannot forward its own input to the
  dimension.
- **The input must be a declared `string`.** A boolean or number is refused. A YAML list is
  still a literal list, and `["${{ inputs.x }}"]` is still refused, as before.
- **The values are checked** before they become Step ids: no `[`, `]`, `,`, `=`, control
  character or `::service-`, no value twice, at least one value. `/` is allowed.
- **Outside a library** the form is refused: a Pipeline file's matrix is a literal list, and
  its `inputs` are the Run's launch parameters, known only at Run creation.
- An input that feeds the matrix of a Step with the build sandbox, `registry_auth` or
  `forge_token` counts as a read of that input by that Step, so it needs `validate:` under the
  existing guarded-input rule (0076).
- Every existing limit (65,536 combinations per matrix, 1,048,576 per compile, 1,024 compiled
  Steps, 16 MiB of IR) applies after resolution.

### 2. An invoke's placement map may name a matrix coordinate

The per-step `placement_profiles` map of an invoke accepts a key
`<step>[<dimension>=<value>(,<dimension>=<value>)*]` for a matrix Step the library writes
itself:

```yaml
- id: app
  invoke: lib:thulasi-ram::scarab-ci::build-multiarch
  placement_profiles:
    "build[arch=linux/amd64]": [amd64-builders]
    "build[arch=linux/arm64]": [arm64-builders]
    push: [ecr-push]
```

- A coordinate key places each instance whose coordinate matches all its pairs; it may name
  any subset of the dimensions. A plain key `build` places the instances no coordinate key
  names. An instance with neither gets no list from the invoke (the operator's default
  applies).
- **Checked at inlining**: the step exists, is a matrix Step of this library (not a nested
  invoke), each dimension exists and each value is one of its values. Two keys that could name
  one instance are refused. A key that names nothing gets a hint: `amd64:` on `build-multiarch`
  reads "to place one instance of a matrix Step, write its coordinate:
  `build[arch=linux/amd64]`".
- **Applied when the matrix expands.** Each compiled Step still carries a plain list of profile
  names. The Policy input, admission (0055) and the `registry_auth_only` check see a list, as
  before, and nothing new.
- A key whose instances `exclude` removes is refused at expansion.
- The existing refusals (a Step with `forge_token`, a Step with its own `placement_profiles`)
  apply to coordinate keys too. Through nesting, an instance placed by an inner invoke's
  coordinate key keeps that placement; an outer invoke's list does not override it, the same
  rule as "the library's own value wins".

### 3. A library Step reads a matrix Step's instances as a list

Inside a library, a Step that needs a matrix Step of the same library and reads
`outputs.<step>` gets a list, one entry per instance, in expansion order (dimensions sorted by
name, the last changing fastest, `exclude` applied):

```
[ { "matrix": { "arch": "linux/amd64" }, "results": { "image": { … } } },
  { "matrix": { "arch": "linux/arm64" }, "results": { … } } ]
```

`results` is `{}` for an instance that published none. A field read on the list
(`outputs.build.image`) is refused at compile, and the message shows the `map` form. One
instance stays readable by its id (`outputs["build[arch=linux/amd64]"]`). A Pipeline file's own
Steps see no change.

CEL gains one function, `join`: `<list>.join()` and `<list>.join(<separator>)`, on a list of
strings. cel 0.15's strings extension has a `join` but is not enabled, and enabling it adds
other functions too; Scarab registers only `join`, written in `scarab-pipeline`, with no new
dependency. `size`, `map` and `has` are standard CEL.

Values read from results are text a build wrote. They reach a Step only through `env:`, as
every result does (0041).

`build-multiarch`'s push Step builds its `layouts` value this way:

```
${{ outputs.build.map(b, has(b.results.image) && has(b.results.image.layout)
    && has(b.results.image.digest)
    ? b.results.image.layout + " " + b.results.image.digest
    : "(" + b.matrix.arch + " wrote no layout)").join("\n") }}
```

and sets `SCARAB_INDEX_LAYOUT_COUNT` to `${{ size(outputs.build) }}`.

### 4. IR version 4, and a rolling upgrade

A compiled Pipeline is IR version 4 when a Step's scope lists a matrix Step's instances, or any
expression it stores calls `join`: a `${{ }}` in a template (a Step's image, command or env, a
caller's `with:` in a scope, `concurrency.group`) or a whole expression (a Step's or trigger's
`when:`, a guard, a `validate:`). A server that predates `join` then refuses the Run by its
version instead of failing on the expression. Otherwise its version is decided as before. Runs
stored at versions 1 to 3 load and render unchanged. The stored scope gains one variant, `{"instances": [{"step", "matrix"}]}`.

A server that knows versions up to 3 refuses to launch a Step of a version-4 Run, with the
config failure "this Run was compiled for IR version 4; this server understands versions up to
3" (0076's version check). The chart's default `strategy` is `Recreate`, which stops every old
server before a new one starts. With a rolling update, a Step of a version-4 Run that an old
replica launches fails that way; rerun it once every replica runs the new version.

### 5. Platforms are spelled `linux/amd64`

`build-multiarch` takes `platforms`, a string, default `"linux/amd64,linux/arm64"`, validated
against `linux/<architecture>[/<variant>]` for the architectures BuildKit builds on Linux. The
`linux/` prefix leaves room for another os later without renaming the values Pipelines already
use. The library has one matrix Step, `build`, and one push Step, `push`, with
`inputs: [build]`.

The `/` puts a `/` inside a Step id: `app/build[arch=linux/amd64]`. Code that took the invoke
prefix as everything before the last `/` read `app/build[arch=linux` as the prefix. The
compiler and the web UI now split an id at the last `/` outside its brackets.

Arch tags are unchanged: they come from the image's config, so `linux/amd64` still tags
`<tag>-amd64`.

### 6. A build Step refuses a node of another architecture

The BuildKit script (one copy in `build`, `build-layout` and `build-multiarch`) takes an
optional `SCARAB_BUILD_PLATFORM`. When set, it must be one of the platforms above, and the
script compares its architecture with the node's (`uname -m`, mapped to Go's names). A
mismatch exits 2 before BuildKit starts: "this step builds linux/arm64, but its node's
architecture is amd64 (uname -m: x86_64); the placement map routes this instance to the wrong
nodes". The platform is passed to BuildKit as `--opt platform=`. Unset, nothing changes.

ADR-0084 relied on the push Step to refuse two images of one platform. That still holds, but
the build-time check names the instance whose placement is wrong, and it fails before a build
spends its time.

### 7. Old placement keys are refused

`build-multiarch`'s Steps were `amd64` and `arm64`; they are now instances of `build`. A
Pipeline whose invoke still names `amd64:` or `arm64:` in its placement map fails to compile,
and the error names the coordinate key to write. We did not keep the old keys working as
aliases: an implicit key would be a second spelling that only this library has, and the
compile error tells each caller the one change to make.

### 8. `build`'s push Step refuses when results are not collected (#219)

Without collected results, the push Step had no build digest to check the layout against. It
now fails (exit 2) after its `push: false` and fork checks: "results are not collected, so the
image's digest cannot be checked against the build step's; set the server's results token
secret (SCARAB_RESULTS_TOKEN_SECRET, the chart value secrets.resultsTokenSecret)". The path
derived from the Step id and both warnings are removed. `build-layout` with `image-index` and
`build-multiarch` already needed results: with none, their push Step gets no layout lines and
refuses.

## Consequences

- `build-multiarch` builds the platforms its caller names, one build Step each, and one Step
  still makes every registry write.
- Callers of `build-multiarch` from 0.7.7 must change `amd64:` and `arm64:` to
  `"build[arch=linux/amd64]":` and `"build[arch=linux/arm64]":` when they upgrade the library.
  Step ids change from `app/amd64` to `app/build[arch=linux/amd64]`, and so do the layout keys.
  The per-architecture S3 cache names stay `<cache_name>-amd64` and `<cache_name>-arm64`.
- An install with no results token secret can no longer push with `build`; it could not push
  with the other two libraries before either.
- Pipelines that use instance lists or `join` need a server that knows IR version 4.
- An existing library whose Step read `outputs.<matrix step>.<field>` now gets a compile error
  that shows the `map` form.
- A library that wants the caller to choose its matrix exposes a string input; Pipelines bind it
  as text, not as a template.

## Alternatives considered

- **`${{ matrix.* }}` in profile names** (`placement_profiles: { build: ["${{ matrix.arch }}-builders"] }`).
  The library would choose operator profile names: every org would have to name its profiles
  the way the library spells them. Profile names belong to the operator and are routed by the
  Pipeline author. Coordinate keys keep that choice with the caller and leave the compiled Step
  a plain list.
- **Implicit `amd64:` and `arm64:` keys** that map to the coordinate keys. A second spelling
  for one library, with its own matching rules, kept only to avoid one edit per caller (§7).
- **List-typed inputs** (`platforms: [linux/amd64, linux/arm64]`). A new input type touches the
  input schema, `validate:` (CEL over a list), the guarded-input rules, the Run-creation checks
  and the API. A comma-separated string with `validate:` gives the same values to the matrix.
- **Dimension values set at Run creation**, as 0084 first wrote it. A template in `with:` is
  known only at launch, after the DAG is fixed; resolving the matrix later would make the DAG
  depend on launch-time values. Inlining is the last point where the DAG is still being built.
- **Spelling `amd64` without `linux/`.** Shorter, and no `/` in Step ids. The owner chose
  `linux/amd64` so a later os does not force a rename.
- **Keeping the push without a digest check when results are off** (#219). A push that cannot
  compare the layout to the build's result would upload whatever is at the path.

## Test plan

- **Compiler** (workspace suite): a dimension from an input, with the caller's literal, the
  default, and each refusal (template, undeclared, non-string, bad character, duplicate, empty,
  outside a library); coordinate keys placing each instance, the plain-key fallback, each
  refusal and the hint for `amd64:`; instance lists in expansion order with `exclude`, the
  refusal of `outputs.build.<field>`; `join` on strings and its error on other values; IR
  version 4 set by an instance list and by `join` in a template and in a `when:`, and 3
  otherwise.
- **Ids with `/`**: the invoke prefix of `app/build[arch=linux/amd64]` in the compiler and the
  web UI.
- **`build-multiarch` rendered** (workspace suite): the push Step's `SCARAB_INDEX_LAYOUTS` and
  `SCARAB_INDEX_LAYOUT_COUNT` for two instances, for a missing result, and for three platforms.
- **Scripts** (real `/bin/sh`): `SCARAB_BUILD_PLATFORM` against each `uname -m` mapping, the
  mismatch refusal, the cache suffix; `build`'s push Step refusing with no `/scarab/results`.
- **Docs pages**: the pages' `build-multiarch` blocks compile with the new keys.
