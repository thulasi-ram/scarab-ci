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

### Breaking changes

- **Migration 0111 adds `kind`, `operations`, `audience` and `oauth_client` to `api_tokens`.**
  No row is rewritten; every existing token keeps working as before. A 0.8.4 server does not
  start on a database that ran it: to roll back, run
  `crates/scarab-db-postgres/rollback/0111_revert.sql` with the server scaled to zero. It revokes
  the tokens that carry an operation list. See the chart README, "Upgrading from 0.8.4".
- **Migration 0114 adds the table `org_fork_run_approval`** (each org's Fork Run approval
  setting; an org with no row has `agent_started`). No row is rewritten. A server that does not
  know 0114 does not start on a database that ran it: to roll back, set it to `off` in every org,
  then run
  `crates/scarab-db-postgres/rollback/0114_revert.sql` with the server scaled to zero. Roll back
  to 0.8.4, not further: 0.8.4 is the first release that can load a Run with the Fork Run approval
  gate. See the chart README, "Upgrading from 0.8.4".
- **An API token no longer approves a gate by itself.** Its approval of any `manual` gate, a
  plain one included, answers `403 step_up_required` with a confirmation request its owner
  confirms with a passkey. A script that approved plain gates with a token must confirm each
  approval or approve from a browser. Rejecting is unchanged. Under `SCARAB_DEV_INSECURE`, where
  step-up is off, a token's approval is still recorded at once.
- **Reads are named by the data they return.** The shared `read` operation is split into
  `run.read`, `step.logs.read`, `step.workspace.read`, `run.artifacts.read`, `repo.read`,
  `account.read`, `access.read`, `security.read`, `secret.names.read`, `settings.read` and
  `server.read`. Security events for reads carry the new names. No operation implies another:
  a token with a list needs `account.read` for `GET /v1/me` and `server.read` for
  `GET /v1/about`.

### Security

- **A dispatch records the kind of ref it ran.** A manual or API dispatch at a bare name such as
  `release/x` was recorded as the branch `refs/heads/release/x` even when the name was a tag.
  Anyone who could push a tag could create `release/x` on their own commit and pass an
  Environment's `allowed_refs` for `refs/heads/release/*`. Scarab now looks the name up: a branch
  is recorded as `refs/heads/<name>`, a tag as `refs/tags/<name>`, and the Run runs that ref's
  commit. A name that is both a branch and a tag is refused with a 400 that asks for
  `refs/heads/<name>` or `refs/tags/<name>`. A name that is neither, such as a short commit SHA,
  is recorded as the full commit SHA, which no branch or tag pattern matches. `HEAD` is recorded
  as the default branch, `refs/heads/<default>`, instead of `refs/heads/HEAD`. A
  `refs/heads/<name>` or `refs/tags/<name>` the repository does not have is refused with a 400.
  The typed ref is trimmed of spaces. A forge error during these ref lookups refuses the
  dispatch ([Runs](/scarab-ci/usage/runs/)).
- **A Forgejo pull request whose head is on no branch is fork code.** Scarab treated a pull
  request as the repository's own code whenever its head was in the same repository. A Forgejo
  AGit-flow pull request (pushed to `refs/for/<branch>`, which read access allows) has its head
  there with no branch, so its Run could get the Environment's secrets. A pull request is now the
  repository's own code only when its head is a branch of the repository; any other is fork
  code, with the fork limits. A dispatch at `refs/pull/<n>/head` applies the same rule. On
  GitHub and Forgejo alike, Scarab now asks the forge for the pull request's head branch before
  it trusts a pull request from the repository, one call per pull request event: the branch must
  exist and point at the pull request's head commit. A missing branch, a branch at another
  commit, or a forge error makes the Run fork code; it is still created. So during a forge
  outage, a pull request Run from the repository gets the fork limits. A known effect: when the
  branch moves between the push and Scarab's check (another push right after), that Run gets the
  fork limits too. A re-run of that Run keeps them; once the forge answers again, start the
  pipeline again with a new push to the pull request or a dispatch at `refs/pull/<n>/head`.
- **A dispatch of fork code starts no follow-up.** A manual or API dispatch whose commit no
  branch or tag of the repository names is fork code, but its Run recorded no `fork` flag, so its
  `on: upstream:` follow-ups started and ran the pipelines at the fork's commit with secrets and
  write tokens. A dispatch now records `fork`, its Run starts no follow-up, and a follow-up whose
  chain started from fork code counts as fork code at launch. Changes you may see: a manual or
  API Run recorded before this release has no `fork` flag and is treated as fork code, so it
  starts no follow-up when it finishes again after a re-run; a `when:` or template on a manual or
  API trigger can read `event.fork`.


### Features

- **Limit an API token to a list of operations.** A token minted with `operations` may use only
  those; every other request is refused with `403 operation_not_granted`. Each token records its
  kind (`issued`), and every rerun, retry, cancel, pin, approval and rejection made with a token
  records the token beside the person in the Run's events, and so does a dispatch on the Run
  itself. The token's id (never its secret) is visible to anyone who can read the Run. A token
  with a list is stored under a different hash, so a server older than this release cannot find
  it during a rolling upgrade. See
  [API tokens](/scarab-ci/usage/api-tokens/#operations).
- **Fork Run approval.** An org setting that holds Runs of fork code at a `manual` gate,
  `fork-run-approval`, before any of their steps or shared services start: `off`,
  `agent_started` (the default: Runs an app started through an `oauth` token) or `all`. Nothing
  issues an `oauth` token yet, so until OAuth sign-in for apps ships, `agent_started` holds no
  Run; choose `all` to hold every Run of fork code. Anyone with write on the repository approves
  the gate from the Run page; an API token's owner confirms each approval, and an app cannot
  approve. The gate fails the Run after 72 hours. It never counts as an Environment approval gate,
  and approving it unlocks no Environment secrets, registry credentials or OIDC roles: the Run is
  still fork code. A Rerun waits at the gate again, and asks the other gates between it and the
  rerun step again (an Environment's approvers, a plugin gate); a Retry does not. After a
  rollback to 0.8.4 a Rerun no longer puts the gate back, so set the setting to `off` and let gated
  Runs finish first. An org admin changes the
  setting from a browser in Org settings › Security
  (`/v1/orgs/{org}/security/fork-run-approval`); a change that holds fewer Runs asks for a passkey
  confirmation. See [Fork Run approval](/scarab-ci/usage/fork-run-approval/).

### Enhancement

- **A manual or API dispatch asks the forge for one ref.** Deciding whether the dispatched commit
  is fork code listed every branch and tag of the repository, page by page, on each dispatch. The
  GitHub and Forgejo adapters now read the one branch or tag the dispatch names.
- **0.8.4 reads a Fork Run approval gate.** A later release adds Fork Run approval, an org
  setting that puts a `manual` gate in front of a Run of fork code. A server before 0.8.4 cannot
  read a Run that has that gate, so roll back from that release to 0.8.4. On 0.8.4 the Run waits
  at the gate as at any `manual` gate, starts none of its shared services until the gate is
  released, and never counts the gate as an Environment approval gate. Nothing in 0.8.4 adds the
  gate.

### Bug Fixes

- **`REGISTRY_AUTH` resolves through the Run's Environment.** A push step's registry credential
  read only the repository's and the org's `REGISTRY_AUTH`, so a value set for an Environment,
  by the repository or by the org, was never used. A push step the Environment admits (approved,
  or an Environment with no approvers) now resolves the Run's Environment chain: the
  repository's value for it, the org's value for it, the repository default, then the org
  default. So an org value set for that Environment now outranks the repository's own
  `REGISTRY_AUTH`. A push step before approval keeps the repository's and the org's values, as
  before. A secret store error now fails the step's launch, where it used to fall through to the
  next scope or to the forge-derived credential, and a value that is not UTF-8 text is refused
  ([Pushing to ECR](/scarab-ci/usage/pushing-to-ecr/)).

### Misc

- **Rollback target for plugin manifest `extensions`.** A later release adds an `extensions`
  field to plugin manifests. 0.8.4 reads and keeps it, so an org's plugin list still loads after
  a rollback from that release, and keeps it when it writes the installation back. It does not
  act on it: such a plugin gets no endpoint and no trust tier. Installing, or upgrading to, a
  manifest that declares `extensions` is refused on 0.8.4. Manifests without the field hash as
  before.

## [0.8.3] - 2026-11-27

### Breaking changes

- **Migration 0110 adds two nullable columns to `attempts`** (a plugin gate's note). No row is
  rewritten. A 0.8.2 server does not start on a database that ran it: to roll back, run
  `crates/scarab-db-postgres/rollback/0110_revert.sql` with the server scaled to zero. See the
  chart README, "Upgrading from 0.8.2".
- **Plugin Deployments are renamed** to `scarab-plugin-<name>-<id>`. On upgrade each plugin is down
  for about 15 to 30 seconds while the old Deployment goes and the new one starts. A 0.8.2 server
  knows only the old names: to roll back, delete the new Deployments first (chart README,
  "Upgrading from 0.8.2").

### Security

- **A Run started by hand at a fork's commit gets the fork limits.** A manual or API dispatch at
  `refs/pull/<n>/head` of a pull request from another repository, or at a commit no branch or tag
  of the repository points at, read and ran the fork's own pipeline with none of the limits a
  fork's pull request gets. Scarab now decides on the commit the forge resolved: it is the
  repository's code only when a branch or tag of the repository, or the head of a pull request
  from one of its branches, points at it. Anything else runs as fork code (no secrets, the
  `env=none` OIDC subject, a read-only clone token, no build push), whoever started it. When the
  forge cannot say, the dispatch is refused. `HEAD` means the default branch (git-bug 14be9e3,
  ADR-0086 §9).

### Features

- **The Run page says why a plugin gate waits.** A plugin can post a note on its gate's open wait
  (`POST /v1/runs/{id}/gates/{step}/note` on the broker listener, `{"attempt", "note"}`, at most
  1024 bytes), and the Run page shows the latest one after "waiting for its plugin". It never moves
  the gate. The Argo CD plugin posts why `wait-live` still waits (`awaiting auto-sync`, the images
  the manifests render, Argo's `ComparisonError`) each time the reason changes. See
  [Deploying with Argo CD](/scarab-ci/usage/deploying-with-argo-cd/#waiting-with-the-argo-cd-plugin).
- **Plugin Pods on tainted nodes.** The chart's `plugins.tolerations` and `plugins.nodeSelector`
  (server: `SCARAB_PLUGIN_PLACEMENT`) go on every plugin Pod and on the isolation probe's Pods.
  Before, on a cluster whose every node is tainted, the probe's Pods stayed Pending and no plugin
  could start. See [Plugins](/scarab-ci/get-started/deploy-helm/#plugins) (git-bug 1b8202c).

### Enhancement

- **Plugin Deployments have readable names.** An installation's Deployment is now
  `scarab-plugin-<name without vowels>-<id>` (`argocd-staging` runs as
  `scarab-plugin-rgcd-stgng-<id>`) in place of `plugin-<id>`, so `kubectl get` shows which
  plugin each workload is. On upgrade the runtime deletes the old `plugin-<id>` Deployment and
  creates the new one once the old Pod is gone, so the two never run at once. The ServiceAccount
  stays `plugin-<id>`.

### Bug Fixes

- **Plugin isolation probe on EKS (Amazon VPC CNI).** The probe's three Pods are now named
  `scarab-probe-<role>-<id>` in place of `isolation-probe-<id>-<role>`. The VPC CNI's network policy
  agent treats Pods whose names match up to the last hyphen as one, so the plugin-side probe Pod
  could take on the control Pod's rules and the probe failed now and then with "NetworkPolicy is
  NOT enforced" on a cluster that enforces it.
- **Argo CD plugin: fixes from a run against a real Argo CD 3.4.** An Application whose auto-sync is
  turned off with `automated: {enabled: false}` is no longer reported as "awaiting auto-sync". A
  `wait-live` gate whose Application Argo cannot compare now gives Argo's `ComparisonError`
  message as its reason in place of "sync status unknown". A Docker Hub short name in the manifest
  (`busybox:1.36.1`) matches its full form (`docker.io/library/busybox:1.36.1`), and a wait for an
  image the manifests do not render gives the images they do (git-bug 9f76ee1).
- **Argo CD plugin: why a `wait-live` gate waits is now logged.** The plugin worked out the reason
  every pass but dropped it, so neither the Run page nor the plugin's log said why a gate was still
  open. It now logs an `argocd: gate waiting` line with the gate and the reason each time the
  reason changes.

## [0.8.2] - 2026-11-26

### Enhancement

- **Text is 0.5px larger across the web UI**: Body and Label go from 13px to 13.5px, Mono from
  12px to 12.5px, and titles from 20px to 20.5px.
- **The step pane's Outputs tab is part of Results.** They showed the same values: Results for
  the selected try, Outputs for the try later steps read. Results now shows the selected try's
  values with the `${{ outputs.<step>.<name> }}` expression under each name, what the try was
  built on, and, when later steps read a different try, a link to it. Links to the Outputs tab
  open Results.
- **The Run page's header is three lines**: the breadcrumb with the Run's number and its ID
  (click the ID to copy it), then the pipeline, title and status with a kept-snapshots mark and
  Live at the right, then where the Run came from.
  Copy run ID left the ⋯ menu, which now shows an icon beside each item. The "N inputs" link left
  the header; the Inputs tab has them, and is now second, after Pipeline.

### Bug Fixes

- **New run with these inputs works when the Run's branch was deleted** (for example when its
  pull request merged and the forge deleted the head branch). The form reads the pipeline at the
  repository's default branch and says so, instead of showing the forge's `422 No commit found`.
  When a pipeline cannot be read at all, the form links to Run pipeline to pick another ref.
- **In the run graph, a step with a sidecar no longer touches the services lane's line above
  it.**
- **A large step result no longer stretches the Results tab.** Results are one row each (name,
  type, value). An object or list, such as the build library's `image` result, is one line that
  opens to its formatted JSON, at most 320px tall, with a Copy button.

## [0.8.1] - 2026-11-26

### Features

- **New run with these inputs** (#225). A Run page's ⋯ menu opens the run form filled in with
  that Run's inputs, at the latest commit on the Run's branch; an input whose earlier value no
  longer passes starts from its default. A new **Inputs** tab lists the inputs a Run was started
  with. See [Runs](https://thulasi-ram.github.io/scarab-ci/usage/runs/#new-run-with-these-inputs).

### Enhancement

- **Run pipeline opens the run form in a dialog** (#225): required inputs, then optional ones, two
  to a row, then the reason. A choice with a few short options is one row of buttons.
- **The run graph opens at 100% and remembers your zoom** (#225). It no longer shrinks to fit; a
  wider graph scrolls sideways. Nodes are more compact and the Run page uses the window's width,
  so up to five columns fit at 100% on a 1440px window. The graph opens at the zoom you last chose
  with ＋/－ (kept in this browser); the reset button goes back to 100%.

### Bug Fixes

- **A long step output no longer runs past its box** in the Outputs tab (#225); each expression
  also lost a stray inner border.

## [0.8.0] - 2026-11-25

### Breaking changes

- **Order: servers first, then libraries and `join`.** Upgrade every server replica before an org
  admin upgrades `build-multiarch`, and before any file calls `join`. A 0.7.7 server cannot parse
  the new `build-multiarch` (a matrix dimension written as text), so a Run that invokes it is
  refused at creation there; and it fails a trigger `when:` that calls `join`, so that delivery
  makes no Run. Redeliver the webhook, or rerun, once every replica is upgraded. To roll back to
  0.7.7, first put the library back to its earlier version. See the chart README, "Upgrading from
  0.7.7".
- **The build library's push step refuses when the server collects no step results** (#219).
  After an org upgrades `lib:thulasi-ram::scarab-ci::build`, a build with `push: true` on a
  server with no results token secret fails (exit 2) with `scarab: push refused: results are not
  collected, …; set the server's results token secret (SCARAB_RESULTS_TOKEN_SECRET, the chart
  value secrets.resultsTokenSecret)`. Earlier versions pushed without checking the layout
  against the build step's digest, and logged a warning. Set `secrets.resultsTokenSecret` (or a
  `SCARAB_RESULTS_TOKEN_SECRET` key in `secrets.existingSecret`) before you upgrade the library.
  `build-multiarch` and `image-index` with `layouts`, which needed results already, now fail with
  `scarab: index refused: results are not collected, …`, which names the same secret. See the
  chart README, "Upgrading from 0.7.7", and
  [Building an image](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#building-an-image).
- **`build-multiarch` refuses its old `amd64:` and `arm64:` placement keys.** Its two fixed build
  steps are now one matrix step, `build`, with an instance per platform. After an org upgrades
  the library, an invoke whose `placement_profiles` map names `amd64:` or `arm64:` fails to
  compile, and the error names the key to write: `"build[arch=linux/amd64]"` and
  `"build[arch=linux/arm64]"`. Step ids change from `app/amd64` to
  `app/build[arch=linux/amd64]`. See
  [Building a multi-architecture image](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#building-a-multi-architecture-image).
- **Pipelines that use the new matrix features, or call `join` in a template, `when:` or
  `validate:`, compile to IR version 4**, which a 0.7.7 server refuses to launch. The chart's
  default `Recreate` strategy stops every old server first; with a rolling update, rerun a step
  that failed with "this Run was compiled for IR version 4" once every replica is upgraded. See
  the chart README, "Upgrading from 0.7.7".
- **A step id may not start with `<invoke id>/` of an invoke beside it.** A step `app/build`
  written beside `invoke` `app` read as one of the library's steps, and the library took its
  results as its own; it is now a compile error. See
  [Reuse: `invoke`](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#reuse-invoke).
- **A `[` or `]` in a step id must pair.** Scarab reads `[…]` as a matrix coordinate when it
  splits an inlined step's name, so an unpaired bracket is now a compile error.

### Features

- **`build-multiarch` builds the platforms you name.** A new `platforms` input, default
  `linux/amd64,linux/arm64`, takes platforms separated by commas or spaces: `linux/amd64` (and
  `/v2` to `/v4`), `linux/arm/v5` to `v8`, `linux/arm64`, `linux/loong64`, `linux/ppc64`,
  `linux/ppc64le`, `linux/riscv64` and `linux/s390x`. 32-bit arm is written with its variant;
  `linux/386` and mips are not offered, because the node check cannot tell them apart. Each
  platform gets its own build step, `app/build[arch=linux/arm64]`, routed with a coordinate key in
  `placement_profiles`, and the one push step joins them all. A build step on a node of another
  architecture fails before BuildKit starts, with a message that names the placement. See
  [Building a multi-architecture image](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#building-a-multi-architecture-image).
- **An invoke places each instance of a library's matrix step.** The map form of
  `placement_profiles` takes coordinate keys, `"build[arch=linux/arm64]": [arm64-builders]`,
  checked when the invoke is inlined. An instance no coordinate key names takes the plain key for
  its step, then the list an outer invoke gives. Each compiled step still holds a plain list, so
  Policies and admission see no change. See
  [Reuse: `invoke`](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#reuse-invoke).
- **A library's matrix can take its values from an input.** A library step's dimension may be
  exactly `${{ inputs.<name> }}`, a `string` input with a default, whose comma- or
  space-separated value the caller writes in `with:` (a template there is a compile error, since
  the matrix expands when the Pipeline compiles). Another step of the library reads every
  instance as a list of `{matrix, results}`, and the new CEL function `join` joins a list of
  strings in any Pipeline or library expression (`${{ }}`, `when:`, `validate:`, `exclude`);
  Policies do not have it. See
  [Libraries: a matrix from an input](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#a-matrix-from-an-input).

### Bug Fixes

- **A library's review page no longer reads its comments as Steps.** The install and upgrade
  review listed an `invoke:`, an `image:` or a `forge_token` that only a YAML comment named, so a
  library whose header comment showed an example invoke was reported as invoking that library.
  The page now cuts each line's comment before it matches.

## [0.7.7] - 2026-11-25

### Security

- **Upgrading the image index library lets it upload workspace content under its existing
  trust.** Earlier versions read only images already in a registry, and their header said the
  step reads nothing from the workspace. Trust is kept per source, so an org that trusted
  `libs/image-index.yaml` with the registry credential keeps that grant on upgrade, and the new
  `layouts` input then makes the step read image layouts from the workspace and upload them. It
  still runs no repository code, and it checks each layout before its first registry write (no
  symlink, only directories and regular files, the manifest and every blob it names hashed, the
  manifest is the digest its `layouts` line gives). The step pushes whatever layout its `layouts`
  lines name: any step of the Pipeline, a plain `run:` step included, can write a layout under
  `/workspace/.scarab-build/` and name its digest, so the digest check shows the layout was not
  changed after it was named, not that a build step made it. The upgrade's review page lists the
  grant it carries. Pipelines
  that pass only `image` and `images` make the same registry calls as before and read nothing
  from the workspace. See
  [Libraries](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#install-scarabs-image-index-library).

### Features

- **One invoke builds and pushes an image for amd64 and arm64.** The new library
  `lib:thulasi-ram::scarab-ci::build-multiarch` (`libs/build-multiarch.yaml`) builds on an amd64
  node and an arm64 node with no credential, and its one `push` step pushes both images by
  digest, pushes the index under `image`'s tag, tags each image `<tag>-amd64` and `<tag>-arm64`,
  and tags the index with each of `extra_tags` (such as `latest`). Before, the same image took two build invokes and an
  image index invoke per tag, with four steps holding the push credential; now one step holds it.
  Deploy by `outputs.<id>.push.digest`. The architectures are fixed to amd64 and arm64, so the
  cluster needs nodes of both. Arch tags are on by default (`arch_tags`), so a registry rule that
  expires untagged images does not remove an image the index names. A retried push step pushes
  both architectures again, and its Pod needs disk for the checkout and both image layouts. Each
  build keeps its own S3 layer cache, `<cache_name>-amd64` and `<cache_name>-arm64`. On a
  fork pull request the builds write no layout and the push step pushes nothing and exits 0.
  Install it and trust its source for the build sandbox and with the registry credential; an
  ECR `ecr-push` Policy must list its source. See
  [Building a multi-architecture image](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#building-a-multi-architecture-image)
  and [Pushing images to Amazon ECR](https://thulasi-ram.github.io/scarab-ci/usage/pushing-to-ecr/#one-image-for-amd64-and-arm64).
- **The image index library pushes images that builds left in the workspace.** New inputs:
  `layouts` (one `<path> sha256:<digest>` per line, from the new
  `lib:thulasi-ram::scarab-ci::build-layout` library, which builds with no credential and pushes
  nothing), `extra_tags` (more tags for the index, so a second tag no longer needs a second index
  step) and `arch_tags` (default `true`, for images from `layouts` only). `images` is now
  optional; at least one of `images` and `layouts` must name an image. Every check the step
  makes itself runs before the first registry write; a failure crane or the registry reports
  after that can leave images pushed by digest with no tag, and a retry pushes them again. The
  writes go in this order: images by digest, the index, arch tags, extra tags. arm64 is tagged
  `<tag>-arm64` whether or not its config names `v8`; a 32-bit arm image is tagged with its
  variant, `<tag>-arm-v7`. Two images that differ only by `arm64` and `arm64/v8` are one platform
  and are refused, `images` sources included. See
  [Joining images into one multi-architecture image](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#joining-images-into-one-multi-architecture-image).

### Enhancement

- **The build library writes its image layout at `/workspace/.scarab-build/<key>/oci`**, the key
  the first 32 hex characters of the SHA-256 of the build step's id, so two builds whose
  workspaces a later step receives no longer overwrite each other's `index.json`. Its `image`
  result gains `digest` (a copy of `containerimage.digest` under a key with no `.`) and `layout`.
  `with:` is unchanged. A step that read `.scarab-build/oci` itself must read
  `outputs.<id>.image.layout` instead. See
  [Building an image](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#building-an-image).

### Documentation

- **Pushing images to Amazon ECR leads with the multi-architecture build library**, and its
  `ecr-push` Policy lists `libs/build-multiarch.yaml` among the sources whose steps may name the
  profile. A Policy copied from the earlier page refuses a Run whose `build-multiarch` push step
  names `ecr-push` until you add that line.

### Misc

- **A pull request that changes only a library file runs the Rust suite.** Four test crates read
  `libs/*.yaml`, so `.github/path-filters.yml` adds `libs/**` (and the two docs pages tests read)
  to `test_and_coverage`.

## [0.7.6] - 2026-11-25

### Breaking changes

- **Migration 0109 adds the table `org_leak_check_limits`** (each org's limits on leak checks).
  It is a new table, so it takes no lock on another and nothing is backfilled. A 0.7.5 server does
  not start on a database that ran it: to roll back, run
  `crates/scarab-db-postgres/rollback/0109_revert.sql` with the server scaled to zero. See the
  chart README, "Upgrading from 0.7.5".

### Features

- **The org can set a secret for one Environment, for every repository.** In Org settings ›
  Secrets, **New secret** asks for a scope: the Org default or one Environment. A Run of `web` in
  `production` now looks a key up in `web · production`, then the org's value for `production`,
  then `web`'s default, then the org default, so a production credential set once by the org is
  not replaced by a repository default. A repository can still set its own value for the
  Environment. The org's Secrets page is now the same list as a repository's Secrets tab: each
  key expands to the Org default and one line per Environment, and each line says which
  repositories set their own value in front of it. Deleting or duplicating an Environment
  includes the org's values for it. During a rolling upgrade, replicas still on the old version
  give Runs the repository default instead of the org's value for the Environment, so set org
  Environment values once every replica runs this version. See
  [Secrets](https://thulasi-ram.github.io/scarab-ci/usage/secrets/).
- **An org Admin can check whether text found outside Scarab holds one of the org's secrets.**
  In Org settings › Secrets, **Check a leaked value** takes pasted text (a token, an `.env` file,
  a log excerpt) and compares it with every current value in the org, its repositories and its
  forge and plugin connections. The answer names each match and its scope, never a value, and
  says when no Run gets a matching value today. A stored value of 20 bytes or more matches
  anywhere in the text; a shorter one only when it is the whole text. Values are compared byte
  for byte, line endings included, so a key wrapped at another width or a file with other line
  endings does not match. The server's own GitHub App key, shared by every installation, is not
  compared. The check runs from the browser only (not with an API token or the CLI), asks for a
  passkey confirmation on every check, takes up to 4096 bytes, and stores and logs nothing of
  the text; each check writes one event with the names that matched. Each person gets a limit
  of checks per org per hour and per day, 20 and 40 by default. Org Owners set the limits in Org
  settings › Security › Leak checks, 0 to 100 per hour and 0 to 1000 per day; 0 turns the check
  off for the org. A value replaced earlier is not kept, so it cannot match. See
  [Secrets](https://thulasi-ram.github.io/scarab-ci/usage/secrets/).

## [0.7.5] - 2026-11-25

### Enhancement

- **The run graph draws fewer edges, and they no longer cross nodes.** A need that another need
  already depends on is not drawn: when `bump` needs `meta` and `manifest/index`, and
  `manifest/index` needs `meta`, only `manifest/index → bump` is drawn. Every edge turns in the gap
  just before its target, so edges into one column share one line. Hovering a step lights it,
  every step it needs (drawn or not) and the edges between them. The selected step is scrolled
  into view when the page opens.
- **A step carried over from an earlier version has a dashed outline at full contrast.** It was
  drawn at half opacity, so a version where most steps were carried over looked disabled.
- **A run's versions are one thin line under the notices, replacing the version dropdown.** Each
  version is a dot coloured by how it ended (one tick each past 8, so 100 versions still fit on
  one line); hovering names it, ← and → step through them. When the line does not fit, it
  scrolls: the edge with more fades out and ‹ › buttons page through. While an older version is in view, the
  line is the top edge of a frame around the rest of the page: a muted green on the latest version, grey
  on an older one, where it says the version is read-only and offers the latest. The graph is no
  longer dimmed, and the bar above it says what that version did differently from the latest (on
  the latest, that it is the latest). Approvals stay above the line. The status badge says how
  the version in view ended. A run with one version shows no line.
- **The run header reads like the runs list.** Line 1 is the pipeline, a slash and the headline;
  line 2 is what started the run and what it was about, then the commit, the author and a deploy
  run's Environment as one group. The start time and duration sit at the right edge, under the
  actions. A run whose workspace snapshots are kept shows a "kept by …" mark there (an expired
  one, "snapshots expired"), and its full note on hover, in place of a sentence of its own.
- **The approval row always keeps its space**, with or without an approval waiting and on any
  version, so the page never moves when an approval comes or goes.
- **The summary line is gone.** Only what needs attention shows, at the right of the run tabs:
  how many steps failed and how many are held back and why ("◷ 2 waiting for a node"). A run that
  is fine shows nothing there.
- **The graph panel opens with one dark bar**: what the version in view did differently and how
  many steps it carried over on the left; fit and zoom (or the List view's All / Failed), then a
  Graph | List switch on the right. The second row of tabs and the "select a step" hint are gone.
  The List view is drawn in the same dark panel, and a carried-over step has a dashed "from v2"
  tag, the list's form of the dashed outline.
- **The step's name and actions sit above its card, and the card opens with its tries**: a band
  drawn like the versions line, one numbered dot per try (one included), coloured by outcome;
  each try's card says what it did, how long it took, why it ran and when. After the tries, the
  band says how a try that did not fail is doing. The Logs, Results and Workspace tabs no longer
  repeat the try, and the step's artifacts link sits at the right of the tab row.
- **A failed try opens on an Error tab**: how it failed, the reason in full, how long it waited
  for a node before it ran, when it started and why, and the last lines of its log, with a link
  to the whole log. The red box above the tabs is gone. See
  [Run logs](https://thulasi-ram.github.io/scarab-ci/usage/run-logs/).
- **More room between the parts of the run page.**

## [0.7.4] - 2026-11-24

### Security

- **`scarab --help` no longer prints the value of `SCARAB_TOKEN`.** clap showed every
  env-backed flag as `[env: NAME=value]`, so the help for `run`, `validate`, `logs`, `rerun` and
  the `security` subcommands printed the API token to anyone reading the terminal. Help now names
  the variable only.

### Bug Fixes

- **Run pipeline starts on the repository's default branch.** The page started on `main`; on a
  repository without that branch the pipeline list failed to load, and after that the page no
  longer updated, so choosing `master` fetched the list but never showed it. A failed load now
  leaves the page working, and `GET /v1/repos/{org}/{repo}/refs` returns `default_branch`, which
  the picker starts on. When the forge does not name one, the list loads at `HEAD`.

## [0.7.3] - 2026-11-24

### Breaking changes

- **Migrations 0106, 0107 and 0108 add nullable columns to `runs`.** 0106 adds `pipeline_path`,
  `pipeline_source` and `pipeline_source_sha` (the pipeline file a Run was created from), 0107
  adds `superseded_by` (the newer Run whose start cancelled this one) and 0108 adds
  `origin_pr_head` (a pull request's head branch). No row is rewritten and none is backfilled:
  Runs created before the upgrade have no saved file, no recorded replacement and no head branch.
  Each `ALTER` waits at most 5 seconds for its lock on `runs`; on a busy database the server stops
  with `canceling statement due to lock timeout` and runs it again when Kubernetes restarts it.
  While one waits, reads and writes of `runs` wait too, up to 5 seconds per migration. A 0.7.2
  server does not start on a database that ran them: to roll back, run
  `crates/scarab-db-postgres/rollback/0106_revert.sql` with the server scaled to zero. See the
  chart README, "Upgrading from 0.7.2".

### Features

- **The Run page's Manifest tab shows the pipeline file the Run was created from**, as Scarab
  saved it when it created the Run, with each step's result beside its `- id:` line, a failed
  step's lines tinted, what each `invoke:` line became (library, content hash, Steps, and a note
  if the version was yanked since), Go to failure, Wrap, Copy, Download and a link to the file
  on the forge at that commit. `GET /v1/runs/{id}/manifest` returns the file and the libraries the
  Run invoked. Runs created before this release, and Runs started through `POST /v1/runs`, have no
  saved file. A long file shows its first 5,000 lines and a line over 2,000 characters shows cut
  and without colours; Copy and Download give the whole file. See
  [The Manifest tab](https://thulasi-ram.github.io/scarab-ci/usage/runs/#the-manifest-tab).
- **The runs list on a repository's page is redesigned.** A status control with counts (All, Running,
  Waiting, Failed, Refused); one Filter panel (pipeline, Environment, author, branch, pull
  request, deploys only, scheduled only) with removable chips, kept in the URL; Runs grouped by
  day; each row names the trigger and what it was about (`pr · #131 feat/x`, `push · main`,
  `tag · v0.7.2`, `schedule · 0 2 * * *`), the commit, the author and a deploy Run's Environment;
  a status word whose card says where a Run failed or what it waits for; refused triggers with
  their reason. The list checks for new Runs every 10 seconds while it is visible, and Runs that
  start while you read wait behind an `N new Runs` pill. Runs list rows gain `failed_step`,
  `failed_steps_count` and `waiting_for`. See
  [The runs list](https://thulasi-ram.github.io/scarab-ci/usage/runs/#the-runs-list).
- **A pull request Run records the branch it comes from**, as `origin_pr_head` on the runs list
  and on `GET /v1/runs/{id}`. The runs list shows it after the number and the Branch filter
  matches it. For a pull request from a fork it is the fork's branch name; Scarab shows it and
  never uses it to decide what runs. See
  [What a row shows](https://thulasi-ram.github.io/scarab-ci/usage/runs/#what-a-row-shows).

### Enhancement

- **A Run that a newer Run replaced is recorded as replaced, not only as cancelled.** When a newer
  Run of the same pipeline on the same ref or pull request, or a newer Run in a
  `cancel-in-progress` concurrency group, cancels an older one, Scarab records the newer Run on
  it (`superseded_by`, sent as `replaced_by` on the runs list). The list shows the older Run as
  Replaced, under the Run that replaced it; before, it read Cancelled, like a Run a person
  cancelled. See [Replaced Runs](https://thulasi-ram.github.io/scarab-ci/usage/runs/#replaced-runs).

### Bug Fixes

- **A Run is stamped with at most 200 characters of its pipeline's `name:`.** The name is text
  from the pipeline file, a fork pull request's included, and every runs list row sends it on
  each poll, so a long one was sent whole every 10 seconds to everyone reading the list. Longer
  names are cut on a character boundary. An `on: upstream:` name is compared in the same cut
  form, so a follow-up of a pipeline with a long name still starts. The compiler still accepts a
  name of any length.
- **The step pane's Source tab names the pipeline file the Run was created from.** It guessed
  `.scarab/<pipeline>.yaml`, which is wrong when a file's `name:` differs from its file name. It
  now reads the saved path, and for a Run with no saved file it reads `the ci pipeline`.

## [0.7.2] - 2026-11-23

### Breaking changes

- **A webhook delivery that left a Pipeline undecided answers 502, and one another try is
  processing answers 409.** A delivery that created some Runs while a Pipeline file could not be
  read answered 202 with `"incomplete": true`; it now answers 502, and the body lists `run_ids`,
  `refused`, `earlier` and `undecided`. A redelivery while an earlier try of the same delivery
  runs answered 503 on the same server replica and `duplicate delivery` on another; it now
  answers 409 on any replica. The overload answer stays 503. Without forge connections
  configured (no delivery ids kept), a delivery that created some Runs while a file could not be
  read also answers 502 now. A tool that watches the forge's delivery log sees these codes. See
  [Webhook deliveries](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#webhook-deliveries).
- **Migrations 0104 and 0105.** 0104 alters `runs` and then `webhook_deliveries` with a 5-second
  lock timeout; on a busy database the server can stop with `canceling statement due to lock
  timeout` and start on its next restart. While it waits, reads and writes of `runs` wait too,
  up to 5 seconds per start attempt. 0105 builds an index concurrently. To roll back, run
  `crates/scarab-db-postgres/rollback/0104_revert.sql` with the server scaled to zero. See the
  chart README, "Upgrading from 0.7.1".

### Bug Fixes

- **A redelivery creates only the Runs a delivery is missing** (git-bug 61174ee). A delivery kept
  its id as "done" once recorded, so a redelivery could not repair one that stopped part way: a
  Pipeline file the forge could not read, a server that stopped mid-delivery, or a second server
  replica that saw the id while the first was still working. Scarab now records what each
  delivery decided for each Pipeline file, in the same transaction as the Run's first row or the
  refusal, and a GitHub Redeliver decides only the files no earlier try decided. A push, a pull
  request or a comment on a pull request creates nothing on a redelivery when a newer delivery of
  that ref or pull request arrived at another commit, and fails its check with "no longer the
  head of". A comment, tag or release delivery reads `.scarab/` at the commit its first try
  resolved; when that commit cannot be resolved it reads at the ref, as before, and a comment on
  a pull request whose head cannot be read runs without the head rule. A Run whose creation stopped
  part way is replaced by the redelivery. A Forgejo Replay has a new id and starts the Runs
  again, as before. See
  [Webhook deliveries](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#webhook-deliveries).

### Enhancement

- **Rerun, retry and gate actions on a Run a redelivery replaced answer 409**, naming the Run that
  replaced it. See
  [Webhook deliveries](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#webhook-deliveries).
- **Webhook deliveries are pruned after 30 days.** `retention.webhookDeliveryDays`
  (`SCARAB_RETENTION_WEBHOOK_DELIVERY_DAYS`) sets the period; the server refuses to start under
  7 days. The table was never pruned before. See
  [Environment variables](https://thulasi-ram.github.io/scarab-ci/usage/environment-variables/).

## [0.7.1] - 2026-11-22

### Breaking changes

- **A GitHub push of 300 or more changed files has an unknown `event.changed_paths`, so its
  path-filtered Pipelines run.** A push's list now comes from the forge's compare API (see
  Enhancement), and GitHub's lists at most 300 files. Before, a push of up to 2,999 files had a
  list, read from the webhook payload. The activity log says `300 or more files changed`.
- **A push's file list on Forgejo needs Forgejo 16.0.1 or later.** An older Forgejo, or a Gitea
  whose compare answer has no per-commit files, gives an unknown list on every push, so its
  path-filtered Pipelines always run. Pull requests are not affected.
- **A `changed_paths` filter is not a deploy guard.** An unknown list counts as true and the
  Pipeline runs, and a list is unknown during a forge outage, when the forge is slow or
  rate-limits Scarab, and on every GitHub push of 300 or more files. A Pipeline that runs a deploy
  when `infra/` changed also runs then; guard a deploy with an Environment's protection and
  approvals. The reasons a list is unknown changed: "the push payload lists no commits" and "the
  push payload lists only some of its commits" are gone; new ones say the forge answered 404, the
  branch's earlier head is not on the new head's first-parent line (Forgejo), more than 1,000
  commits were pushed (Forgejo), the answer was over 16 MiB, the answer did not list every pushed
  commit or file, or the payload's commit ids are not commit ids. A tool that matches the reason
  text in the activity log needs the new ones. See
  [Changed files](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#changed-files-eventchanged_paths).
- **Migration 0101 validates three of 0099's checks on `log_chunks` when the server starts.**
  `log_chunks_kind`, `log_chunks_reason` and `log_chunks_body` are checked against every stored
  row, under a lock that blocks no read or write, so on a large table the start takes longer.
  The migration waits at most 5 seconds for that lock: while a manual `VACUUM` or `ANALYZE`, an
  anti-wraparound autovacuum or a `CREATE INDEX CONCURRENTLY` runs on `log_chunks`, the server
  stops with `canceling statement due to lock timeout` and Kubernetes restarts it until the other
  session ends. Migration 0102, right after it, adds the `values_too_large` reason to
  `log_chunks_reason` (see Security): it drops the check and adds it again `NOT VALID`, scanning no
  row, under a lock that every other lock on `log_chunks` blocks: an open transaction that read the
  table (a `pg_dump`, a long `SELECT`) makes it stop with the lock timeout, and while it waits, up
  to 5 seconds, log reads and writes on `log_chunks` wait behind it. Migration 0103 then validates it again in its own transaction, under the same lock and
  5-second wait as 0101, so the start scans `log_chunks` once more. A 0.7.0 server does not start
  on a database that ran 0101, 0102 or 0103: to go back, scale the server to zero, run
  `DELETE FROM _sqlx_migrations WHERE version IN (101, 102, 103);` and install 0.7.0. To go back
  to 0.6, `rollback/0099_revert.sql` now reverts 0101 to 0103 too, and a second run of it changes
  nothing (it failed with `column "kind" does not exist`). See
  [Migration 0101](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#migration-0101)
  and [Migrations 0102 and 0103](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#migrations-0102-and-0103).
- **A webhook delivery without a delivery id is refused with 400** when forge connections are
  configured, since it could not be deduplicated. GitHub and Forgejo send the header on every
  delivery; a hand-made request or script that posts to `/webhooks/github` or `/webhooks/forgejo`
  must send `X-GitHub-Delivery` or `X-Forgejo-Delivery`.

### Security

- **The refusal for an unknown placement profile no longer lists the operator's profiles.** It
  named every profile in the registry, so a repository author could list them by naming one that
  does not exist. It now names the step and the profile only. See
  [Placement & resources](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#placement--resources).
- **Overlapping secret values are redacted as one `***`, and no byte of either is stored.** The
  redactor replaced the longest value at a position and went on after it, so a value that started
  inside it and ended after it kept its end in clear: with the values `user:hunter2pw` and
  `hunter2pw-and-more-secret`, `dsn=user:hunter2pw-and-more-secret` was stored as
  `dsn=***-and-more-secret` (git-bug 32c3c10, present before 0.7.0). The redactor now finds every
  match first and stores each run of bytes they cover as one `***`; values that only touch stay
  two. A value that repeats a word, printed twice in a row, is now one `***` (`abab` twice, was
  `******`). See [Edges of a value](https://thulasi-ram.github.io/scarab-ci/usage/run-logs/#edges-of-a-value).
- **A log part that starts inside a value no longer stores the value's end, however the log was
  read in chunks.** In 0.7.0, a part after a `source_restarted` note that began with 8 bytes ending
  one value and starting another stored the second value's last bytes when the read was cut after
  those 8 bytes (git-bug 2599a7b). That case now stores `***`.
- **The start of every log is checked against the ends of values.** 0.7.0 starred a value's last
  8 or more bytes only at the start of a part after a `source_restarted` note; now the first part
  of every log gets the same rule. Output that begins with 8 or more bytes that end a secret value
  shows `***` there, and the first bytes of a log can appear a moment later.
- **A log part that lies wholly inside a value is `***`.** A part that starts after a value's first
  byte and whose container ended before the value's last byte (the kubelet rotated the file between
  two lines of a PEM key, and the process ended) stored those lines. It now stores `***` when the
  part is 8 bytes or longer.
- **A step whose secret values add up to more than 256 KiB has its log not stored.** It gets one
  `not_stored` note with the reason `values_too_large`, the server reads none of its log, and a
  failure cause about it is replaced by "the cause was not stored". The redactor keeps up to 52
  bytes of lookup tables per value byte, so the cap holds them at 13 MiB per step being read. A
  server keeps at most 128 MiB of these tables by default (`SCARAB_LOG_AUTOMATA_BUDGET_MIB`, chart value
  `scarab.logAutomataBudgetMib`), frees a step's tables when its last log read ends, and builds
  them for 2 steps at a time; a read that does not fit waits for another to end. A value is never
  redacted in part.
- **A failure cause that ends in the first bytes of a secret value has them replaced by `***`.**
  The executor keeps 1,024 bytes of an API server message (an admission denial) and could cut it
  inside a value; the part before the cut was no occurrence and was stored. A cut message now ends
  with `…`, and the bytes before it count as the end of the cause.
- **Rows already stored are not rewritten.** A value's end that 0.7.0 stored stays stored. A
  re-read of a part stored by 0.7.0 that held an overlap stores one `source_restarted` note and a
  second, redacted copy.
- **Overlapping values are redacted in full only once every replica runs this version.** During a
  rolling upgrade, a 0.7.0 replica that takes over a log's tail can store its own copy with the
  0.7.0 redactor after a `source_restarted` note.
- **Every GitHub and Forgejo answer, and every answer from the login provider, is read up to
  16 MiB.** Only the compare answer had a cap; a forge that sent a longer answer to any other call,
  an error answer included, had it read whole into the server's memory, and so did the OAuth or
  OIDC provider's token, userinfo, discovery and key set answers at login. A longer forge answer
  now fails the call with `the forge's answer was over 16777216 bytes and was not read`, a longer
  provider answer fails the login, and none of either is parsed (git-bug bfa8dfd).

### Features

- **A `when:` can read `event.changed_paths_known`** to tell an unknown changed files list from a
  known one. It is `true` when Scarab has the complete list, an empty one included, and `false`
  when the list is unknown and on every trigger with no list (`tag`, `release`, `comment`,
  `cron`, `manual`, `api`, `upstream`). An unknown list still counts as true, so `when:
  event.changed_paths_known && event.changed_paths.exists(p, p.startsWith("infra/"))` is how a
  trigger skips on an unknown list. A template cannot read it. See
  [Changed files](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#changed-files-eventchanged_paths).

### Enhancement

- **A push's changed files come from the forge's compare API**, from the branch's head before the
  push to its head after it, asked once per delivery and only when a Pipeline whose `push` trigger
  matches reads `event.changed_paths`. The list now includes a file changed only in a merge's
  conflict resolution, on GitHub and Forgejo, and a forced push to Forgejo gives an unknown list
  (`forced push`) instead of only the files of the commits it added, so its path-filtered
  Pipelines run. A forge answer is read up to 16 MiB. See
  [Changed files](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#changed-files-eventchanged_paths).

### Bug Fixes

- **A Pod Security or LimitRange denial caused by a step's `k8s_overlay` fails the step.** It was
  a launch error that the scheduler retried until the Run dead-lettered. When Kubernetes refuses a
  step Pod with a 400, 403 or 422, Scarab now dry-runs the same Pod and, when that is refused
  again, the Pod without the overlay: up to two more requests to the API server. The step fails
  as a configuration error only when the same Pod is refused twice and the Pod without the overlay
  passes. A refusal gone by the dry run, an RBAC denial and a ResourceQuota denial (matched by
  Kubernetes' own `is forbidden: exceeded quota: ` text, so a policy message that only contains
  the words still fails the step) stay launch errors. A webhook that refuses for a reason of the
  moment on both requests fails the step; rerun it. The API server's message in the failure is
  cut to 1,024 bytes, also for a refusal by the chart's admission policy. See
  [Placement & resources](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#placement--resources).
- **A webhook delivery whose forge stopped waiting still creates its Runs.** The work runs in a
  task of its own, and the delivery id is recorded inside it, so a delivery dropped before the
  task started records nothing and its redelivery is processed. At most 64 deliveries run at once;
  one that gets no turn within 10 seconds is answered 503 with nothing recorded. On shutdown the
  server waits up to 20 seconds for running deliveries. See
  [Webhook deliveries](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#webhook-deliveries).
- **A delivery whose Pipeline files could not be read from the forge can be redelivered.** A
  failed read is tried three times. When it still fails and no Run was created, the delivery id
  is forgotten and the answer is 502, so a redelivery creates the Runs; before, the answer was
  200 `no matching pipeline` and a redelivery was a duplicate. When some Runs were created, the
  answer is 202 with `"incomplete": true`.

### Tests

- **`just test chaos kill-tailer`** (rank 2) SIGKILLs the server replica that owns a step's log
  tail lease while the step prints, and checks that each log's lines are stored whole and in
  order or the log ends in a note, and that the step's secret is in no stored object and not in
  the log API.
- **`just test e2e` checks that a rerun step that writes the same bytes skips the step after
  it.**
- **`just test record-compare`** records the Forgejo compare answers the adapter's tests read,
  from the kind stack's Forgejo.

### Documentation

- **Set `kernel.yama.ptrace_scope` to 1 or more on the nodes that run step Pods.** A new section
  says what it stops and what it does not, its cost, how to read a node's value, and how to set
  it on Amazon Linux 2023 nodes with Karpenter. Scarab does not check it. See
  [Hardening the nodes that run step Pods](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#hardening-the-nodes-that-run-step-pods).
- **The 0.7 docs site builds again.** The 0.7.0 publish stopped at a `<file>` in the Pipeline
  syntax page.

### Misc

- **CI builds the docs site on pull requests that change it**, so an MDX error fails the pull
  request instead of the release.
- **The kind stack runs step Pods in the chart's default namespace, `scarab-steps`**, not the
  release namespace. The e2e and chaos harnesses read the step namespace from the server's
  ConfigMap.

## [0.7.0] - 2026-11-22

### Breaking changes

- **Let running Runs finish before you upgrade.** Scarab now stores a step's log only when it
  launched the step with a value ledger (see Security). A step or shared service that runs across
  the upgrade stores nothing after it: its log ends with the note "the rest of this log was not
  stored: this step was started by an earlier Scarab version, so its secret values are not known",
  after the part stored before the upgrade, and a failure cause about it reads "the cause was not
  stored: it may quote secret values that could not be checked". `kubectl -n <step namespace> get
  pods -l scarab.io/run` lists no Pod when no step runs. See
  [Upgrading to Scarab 0.7](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#upgrading-to-scarab-07).
- **Keep the server Deployment's `strategy.type: Recreate` for this upgrade.** With
  `RollingUpdate`, a step that a 0.6 server launches while the new servers start gets the same
  note and no log: its Pod has no `scarab.io/value-ledger` label, and once migration 0099 has run
  the database refuses log chunks from a 0.6 server. `Recreate`, the chart's default, stops every
  old server before a new one starts.
- **A 0.6 server does not start on a database that ran migration 0099, so a rollback needs the
  revert file.** The 0.6 migrator finds version 99, which it does not know, and stops. To go back:
  let Runs finish and stop new ones, scale the server to zero (`kubectl -n scarab scale
  deploy/scarab --replicas=0`), back up the database, run
  `psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f crates/scarab-db-postgres/rollback/0099_revert.sql`
  (it reverts 0100 too), then install the 0.6.1 chart. The revert deletes the notes of logs stored
  by 0.7 and every value ledger; output rows stay, and 0.6 reads them as before. Upgrading again
  runs 0099 and 0100 again. Never drop the `log_chunks_fenced` check on `log_chunks`: during a
  rolling update it is what stops a 0.6 server that is still running from storing output it cannot
  redact. See
  [Rolling back to Scarab 0.6](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#rolling-back-to-scarab-06).
- **Migration 0100 builds an index with `CREATE INDEX CONCURRENTLY`.** It adds
  `attempts_started_at_idx` outside a transaction, so writes to `attempts` go on while it builds. A
  build that fails or is stopped leaves an invalid index behind, and the next server to start drops
  it and builds it again (see Bug Fixes). Migration 0099's checks on `log_chunks` are `NOT VALID`,
  so it scans no stored chunk.
- **Upgrade the chart with the server image: step Pods mount their credentials from Secrets with
  new names.** A step Pod made by 0.7 carries the label `scarab.io/value-ledger: "1"` and mounts
  `<pod>-v-secrets`, `<pod>-v-registry`, `<pod>-v-token` and `<pod>-v-oidc`. The 0.7 chart's
  admission policy allows these and the names 0.6 wrote. Under the 0.6 chart's policy, every step
  Pod that mounts one of the new Secrets is refused and its step fails. A policy or tool of your own
  that matched the old names (`<pod>-secrets`, `<pod>-token`, …) needs the new ones. A re-drive
  that finds a Pod without the label writes no Secret to it; one whose containers never started is
  deleted and created again. See
  [The step Pod admission policy](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#the-step-pod-admission-policy).
- **Forge tokens minted by 0.6 are not revoked after the upgrade.** 0.7 revokes only tokens it finds
  in a value ledger, and a step 0.6 launched has none. A GitHub App token minted for a step that ran
  across the upgrade expires an hour after it was minted.
- **Every replica must use the same master keys, and an old key stays until the rotation metric
  reads 0.** Each step's value ledger is encrypted with the master keys; a replica that cannot
  decrypt it stores none of that step's log. `scarab_secrets_rows_under_nonactive_key` now also
  counts ledger generations under a key that is not the active one; it is set at boot, so restart
  the replicas to read it again. Ledger generations cannot be rewrapped: remove an old key only
  when the metric reads 0 on every replica, at most 24.5 hours after the key stopped being active
  (the longest a ledger lives). Removing it earlier stops the logs of running steps from being stored, and a
  re-drive of such a step fails. See
  [Master keys and value ledgers](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#master-keys-and-value-ledgers).
- **A Pipeline file that does not compile now fails the checks of the Pipelines the event would
  have started.** A broken file still stops every Pipeline in the delivery, and now before any of
  them starts a Run: every file is compiled first. Each file that does not compile is listed as a
  refused Run and posts `failure` under its usual check name with its first compile error, when its
  `on:` names the event or cannot be read. Each other Pipeline whose trigger matches the event,
  `when:` included, posts `failure` with "not run: <file> failed to compile"; one whose `when:`
  cannot be decided posts it too, since it may have run; one whose trigger does not match posts
  nothing, so a Pipeline kept to `main` gets no red check on a feature branch. Before, the broken
  file posted no status, so a required check waited for good, and Pipelines sorted before it had
  already started Runs. See
  [When a Pipeline file does not compile](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#when-a-pipeline-file-does-not-compile).
- **A refused Pipeline now fails its check on the commit.** A Pipeline that a push or pull request
  does not start because a Policy denied it, a sandbox or library rule refused it, an `event` field
  or `forge_token` rule refused it, an approval gate is missing, its `when:` could not be decided,
  or a loop of follow-up Runs or a chain 8 Pipelines long stopped it, posts `failure` under its usual check
  name, on GitHub and Forgejo, with the reason as the description and a link to the repository
  page, where the refused Run is listed. An Environment refusal already did. A branch-scoped
  Environment skipping another branch still posts nothing. Each refused event posts again, also on
  a commit where a later Run of the Pipeline has posted its own status since; the same delivery
  sent twice posts once. See
  [Commit statuses](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#commit-statuses).
- **Closing, merging or labelling a pull request no longer starts its `pull_request` Pipelines.**
  Scarab started Runs for every pull request webhook, so closing or merging a pull request, or
  labelling it, ran the Pipeline again, and a build Pipeline with `push: true` pushed its images
  again. A pull request now starts Runs only when it is opened or reopened, gets new commits, or is
  edited to target another base branch. Other deliveries (closed, merged, labelled, assigned, a
  review request, ready for review, a title or description edit) start no Run and post no status,
  and the webhook answers 200 with `{"ignored": "<reason>"}`. GitHub and Forgejo. See
  [Triggers](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#triggers-on).
- **A step id may not contain `::service-`.** Scarab names a step's sidecar logs
  `<step>::service-<i>`, so a matrix value such as `1]::service-0` gave a step whose log was read
  as another step's sidecar and redacted with that step's values. The compiler refuses a step id,
  written or built from matrix values, that contains it, with one error naming the matrix value.
  See [Fan-out](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#fan-out-matrix).
- **A raw log download writes the server's notes as `[scarab] <text>` lines.** Add
  `?markers=none` to get the stored output bytes only. The `X-Scarab-Log-Markers` header counts the
  notes either way. See [Run logs](https://thulasi-ram.github.io/scarab-ci/usage/run-logs/#through-the-api).

### Security

- **A step's log is redacted with every secret value any launch of it was given, on every
  replica.** Each launch stores the values it hands the step (secrets, registry credential, forge
  token, OIDC token) as a new generation of the step's value ledger, encrypted with the master key,
  in Postgres, before it creates the Pod or any Secret; the replica that reads the log loads every
  generation or none. A re-drive that minted a new forge token on another replica no longer leaves
  the old one unredacted, and a failed re-launch no longer drops the values of the running step.
  The ledger is deleted once every log of the try is stored and its tokens are revoked, or an hour
  after the try or its Run ends once its tokens are revoked. See
  [Secret values](https://thulasi-ram.github.io/scarab-ci/usage/run-logs/#secret-values).
- **Scarab no longer reads secret values back from Kubernetes Secrets.** Neither the log tail nor
  forge token revocation reads them; the Secrets are deleted by name when a try ends.
- **Every forge token minted for a step is revoked once, whichever replica minted it.** Each
  ledger generation records when its token was revoked. A re-drive revokes the tokens of older
  generations only, so a launch that returns late no longer revokes a newer launch's token, and
  the end of the try revokes the rest. A launch whose minted token is in no ledger generation
  revokes it and fails, so a token is never left live with nothing to revoke it. One replica, the
  holder of the lease `ledger-sweep`, retries failed revocations and deletes ledgers every 30
  seconds, in a task of its own beside the scheduler: each step's revocations are cut off at 20
  seconds, a failed one is tried again after 1 minute, doubling to 1 hour, and a ledger past its
  cap (24.5 hours after its first generation, once the try is not running) is deleted anyway; its
  tokens then expire on their own. Tokens minted by 0.6 are not revoked (see Breaking changes).
  See [Forge tokens and when a ledger is deleted](https://thulasi-ram.github.io/scarab-ci/usage/run-logs/#forge-tokens-and-when-a-ledger-is-deleted).
- **A failure cause or infra message is redacted with the values of the step it is about**, read
  from the step's ledger each time a cause is stored, so a value another replica recorded after
  this replica last loaded the ledger is redacted too. A cause about a step with no ledger is
  replaced by "the cause was not stored: it may quote secret values that could not be checked".

### Features

- **The server's notes in a log are their own rows.** A restarted log source, a part not stored
  and an end that could not be read are stored as index rows, not text in the log, so a step that
  prints the same text changes nothing. The Run page shows each as a row set apart from the
  output, the log stream sends `event: marker`, and `scarab logs` prints them on stderr. See
  [Notes in a log](https://thulasi-ram.github.io/scarab-ci/usage/run-logs/#notes-in-a-log).

### Bug Fixes

- **A `when:` that cannot be decided refuses its own Pipeline only.** A trigger or Step `when:`
  that failed to evaluate (for example, one reading an event field that is not there) stopped the
  whole delivery, so the Pipeline files after it in `.scarab/` were not evaluated. That Pipeline is
  now listed as a refused Run with the error and fails its check, and the other files still start
  their Runs.
- **Two replicas no longer write one log at once.** Every log chunk is stored under the lease of
  the log it belongs to, and the database refuses a chunk from a replica whose lease was taken over
  or that holds another log's lease, so chunks are no longer dropped, reordered or pointed at
  another replica's body. Chunk bodies are stored under their content hash, so a second read never
  overwrites a stored body.
- **A log is complete only when its container ended.** A read that ended while the container still
  ran (a kubelet restart, a dropped connection) was taken as the end of the log, so output after it
  was lost. The server now asks Kubernetes and reads again. A crashed sidecar or shared service
  keeps the output of its next run; while its container waits to restart, it is read again after
  3 seconds, doubling to 60.
- **A live log no longer stops on a dead connection.** A followed log read had no read timeout,
  and the Kubernetes client has no TCP keepalive setting, so a connection a node or load balancer
  dropped without a reset froze the log until the step ended. A follow that gets no byte for 5
  minutes now fails, is opened again and is compared with the stored part.
- **The rest of a log is no longer lost or withheld after a broken read.** The "from now on" and
  "withheld" fallbacks are gone: the server compares a new read with the stored part by digests,
  stores the source again after a note when it differs, and no longer fetches every stored chunk
  on each retry.
- **A value cut at the end of a log, or at the start of a part after a restart, is stored as
  `***`.**
- **A step that ended while no server ran, or whose read hung, still gets its log stored or
  closed**, within 30 minutes of the step ending. A step cancelled before it started gets no note.
- **A library can move to a new commit when its file did not change.** A tag or branch that
  moved to a commit with the same library file produced no diff, and the review page showed
  "This version is installed already" with Upgrade turned off, so the installation stayed at the
  old ref and commit. The review page now says the file is the same at the new commit, and
  **Move to this commit** moves the installation there, with the same passkey confirmation and
  trust boxes as an upgrade; the security log names both commits. The fetch answer has
  `content_unchanged`, an installation's `source` names the commit it is at, and an install at
  the ref and commit already installed is refused with 409. Runs inline the same content, and
  Runs created before the move do not change. See
  [Upgrade](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#upgrade).
- **A request no longer fails with "tried to clone a span that already closed".** The server's
  request-id middleware held its tracing span entered across an await, so a request that finished
  on another worker thread left its closed span on the first thread, and a later request there
  could panic. It now enters the span for each poll of the request. Seen as a failed status poll
  in the Argo CD plugin's end-to-end test.
- **A forge token recorded after the end of a try was seen is revoked.** A launch on a replica
  that no longer drives the Run can record a new token in the step's value ledger after another
  replica saw the try end and revoked the tokens then in it. The ledger is no longer taken as
  revoked: the next end of the try or the ledger sweep revokes the new token, once.
- **A value ledger is deleted only at the generation its deletion was decided on.** A generation
  that a late launch writes between the decision and the delete is kept, and the ledger sweep
  revokes its token and deletes the ledger on its next pass.
- **A forge token minted for a launch that is refused before its value ledger is written is
  revoked at once**, for example for a step whose secret is not in its scope. The token was in no
  ledger, so nothing revoked it and it lived until it expired.
- **A server that shuts down cleanly releases the `ledger-sweep` lease.** Before, the next
  replica waited up to the lease's 6 minutes before it swept.
- **The server builds `attempts_started_at_idx` again when an earlier build left it invalid.**
  Migration 0100 builds the index with `CREATE INDEX CONCURRENTLY`. A build that failed or was
  stopped left an invalid index, which the migration's `IF NOT EXISTS` passed over, so the query
  for logs still to be read scanned `attempts`. At start, after the migrations, the server now
  drops an invalid index of that name and builds it again, under a lock, so two replicas that
  start at once do not build it together.

### Tests

- **The kind tier checks logs and the admission policy on a real cluster.** `just test e2e` (run on
  pull requests labelled `ci:full`) now has a step that prints numbered lines and an Environment
  secret across a server SIGKILL, a kubelet restart and a containerd restart: every line is stored
  in order within each part, a gap or repeat sits only after a `source_restarted` note, and the
  secret appears in neither the raw download nor the stream. The admission policy case admits a
  Pod with the four `-v-` names and refuses another Pod's, and 40 short step Pods confirm a clean
  end on a real kubelet. A host test evaluates the chart's admission policy expression against the
  Secret names the executor writes.

### Misc

- **Migration 0099 changed before the release.** A local kind stack whose database ran an
  unreleased build of it needs `just stack down` and `just stack up`.

## [0.6.1] - 2026-11-20

### Bug Fixes

- **The log pane shows "Loading the log…" while a try's log is fetched.** It said "No log output
  was captured for this try." before the first byte arrived. It says so now only after the server
  ended the stream with nothing stored, or the stream stayed open and quiet. When the server
  reports a read failure, the stream cannot be opened, or it does not open within 20 seconds, the
  pane says "Could not load the log: <reason>." with a Retry button. Step, sidecar and service
  logs.

## [0.6.0] - 2026-11-20

### Breaking changes

- **The chart's new step Pod admission policy is cluster-scoped.** On Kubernetes 1.30+ the chart
  now creates a ValidatingAdmissionPolicy and a ValidatingAdmissionPolicyBinding
  (`admissionregistration.k8s.io`), so the account that runs `helm upgrade` needs rights to create
  them. With `scarab.createNamespace: false`, the setting for an installer with rights in the
  release namespace only, the chart leaves the policy out and the install notes say how to turn it
  on (`scarab.admissionPolicy.enabled: true`). Any other install without those rights sets
  `scarab.admissionPolicy.enabled: false`. See [The step Pod admission policy](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#the-step-pod-admission-policy).
- **The render fails when the placement config asks for a host field while the policy is on.** A
  `scarab.placement` baseline or profile with a `hostPath` volume, `hostNetwork`, `hostPID` or
  `hostIPC` would have every step Pod it places refused, so `helm upgrade` now fails naming the
  baseline or profile. Remove the field, or set `scarab.admissionPolicy.enabled: false`. The render
  also fails when step Pods share the release namespace and the server's ServiceAccount is
  `default` (`serviceAccount.create: false` with no name) or another name the policy allows: give
  the server its own ServiceAccount, run step Pods in another namespace, or turn the policy off.
- **Migration 0098 rewrites the `step_runs` table; plan the upgrade window.** It gives every
  existing Step a key, which rewrites the table under a lock that blocks every read and write of
  Steps until it ends: at least 7 µs a Step on Postgres 16 (measured on narrow rows; Steps with
  large stored specs take longer), so 7 s or more for a million Steps. The server runs it before it
  listens, and the chart's new `startupProbe` gives it 30 minutes before liveness counts; raise
  `startupProbe.failureThreshold` for a larger database. With more than one replica and a rolling
  update, the replicas still on the old version wait on the lock, and until the rollout ends an old
  replica takes a key in a step URL for an unknown Step id: a results POST gets a 401, which the
  results sidecar retries, and a read gets a 404 or an empty answer. Once 0098 has run, a 0.5.x
  server refuses to start on the database, so a rollback needs a database backup from before the
  upgrade.
- **`GET /v1/runs/{id}/steps/{step}/results` for a Step the Run does not have answers 404.** It
  answered `200 []`. A script that polls results for a Step name that is not in the Run gets a 404.

### Security

- **The chart installs an admission policy that holds step Pods to their own Secrets and
  ServiceAccount.** A ValidatingAdmissionPolicy refuses a Pod the server's ServiceAccount creates
  in the step namespace when it names a Secret other than its own per-Pod Secrets and the ones
  `scarab.placement` or `scarab.admissionPolicy.allowSecrets` names, runs as a ServiceAccount
  other than `default` and the ones the placement profiles or
  `scarab.admissionPolicy.allowServiceAccounts` name, or asks for `hostPath`, `hostNetwork`,
  `hostPID` or `hostIPC`. It holds both namespace modes, `<release namespace>-steps` and
  `scarab.namespace` set to the release namespace. A step, shared service or debug shell whose Pod
  it refuses fails with the policy's reason. It needs Kubernetes 1.30+; on an older cluster the
  chart skips it and the install notes say so. Set
  `scarab.admissionPolicy.validationActions: [Audit]` to try it first. See
  [The step Pod admission policy](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#the-step-pod-admission-policy).

### Features

- **A library keeps the ref it was fetched at and the commit that ref resolved to.** The library
  list, each library's page, the review page and the upgrade diff show them as
  `v1.2.0 → 1a2b3c4` (the commit alone for a source pinned with `@<commit>`), with the full commit
  in the commit's title and a click to copy it. An upgrade reads from the installed ref and commit
  to the fetched ones, and Upgrade starts from the installed ref. The fetch answer and every
  installation carry `requested_ref` and `resolved_commit`, and the install's passkey
  confirmation and security log name them. An installation records the ones of the fetch it
  installed: fetching the same content again under another ref does not change it. An install
  that names the fetch it reviewed is refused with a 409 when the same content was fetched again
  in between at another ref or commit. Migration
  `0097` adds nullable columns and fills in the commit of every library installed before it; their
  ref stays empty. See
  [The ref and the commit](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#the-ref-and-the-commit).

- **Step keys.** Each Step of a Run has a key: 12 random characters of `a`-`z` and `2`-`7`,
  unique within the Run and kept across retries, reruns and Takes. `GET /v1/runs/{id}` lists it
  as `steps[].key`, and every `/v1/runs/{id}/steps/{step}/…` and `/v1/runs/{id}/gates/{step}/…`
  route takes it as `{step}`. A Step id, percent-encoded, still works. Migration 0098 gives every
  existing Step a key. See
  [Step keys](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#step-keys).

### Bug Fixes

- **Results of a Step whose id holds `/`, `[`, `]`, `,` or `=` are stored.** Every Step of an
  invoked library has such an id (`amd64/image`), and so does a matrix Step (`test[os=linux]`). On
  Kubernetes the results sidecar put the raw id in its URL, the request matched no API route, and
  the server answered it with the web UI and a 200. The sidecar logged "results posted", nothing
  was stored, the Run page said the try published no results, and a later Step that read them got
  an empty value: the build library's push Step refused with an empty `SCARAB_BUILD_DIGEST`. The
  results sidecar now names the Step by its key, and `scarab.resultsApiUrl` should point at the
  in-cluster Service (see
  [A proxy in front of Scarab](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#a-proxy-in-front-of-scarab)).
- **Step requests no longer need a proxy that keeps `%2F`.** The web UI, `scarab rerun` and the
  results sidecar name a Step by its key, so the Run page shows the logs and results of an invoked
  or matrix Step, and reruns it, behind a gateway that decodes `%2F` or redirects to the decoded
  path (Envoy Gateway's default). Only an API caller that sends a Step id holding `/` still needs
  the proxy to keep `%2F`.
- **The results sidecar counts only the server's 202 as delivered.** Any other answer, a 200 or a
  redirect included, is logged with its status and retried. After 6 tries the sidecar's log says
  the results were not delivered and it exits 1 instead of 0, and the Step fails: a Step whose
  results were lost in 0.5.0 without a word now fails. A `scarab.resultsApiUrl` that points at a
  gateway that redirects or rewrites the path is the likely cause.
- **An unknown `/v1/…` path answers a JSON 404.** The web UI answered every request no route
  matched, under `/v1` and for every method, with its page and a 200. Requests under `/v1`,
  `/webhooks` and `/.well-known`, and requests with a method other than GET or HEAD, now get
  `404 {"code":"not_found",…}`. An org named `v1` or `webhooks` loses its deep links: opening or
  reloading `/v1/<repo>` or `/webhooks/<repo>/…` gets the JSON 404 instead of the page. An uptime
  check aimed at a path Scarab never served now gets a 404 too; the chart's probes use `/livez`
  and `/readyz`.
- **The Details link of a commit status opens the Run.** It pointed at `/runs/<id>`, which the web
  UI showed as an empty repository; it is now `/<org>/<repo>/runs/<id>`. Statuses posted by an
  earlier version keep the old link.
- **A long log downloads and loads at the speed of the object store.** The raw download and the
  Run page fetched a log's stored chunks one at a time, and the log tail stored one chunk per
  read of the container log (at most 8 KiB, often one line), so a 19.8 MB log was thousands of
  chunks and downloaded at about 22 KB/s. The tail now stores chunks of up to 256 KiB (or what it
  read in 250 ms), and readers fetch up to 16 chunks ahead, in order. Logs stored by an earlier
  version keep their small chunks.
- **A step's log is stored while the step prints, not minutes after it ended.** The tail wrote
  each small read to the object store and the database in turn, so a step that printed fast
  ended long before its log was stored. With 256 KiB chunks it keeps up.
- **The Run page follows a log that is still being stored.** For a try that had ended while its
  log was still being stored, the page said "No log output was captured for this try", and the
  log stream of a finished Run ended with the part stored so far. The stream now follows the log
  of an ended try until the replica storing it is done (at most 10 minutes past the Run's end),
  sends a `storing` event, and the page says "The log is still being stored." until then, or
  "reload to check again" once the server stops waiting.
- **The log notice gives the log's size and the part shown.** The pane loads at least the last
  2 MiB (from the stored chunk that holds the cut, where it used to start after it), and the
  notice takes the log's size from the server, so it no longer says "Showing the last 1.0 MB of
  17.9 MB" for a 19.8 MB log whose end is still loading. The size grows as more is stored, says
  "so far" while the try runs, and is named in KiB, MiB and GiB, the powers of 1024 it is
  counted in.

## [0.5.0] - 2026-11-20

### Breaking changes

- **Step Pods run in a namespace of their own by default.** With the k8s executor, `rbac.create`
  and `scarab.createNamespace` on (the defaults), an empty `scarab.namespace` now means
  `<release namespace>-steps`, which the chart creates, instead of the release namespace, so
  pipeline code no longer runs beside the server's Secret and ServiceAccount. The executor's Role
  and RoleBinding move with it. Step Pods running during the upgrade stay in the release
  namespace, and the new server does not see or delete them: it marks a step that was still
  running lost, and the step's retry policy decides whether it runs again. Let running Runs finish
  before you upgrade, or set `scarab.namespace` to the release namespace to keep step Pods where
  they are. Creating a Namespace needs cluster-wide rights: an installer with rights only in the
  release namespace sets `scarab.createNamespace: false` (step Pods then run in the release
  namespace) or names an existing namespace in `scarab.namespace`. With `rbac.create: false` an
  empty `scarab.namespace` still means the release namespace, where your own RoleBinding is.
  Recreate in the new namespace what you set up for step Pods (a LimitRange, `imagePullSecrets`,
  the ServiceAccounts placement profiles name). Installs that set `scarab.namespace` already see
  no change. See
  [Where step Pods run](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#where-step-pods-run).
- **A step's `k8s_overlay` may set only an allowed list of Pod fields, and its labels,
  annotations and tolerations only with keys the operator lists** (git-bug 24a3126, 743545c,
  5b6b5cf). The Security entry below says why. An overlay that worked before is refused when it:
  - sets any label, annotation or toleration whose key the operator has not listed. Nothing is
    listed by default, so this covers team or cost labels, `karpenter.sh/do-not-disrupt`,
    `cluster-autoscaler.kubernetes.io/safe-to-evict`, Prometheus or Datadog scrape annotations,
    and tolerations of a GPU or spot pool taint. To allow a key, list it in the placement config
    (the chart value `scarab.placement.author_overlay`), for example
    `author_overlay: { label_keys: [team], annotation_keys: ["karpenter.sh/do-not-disrupt"], toleration_keys: [nvidia.com/gpu] }`.
    Each entry is an exact key, `prefix/*` (a prefix with no `/` of its own), or `*` for every
    key with no prefix. Keys under
    `scarab.io/`, `scarab.dev/` and `app.kubernetes.io/`, the AppArmor and seccomp annotations,
    `node-role.kubernetes.io/` taints and a toleration with an empty or missing key stay refused
    even when listed;
  - sets `spec.affinity` other than `nodeAffinity` (Pod affinity and anti-affinity), or any field
    outside the list, such as `priorityClassName`, `schedulerName`, `serviceAccountName`, a host
    field or a Secret reference;
  - declares a volume with a name the Pod already has, or lists one twice;
  - changes or removes what the placement baseline or a profile set, such as a baseline
    toleration or a profile's variable.

  Move such a setting into a placement profile (under the profile's `k8s`), where these limits do
  not apply, and name the profile in the step's `placement_profiles`. A misspelled key inside
  `author_overlay` stops the server at boot, and a malformed entry fails validation. See
  [Placement & resources](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#placement--resources).
- **An API script that installs or upgrades a library whose source holds a trust grant must
  send `carry_grants`** (git-bug 8c62098). Copy the list from the `carried_grants` field of the
  `POST /v1/orgs/{org}/libraries/fetch` answer into the `POST /v1/orgs/{org}/library-installations`
  body. It names every grant the source holds: forge credentials, the registry credential and the
  build sandbox. Without it the install fails with a 400 that names the grants. A source with no
  grant needs nothing. See
  [Upgrade](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#upgrade).
- **A step with `registry_auth: true` may not set `services:`** (git-bug d2a8efc). Such a Pipeline
  no longer compiles. The Security entry below says why. The build library's push step sets no
  services, so it is unaffected.
- **A service with `run_as_root: true` beside a non-zero `run_as_user` no longer compiles**
  (git-bug 1a20384). The two fields contradict each other. `run_as_root: true` beside
  `run_as_user: 0` is the same request and compiles, and runs as root. A step's `security` block
  follows the same rule. See
  [Steps: the step shape](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#steps-the-step-shape).
- **Install the build library again from the release commit, and change the ECR login init
  container** (git-bug 1a20384, 23a29ab, 410eed3). The build library's fixes below reach an org
  only through a new install. Its push step now runs as uid 1000. The ECR guide's `ecr-login` init container runs as uid 1000 too, with `HOME=/tmp`, so the
  push step can read the `0600` login file it writes: an operator who set up that profile from the
  guide changes its `securityContext` and env to match. See
  [The placement profile](https://thulasi-ram.github.io/scarab-ci/usage/pushing-to-ecr/#2-the-placement-profile).
- **A `run.create` Policy counts and sees services** (git-bug 328010f). While an enforced
  `run.create` Policy applies, Run creation refuses a Run with more than 8 sidecar services on one
  step, more than 32 shared services, or more than 4 grants on one service. A sidecar that runs as
  root (`run_as_root: true` or `run_as_user: 0`) now puts `run-as-root` in its step's `grants`, so
  a Policy that denies root now refuses Runs it passed before. To keep such a Run, allow the
  service in the Policy by reading the step's `services`, or run the service as a non-root uid. The
  Security entry below says why. See
  [Policies: rules over services](https://thulasi-ram.github.io/scarab-ci/usage/policies/#rules-over-services).

### Security

- **A step's `k8s_overlay` may set only an allowed list of Pod fields** (git-bug 24a3126, 743545c,
  5b6b5cf). An Environment that permits raw overlays let a pipeline author set any Pod field: mount
  the server's Secret or another Run's per-Pod Secret by its name, run as the server's
  ServiceAccount, use the host's network, PID namespace or filesystem, run privileged, or point
  the sidecars at another address with `hostAliases` or `dnsConfig`. The overlay may now set
  `nodeSelector`, `affinity.nodeAffinity` and `topologySpreadConstraints`, emptyDir volumes with
  new names, and the step container's `resources`, literal `env` and mounts of those volumes.
  Labels, annotations and tolerations are allowed only for keys the operator lists: a label can
  put the Pod in a NetworkPolicy's or a Service's selector, an annotation can ask an admission
  webhook for an IAM role or a Vault or mesh sidecar, and a toleration lets the Pod onto a node
  pool kept for something else. Pod anti-affinity is refused because a required one keeps other
  Pods off the step Pod's nodes. A volume the overlay re-declared by name merged into the
  operator's volume of that name, so the step container could mount it; that is refused now. The
  overlay may add to the Pod but not change what the placement baseline or a profile set. Run
  creation, `scarab validate --server` and the policy test refuse with the field's path, and the
  executor checks again before it creates the Pod. Breaking changes above says what to do. See
  [Placement & resources](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#placement--resources).
- **The workspace drain checks its fence against the Pod's name** (git-bug 13f0c09). The drain read
  the `{run, step, attempt}` fence from the step container's env, which the Pod spec sets. It now
  refuses a fence whose step Pod name is not the Pod's own name.
- **Installing or upgrading a library names the grants its source's trust gives the new
  commit** (git-bug 8c62098). Trust stays keyed by source, so a new commit from a source trusted
  with forge credentials, the registry credential or the build sandbox gets them too, and until
  now nothing said so. The fetch answer lists every grant the source holds in `carried_grants`;
  the install must name the same grants in `carry_grants`, or it fails with a 400 that names them.
  The review page shows one box per grant, and **Upgrade** stays off until each is ticked. The
  step-up summary and the `op.completed` security event name the grants carried. The install
  checks `carry_grants` before it asks for step-up, so a missing or wrong list is a 400 and uses
  no confirmation, and it compares the grants again inside the install's own transaction, so a
  grant another admin gives the source in between is refused with a 400 that names the grants
  (git-bug 9c54b46). See
  [Upgrade](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#upgrade).
- **The build library's push step pushes only the image the build step built** (git-bug 23a29ab).
  It receives BuildKit's `containerimage.digest` from the build step's `image` result and refuses
  (exit 2) a layout whose `index.json` does not name exactly that one manifest, or whose manifest
  file does not hash to its digest: a stale, half-written or swapped layout. It also refuses an
  `index.json` that holds a backslash, a byte that is not printable ASCII, or the word `digest` in
  any case other than once, since crane's JSON decoder reads keys in any case and decodes escapes.
  crane then pushes from an `index.json` the step writes, naming only the checked manifest. If
  crane reports another digest, the step fails and writes no `push` result. The check does not
  cover a build that rewrites its own `image` result as well as the layout. When the server
  collects no step results, the step checks only the layout, pushes, and logs a line starting
  `scarab: warning:`. See
  [Building an image](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#building-an-image).
- **The build library refuses a `build_args` name that starts with `BUILDKIT_`** (git-bug 410eed3).
  BuildKit reads those arguments as settings, and `BUILDKIT_SYNTAX` names a frontend image that it
  pulls and runs, so a `build_args` value built from text someone else writes could pick the
  image. The input's `validate:` rule and the build step's script both refuse such a name in any
  case, and the script also refuses a control character in a value. The docs now say not to build
  `build_args` from free text.
- **A step with `registry_auth: true` may not set `services:`** (git-bug d2a8efc). A sidecar
  service runs in the step's Pod, under the ServiceAccount a `registry_auth_only` placement profile
  gives that Pod, and the IRSA skip list in the ECR guide does not name `service-N` containers.
- **The build library's push step no longer runs as root** (git-bug 1a20384). It sets
  `security: { run_as_user: 1000 }` in place of `run_as_root: true`, so its Policy input no longer
  lists the `run-as-root` grant and an org Policy that denies root no longer refuses every build.
- **A `run.create` Policy sees services** (git-bug 328010f). The Policy input listed no service,
  so a Policy that denies root saw a step's `run_as_root` but not a service's, and a Run with a
  root service passed it. Each step now lists its sidecar services under `services`, and the Run's
  shared services are in a top-level `services`, each with `image`, `digest`, `run_as_root`,
  `run_as_user` and `grants`. `run_as_root` is true for `run_as_root: true` and for
  `run_as_user: 0`. A root sidecar also puts `run-as-root` in its step's `grants`, so a Policy
  written against step grants now refuses it; a root shared service belongs to no step, and a rule
  reads the top-level `services` to see it. Decisions recorded before have neither field.
  Breaking changes above gives the limits on services and what an existing Policy now refuses. See
  [Policies: rules over services](https://thulasi-ram.github.io/scarab-ci/usage/policies/#rules-over-services).
- **A server replica that reads a step's log redacts that step's secrets, even when another
  replica launched the step.** The values to redact were held only in the memory of the replica
  that launched the step. When another replica took over the log tail, as on any server roll while
  a step ran, it stored the step's output as printed, secrets included. A replica now reads the
  step's secret values, its registry credential, its forge token and its OIDC token from the step
  Pod's Secrets before it claims the log tail and before it reads any of the log, and keeps them
  until the log is complete. A read that fails is never taken as "no secrets": while the step runs,
  the replica reads no log and tries again later. When the step has ended and the values were
  never read (its Secrets are deleted when its try ends, or the read keeps failing), the replica
  stores this line in place of the rest of the log, and logs a warning:
  `scarab: the rest of this log was not stored: its secret values could not be read`. Before, a
  replica that a dead replica's tail lease kept away until the step ended read no values and
  stored the end of the log as printed. When a re-read differs from the stored part where that
  part holds `***`, the replica no longer stops storing the log with no line saying so: for a
  running step it stores
  `scarab: earlier output could not be checked; showing output from now on` and then the output
  printed from then on (what was printed in between is not stored), and for a step that has
  ended it stores
  `scarab: the rest of this log was withheld: it could not be checked against the part already stored`.
  See
  [Run logs](https://thulasi-ram.github.io/scarab-ci/usage/run-logs/#how-the-server-stores-a-log).
- **A step's secret values redact only that step's log.** The values were one list per server,
  for every step of every Run, kept for the life of the process. Every log byte was checked
  against the whole list, under one lock, so with a forge token or an OIDC token added at each
  launch, storing logs slowed down the longer the server ran (about 3 s per MiB of log at 1,000
  values). A step's output equal to another step's secret, in any org, was stored as `***`, which
  showed which values existed, and made a later re-read of that log differ where nothing had been
  redacted for it. Each value is now kept for the log of the step try (or sidecar) it was mounted
  into, and dropped once that log is complete and the step try has ended. A failure message or
  infra condition, which the server reads by Pod and not by log, is redacted with the values of
  every step try that has not ended yet, its own included.
- **Forge and identity tokens mounted into a step are redacted from its log.** Only the step's
  own secrets and its registry credential were redacted. The forge token a step gets with
  `forge_token:` and the OIDC token Scarab issues to a deploy step were stored as printed. Both
  are now registered for the step's log when the step launches, again with the new token on each
  relaunch, and a replica that did not launch the step reads both back from the step Pod's
  Secrets.

### Features

- **Step logs stream in parts and can resume** (git-bug 9ac20c0). The server read a try's whole
  log into memory and sent it as one event. It now reads the log chunk by chunk and sends events of
  about 256 KiB (at most 256 KiB plus two stored chunks), each with an id
  (`<attempt_id>:<next_seq>`). `?from=<id>` or the `Last-Event-ID` header continues after that
  event. An id the server cannot read, or one that names a try outside the stream, is a 400; an
  id with a position past the end of a try's log is not refused, and the stream sends nothing for
  that try until its log passes that position. `?tail=<bytes>` (at most 64 MiB) sends only the end
  of the log, from a chunk boundary, after an `event: truncated` that gives the skipped and total
  bytes. A character a stored chunk cuts in two is sent whole; on a running try the server waits
  at most two polls for the rest, then sends U+FFFD, and on an ended try it sends U+FFFD at once.
  A stream ends one poll after the Run finishes, or when the Run no longer exists.
  `GET /v1/runs/{id}/steps/{step}/logs/raw?attempt=<id>` downloads one try's whole log as plain
  text, with `X-Content-Type-Options: nosniff`. See
  [Run logs](https://thulasi-ram.github.io/scarab-ci/usage/run-logs/).
- **The Run page loads the last 2 MB of a log** (git-bug 9ac20c0). A notice above a longer log
  says how much shows and where line numbers start ("Showing the last 2.0 MB of 14.3 MB. Line
  numbers start here."). **Load full log** reads up to 8 MB, and for a step **Download** saves the
  try's whole log; a shared service's pane has both. The pane keeps
  at most 8 MB, dropping lines from the top; a log with no line breaks is cut to its last 8 MB
  once it passes 16 MB. A dropped connection reconnects after 1 s, 2 s, 4 s and so on up to 30 s
  and continues after the last part received. After three reopens in a row that fail before the
  stream opens, the pane asks the server about its position; only when the server answers 400 does
  it load the end of the log again, as much as it showed. See
  [On the Run page](https://thulasi-ram.github.io/scarab-ci/usage/run-logs/#on-the-run-page).
- **A shared service's log can be downloaded** (git-bug 5ca1f3c).
  `GET /v1/runs/{id}/services/{service}/logs/raw?take=<n>` sends the service's whole log in Take
  `n` (default the current Take) as `text/plain`, named `scarab-<run>-<service>-take<n>.log`, with
  each character of the run id and service name outside `[A-Za-z0-9_-]` replaced by `_`. The
  service pane has **Download** in its header and in the notice above a longer log. See
  [Run logs](https://thulasi-ram.github.io/scarab-ci/usage/run-logs/).
- **A library joins per-architecture images into one multi-architecture image** (git-bug
  794b3ac). `lib:thulasi-ram::scarab-ci::image-index`, the file `libs/image-index.yaml`, runs
  `crane index append` over images named by digest (one `<repository>@sha256:<digest>` per line)
  and pushes the index under a tag. Write each line as
  `${{ has(outputs.amd64.push) ? outputs.amd64.push.ref : '' }}`: on a fork pull request the
  builds push nothing, the lines are empty, and the index step pushes nothing and exits 0; anywhere
  else an empty `images` fails the step. It refuses an image named by tag, an image that is itself
  an index, two images of one platform, and a manifest or config that hides a key from its checks
  with a `\u` escape or a second spelling in other case. It reports the index's digest as
  `outputs.<id>.index.digest`. Its one step holds the registry credential, so an org admin
  installs it and trusts its source with the registry credential. See
  [Joining images into one multi-architecture image](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#joining-images-into-one-multi-architecture-image).
- **A step can name the uid it runs as** (git-bug 1a20384). `security: { run_as_user: <uid> }`
  sets the step container's `runAsUser`, for an image whose `USER` is root or a name. A non-zero
  uid is self-service and still runs with `runAsNonRoot`. `run_as_user: 0` is the same request as
  `run_as_root: true`: it needs the same grant, and Policies see it as `run-as-root` in the step's
  `grants`. `run_as_root: true` beside `run_as_user: 0` compiles; beside a non-zero uid it does
  not. `run_as_user` cannot be combined with `sandbox`. The `run.create` Policy input has a new
  `step.run_as_user` field. A service's `run_as_user: 0` is likewise its `run_as_root` request: it
  ran under `runAsNonRoot` and the kubelet refused it. Running a service as root stays
  self-service; the Security entry on services in the Policy input says what a Policy sees. See
  [Steps: the step shape](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#steps-the-step-shape).

### Enhancement

- **A live log reads only the new part of its chunk index** (git-bug 209ef14). Each poll of a
  running step's log, every 500 ms, read the step's whole list of stored chunks; it now reads the
  rows from its position on. Storing a chunk reads only the newest row.
- **The server warns at startup when `default_resources` sets one axis only** (git-bug 9cfeac4).
  With `cpu_millis` and no `memory_mib`, a step container that sets no memory has no
  `requests.memory` or `limits.memory`; with `memory_mib` and no `cpu_millis`, it has no
  `requests.cpu`. The warning names the missing field and says that a cluster that requires
  resource limits (a Kyverno require-resource-limits policy, for example) will refuse those step
  Pods. See
  [Placement & resources](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#placement--resources).

### Bug Fixes

- **`scarab logs` no longer adds a line break between stored chunks** (git-bug 9ac20c0). It
  printed each `data:` line with a newline after it, so a line split across two stored chunks came
  out as two lines. It now joins an event's lines as Server-Sent Events defines and prints the
  text with nothing added. When the reader of its output goes away (`scarab logs <run> | head`),
  it stops and exits 0; it used to keep its connection open until the Run ended.
- **A `\r\n` in a step's output shows as one line break**, on the Run page and in `scarab logs`.
  It showed as two, with a blank line between. A lone `\r` still shows as a line break. The raw
  download keeps the bytes the step wrote.
- **A short step keeps its logs** (git-bug 1dee2f3). The log tail read a step only while it was
  running, and waited 3 seconds after a read that found the Pod not ready. A step that started and
  finished inside that wait had no logs stored. The server now reads the log of each step, retried
  attempt and service that stops running before its log was read in full, and stores what follows
  the part already stored, as the next entry describes. That first read does not wait out the 3
  second backoff, so a viewer whose stream ended sees the output on the next load.
- **Log output printed after a log tail broke part way is stored** (git-bug de2bd9b). Before, it
  was lost if the step ended before the tail retried, and a retry while the step ran stored the
  earlier lines twice. On a retry, and on the read after a step stops, the server now reads the
  container's log from the start, redacts it as one stream, and compares it with the part already
  stored. When the log begins with exactly that part, the server stores only what follows it.
  When it does not (the kubelet rotated the log file, the container restarted, or the step's
  secret values changed), the server
  stores the line `scarab: the log source restarted; earlier lines may repeat` and then the whole
  log it read, so no output is dropped and earlier lines may show twice. A secret value split
  between two reads of the log, or between two chunks of one read, is redacted; each chunk used to
  be redacted on its own. Each retry reads the stored part back from object storage to compare it.
  Output the kubelet rotated away before the server read it is gone: the marker line says the
  source restarted, and does not recover those lines. Two replicas that tail one step at the same
  time can still store one replica's chunk under the other's index entry. See
  [Run logs](https://thulasi-ram.github.io/scarab-ci/usage/run-logs/#how-the-server-stores-a-log).
- **A step whose Pod is refused at launch fails as a configuration error.** A refusal of the
  step's `k8s_overlay` when the Pod is built (a clash with the placement baseline or a profile, or
  a Run stored before a field left the list) was retried until the Run was dead-lettered. The step
  now fails with the reason.
- **A step whose `k8s_overlay` makes a Pod Kubernetes refuses fails as a configuration error.**
  An overlay that keeps to the allowed fields can still be invalid to Kubernetes: a
  `nodeSelector` value that is not a label value, a memory request above the limit, a volume name
  in capitals. The launch error was retried for 5 minutes and then the Run was dead-lettered. When
  Kubernetes refuses the Pod as invalid (400 or 422), Scarab now creates the Pod without the
  overlay as a dry run; if that passes, the step fails with Kubernetes' message. If it fails too,
  the operator's baseline or a profile is at fault and the launch error stays. A quota or
  permission refusal (403) stays a launch error. See
  [Placement & resources](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#placement--resources).
- **Run creation refuses a placement profile the operator's config does not allow.**
  With the k8s executor, a step that names a profile the registry does not hold, or a
  `registry_auth_only` profile without `registry_auth`, was created and then failed at every
  launch until the Run was dead-lettered. Run creation now refuses it, naming the step and the
  profile, and for an unknown name the profiles the registry holds. `scarab validate --server` and
  the policy test report the same refusal. A `registry_auth_only` profile on a step without
  `registry_auth` that still reaches launch fails the step as a configuration error. A profile
  removed after the Run was created stays a launch error, which a retry covers during a server
  roll. See
  [Placement & resources](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#placement--resources).
- **The Run page's Activity tab words every step status change** (git-bug d390aa3). A change with
  no try read `running → ready`; it now reads "queued", "waiting to retry", "reset to run again",
  "needed again", "skipped because build failed", "skipped by its when: condition" or "passed
  without a try: inputs unchanged".
- **The Run page reads the status names the server's events carry** (git-bug d390aa3). The server
  sends `Running`, `Succeeded`; the page matched only lowercase names. Activity rows for a status
  change all had the plain info mark, a finished try and its step's move to passed or failed took
  two rows, and an earlier version could show a re-armed step that had passed or failed as not
  run.
- **Plugin probe Pods have cpu and memory requests and a memory limit** (git-bug dac7953). A
  namespace that requires resources refused the probe Pods, so the plugin namespace check failed.
  They now carry the same resources as a Plugin Pod's self-check.
- **`scarab validate --server` reports what the server checked** (git-bug 1aa19e6). It printed
  "`lib:` ... its Steps were not checked (use --server to check them)" although the server had
  compiled those libraries, and counted the Run's Steps from the offline check, where each
  library is one Step. With `--server` it now prints the Run's step count as the server compiled
  it and the installed libraries it resolved. The policy test API returns these as `pipeline`. The
  approval gates and warnings from the local check now print before the request, so they show
  when the server cannot be reached or refuses the token.
- **The policy test refuses what Run creation refuses** (git-bug 1aa19e6). A Run of a Pipeline
  whose library Step asks for the build sandbox, a forge token or a yanked version without the
  trust Run creation requires, or whose Step's `k8s_overlay` or privilege request Run creation
  refuses, got `outcome: allow` from `scarab validate --server` and was refused when created. The
  policy test now runs the same checks as Run creation and answers a deny with Run creation's
  reason, also returned as `refusal`. A server with no forge configured leaves the forge token
  check out of the answer and logs that it did. For a manual, api or cron trigger the policy test
  renders a step's image with the Pipeline's input defaults, as Run creation does, so an image
  such as `reg/x@${{ inputs.digest }}` is checked against a privilege grant by its digest.

## [0.4.4] - 2026-11-19

### Enhancement

- **The Run page has run tabs, try tabs and a List view.** Under the summary line, run tabs
  switch between Pipeline, Artifacts and Activity. Artifacts lists the run's files grouped by the
  step that published them, the copy of record by default and earlier copies on request.
  Activity hides the Pod condition events that have cleared and says how many it hides; a
  condition holding a step back stays in view. A step with two or more tries gets a row of try
  tabs, newest first, each with its outcome, duration and why it ran. The graph is drawn at full
  size with nodes wide enough for their names, and a List view shows the steps in dependency
  order with each step's tries under it. The URL keeps the selected step, version, try and tabs.
- **Each attempt in the run detail API has `finished_at`.** `GET /v1/runs/{id}` gives every
  attempt in a step's `attempt_list` the time it finished, read from the run's event log, or null
  while it runs.

### Bug Fixes

- **A Pod waiting for a node is one infra condition while only the node counts change.** The
  scheduler's message changes as nodes come and go (`0/12 nodes` to `0/14 nodes`), and each change
  closed the condition and opened a new one: one run with 7 steps logged 38 condition events.
  Messages that differ only in their numbers are now the same condition.
- **A Pod that is starting no longer shows an old scheduling message.** Once a Pod has a node,
  `PodInitializing` no longer borrows the message of an earlier `FailedScheduling` event.
- **A dead-lettered run counts as finished on the Run page.** The page stopped polling it, and no
  longer shows it as live or offers Cancel run.
- **A manual Retry is no longer labelled an automatic retry.** The try says who retried it.
- **A try with no log output shows why it failed.** The Logs tab said "waiting for output…" for
  good, and the failure reason under it never appeared.
- **A step whose Pod waits for a new node keeps its logs.** While a Pod has no node (a cluster
  autoscaler such as Karpenter is adding one), Kubernetes answers a log request for it with an
  empty response instead of an error. The server took that as the step's complete log and never
  read the step again, so every line the step printed once the node joined was lost, with nothing
  logged. Since 0.4.1 lets such a Pod wait for a node instead of failing, this hit most steps on
  autoscaled clusters. The server now waits until the Pod has a node, and a log read that ends
  with no output is tried again. The server log has an INFO line when a step's output starts
  arriving and when its log read ends, with the byte count.
- **Scarab's own helper containers set resource requests and a memory limit** (git-bug e8b5967).
  The results sidecar and the workspace fetch and drain helpers had none, so a cluster that
  enforces a require-resource-limits policy (Kyverno, for one) refused or held the Pod. The
  results sidecar now requests 10m CPU and 16Mi memory with a 64Mi limit. With
  `workspace.helperCpuMillis` and `workspace.helperMemoryMib` unset, the workspace helpers request
  50m CPU and 64Mi memory with a 4096Mi memory limit and no CPU limit; a value you set still
  applies as both request and limit. A workspace step Pod's request grows by up to 60m CPU and
  80Mi memory, from the two sidecars; the fetch init container does not raise it.

  The other containers are not covered by this fix. The step container and the shell of a debug
  Pod pass such a policy only when the step's `resources` or the placement config's
  `default_resources` set both `cpu_millis` and `memory_mib` (each set axis becomes both the
  request and the limit). Service containers carry no resources, since a service has no
  `resources` setting yet, so such a policy still refuses a step with services.

  The 4096Mi default limit on the workspace helpers has two effects in a namespace that limits
  memory. A LimitRange with a maximum container memory below 4Gi, or a maximum limit-to-request
  ratio below 64, refuses the Pod. A ResourceQuota on `limits.memory` counts the 4096Mi against
  the quota. Set `workspace.helperMemoryMib` to a lower value to fit either.

## [0.4.3] - 2026-11-17

### Enhancement

- **`/livez` fails for any replica whose scheduler loop is stuck, not only the lease holder.** A
  replica waiting to take over the scheduler lease that is itself stuck could not take over, and
  nothing restarted it. A database that stops answering now restarts every replica, not only the
  lease holder. See
  [Health checks](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#health-checks).
- **The scheduler loop's limits are settings.** `SCARAB_DRIVER_STALL_CUTOFF_SECS` (chart
  `scarab.driverStallCutoffSecs`, default 60, from 30 to 3600) stops a pass with no progress, and
  `SCARAB_DRIVER_STALLED_AFTER_SECS` (chart `scarab.driverStalledAfterSecs`, default 90, at least
  the cutoff plus 30) fails `/livez`. Raise them when one call on your install takes longer than a
  minute.
- **The chart warns when the liveness probe checks `/healthz`.** A values file that sets the whole
  `livenessProbe.httpGet` with the path `/healthz` does not get the stuck-loop check; `helm install`
  and `helm upgrade` now say so.

### Bug Fixes

- **A Kubernetes or forge request that never answers now fails instead of hanging.** The
  Kubernetes client had no read timeout and the GitHub and Forgejo clients had no timeout, so one
  request whose response never arrived could hold the scheduler loop (git-bug afa9f07). Kubernetes
  requests now fail after 30 seconds without response bytes; log tails and `exec` sessions are not
  limited. Forge requests and sign-in requests to the login provider fail after 30 seconds in all.

## [0.4.2] - 2026-11-17

### Bug Fixes

- **A stuck scheduler loop no longer leaves Runs pending with nothing logged.** The loop that
  starts Runs renews the scheduler lease as it works, and nothing bounded a pass: one call that
  never returned stopped it, the lease stopped renewing, and on a single replica Runs stayed
  pending until the Pod was restarted. A pass that makes no progress for 60 seconds is now stopped
  with a warning that names the part it was in; a slow pass that keeps making progress is not. A
  new liveness endpoint, `/livez`, answers 503 when this replica holds the scheduler lease and has
  made no progress for 90 seconds, and the chart's liveness probe now uses it, so Kubernetes
  restarts the Pod. If the loop stops (a panic), the server shuts down as on SIGTERM and exits
  with status 1. A `values.yaml` that sets the whole `livenessProbe.httpGet` keeps its own path;
  point it at `/livez` to get the check
  ([Health checks](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#health-checks)).

## [0.4.1] - 2026-11-13

### Breaking changes

- **A library has one name, and Pipelines write `lib:<name>`.** The name is parts of letters,
  digits, `.`, `_` and `-`, joined by `::` or `/` (`acme::scarab-libs::deploy-helm`,
  `platform/helm`, `deploy`), at most 255 bytes and case-sensitive. A name with a space, a quote,
  a lone `:`, `$`, `{` or other punctuation is refused by the compiler and by the API. A fetch
  with no `name` installs under `<owner>::<repo>::<file stem>`, so Scarab's own libraries now
  default to `lib:thulasi-ram::scarab-ci::clone` and `lib:thulasi-ram::scarab-ci::build`.
  Migration `0096` joins every stored publisher and name into `<publisher>/<name>`: an
  installation made before it keeps its id, its version and its yanks, and Pipelines that invoke
  `lib:thulasi-ram/clone` keep compiling. Docs, the compiler's hints and this repository's own
  Pipelines now name `lib:thulasi-ram::scarab-ci::clone`: an org that installed the clone or build
  library before keeps it as `thulasi-ram/clone` or `thulasi-ram/build`, so either keep invoking
  that name or install it again under the new one. A server from before `0096` cannot run against the
  migrated database, so stop every old replica before the new one starts (the chart's default
  `Recreate` strategy does this). The API changes with it:
  - `POST /v1/orgs/{org}/libraries/fetch` no longer takes `publisher`; `name`, when given, is
    the whole name.
  - `PUT /v1/orgs/{org}/libraries/{publisher}/{name}` is replaced by
    `POST /v1/orgs/{org}/library-installations` with `{ "name", "content_hash", "replaces" }`.
  - `DELETE /v1/orgs/{org}/libraries/{publisher}/{name}` is replaced by
    `DELETE /v1/orgs/{org}/library-installations/{id}`, the id `GET` on that path takes.
  - Fetched versions, installations and yanks drop `publisher`; `name` is the whole name.

  See [Install a library](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#install-a-library).

### Features

- **Install a library at a branch, tag or commit.** **Install library** asks for the file and a
  ref in two fields, and the fetch API takes `ref` beside a `source` with no `@<commit>`. Scarab
  reads the file at the commit the ref points to when you fetch, and the review page and the
  installation name that commit, so a branch that moves later changes nothing until you upgrade
  ([Install a library](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#install-a-library)).

### Bug Fixes

- **A connection that cannot reach a library's repository no longer blocks its install.**
  Choosing an org connection that cannot read the repository (a GitHub App not installed on it
  answers 404) refused the install, even for a public file that installs with no connection.
  Scarab now reads the file, or looks up the ref, without credentials when the connection fails;
  when that fails too, the error gives both reasons and says to give the connection access.
  Plugin manifests are read the same way.

- **A step waiting for an autoscaled node no longer fails at once.** On a cluster whose node
  pools scale to zero (Karpenter, Cluster Autoscaler), the scheduler calls every Pod that needs a
  new node Unschedulable until the node joins; Scarab failed such a step within a second, and
  its retries were spent before the autoscaler launched a node. A step Pod now waits up to
  `SCARAB_UNSCHEDULABLE_GRACE_SECS` (chart value `scarab.unschedulableGraceSecs`, default 300)
  for a node, then fails with the scheduler's message as before; each automatic retry waits again.
  The most it accepts is 1800, the engine's own limit for an attempt that has not started. `0`
  keeps the old behaviour.

## [0.4.0] - 2026-11-12

### Breaking changes

- **The `build` step kind is removed; build images with `lib:thulasi-ram/build`.** A Pipeline with
  a `build:` step fails to compile with one error for that step, which holds the replacement
  written from the step's own fields, ready to paste:

  ```yaml
  # before (0.3.2)
  - id: image
    needs: [checkout]
    build: { image: "ghcr.io/acme/app:${{ event.sha }}", context: ., dockerfile: Dockerfile, push: true }

  # after
  - id: image
    needs: [checkout]
    invoke: lib:thulasi-ram/build
    with: { image: "ghcr.io/acme/app:${{ event.sha }}", context: ., dockerfile: Dockerfile, push: true }
  ```

  `needs`, `when`, `timeout`, `resources` and `placement_profiles` carry over as written;
  `env`, `retry` and `artifacts` on the old step are not accepted on an invoke step and are left
  out. `${{ outputs.image.image }}` reads the same result. A step of a Run created before the
  upgrade that was stored as a `build` step fails when it would launch (and again on each retry
  or rerun) with the same replacement, and the Run has to be started again. A Pipeline that fails
  its check, such as one with an old `build:` step, creates no Run and posts no commit status yet:
  look for the error in the repository's Runs list in Scarab. Nothing is installed in an org by
  default, so after upgrading the server, an org admin:
  1. installs `github.com/thulasi-ram/scarab-ci//libs/build.yaml` at a release commit as
     `thulasi-ram/build`;
  2. trusts that source for the build sandbox (the new `build_sandbox` grant of
     `POST /v1/orgs/{org}/library-trust`, or **Build sandbox › Trust source** on the library's
     page);
  3. if the builds push, trusts it with the registry credential too (`registry_credential`), so
     its push step receives the `REGISTRY_AUTH` secret; to push to the forge's own registry with
     no `REGISTRY_AUTH` secret, also with forge credentials;
  4. if a placement profile grants push access, has the operator mark it
     `registry_auth_only: true` (see Features) and routes it to the push step with
     `placement_profiles: { push: [<profile>] }`.

  The push step runs as root (crane's image has no other user), so an org Policy that denies
  run-as-root refuses every Run that builds an image.

  Between the upgrade and step 2, every Run that builds an image is refused. Upgrade every
  server replica first: a server from before this release ignores a request's `grant` and
  records trust with forge credentials, so never send `grant` to one.

  The library has two steps. `image/image` runs rootless BuildKit in the build sandbox and holds
  no registry credential: BuildKit runs without its process sandbox, so a Dockerfile `RUN` could
  read anything that step holds. An image to push is written as an OCI image layout into the
  workspace, and `image/push` pushes it with crane (pinned by digest) after refusing a layout
  that holds a symlink or leads outside the workspace. Pushing a large image therefore costs a
  transfer through the Data Depot. The push step removes the layout from its own workspace, but a
  step that reads `outputs.<id>.image` needs the build step too and receives the layout through
  its workspace; deploy by `outputs.<id>.push.digest`, or limit the step with `inputs:`. The
  build step cannot pull private base images. There is no
  registry layer cache (`cache: registry`, `cache_ref`), because writing one needs the push
  credential in the build step; the S3 cache stays, reached with the build Pod's own identity.
  `${{ outputs.image.push }}` gives the pushed reference and digest. `containerimage.digest` in
  `outputs.image.image` is the same digest unless the Dockerfile changed the layout after
  BuildKit wrote it, so deploy by `push.digest`. A fork pull request's build, or a Run locked out
  of its Environment's secrets, neither pushes nor exports its layer cache. The library adds build arguments, an S3 layer
  cache, images pinned by digest and templates in `image` (such as `${{ event.sha }}`)
  ([Building an image](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#building-an-image),
  [Install Scarab's build library](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#install-scarabs-build-library),
  [ADR-0018](https://github.com/thulasi-ram/scarab-ci/blob/main/docs/adr/0018-image-building.md)).
- **`DELETE /v1/orgs/{org}/library-trust` with no `grant` removes every grant of the source.**
  Name `grant` to remove one. Trust with forge credentials stays where it was; migration 0095
  only adds a table (`library_source_grants`) for the new grants, so a server from before it
  keeps working against the migrated database.

### Security

- **A fork pull request's Run cannot use `k8s_overlay`.** The fork's author writes the Pipeline,
  because it is read at the pull request's head. A Run from a fork pull request, or a Run locked
  out of its Environment's secrets, is now refused at launch when a step sets `k8s_overlay`.
- **The build library keeps its paths inside the workspace.** `context` and `dockerfile` may not
  be absolute, hold `..`, a `,` or a control character, and `image` must be an image reference.
  Before running buildctl, the build step resolves the context and the Dockerfile and refuses
  (exit 2) one that leads outside `/workspace`, such as through a symlink committed to the
  repository.

### Features

- **Trigger on changed files.** A `when:` can read `event.changed_paths`, the files a push or
  pull request changed, so `when: event.changed_paths.exists(p, p.startsWith("docker/ci-tools/"))`
  starts a Run only when that directory changed. A push's list comes from its webhook payload, a
  pull request's from the forge's files API, asked only when a Pipeline reads the field. When the
  list is unknown (a new branch, a forced push, a cut commit list, 3,000 or more files, an API
  error or a request slower than 5 seconds), a `when:` that needs it counts as true and the Run's
  activity log says why. Forced-push detection is GitHub only: Forgejo's push payload does not
  say whether a push was forced. See
  [Changed files](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#changed-files-eventchanged_paths).

- **An invoke step can route placement profiles per library step.** `placement_profiles` on an
  invoke step may be a map from a step id of the library to that step's list, such as
  `{ image: [arm64-builders], push: [ecr-push] }`. The list form applies to every inlined step as
  before. A key that is not a step of the library, or names a step with `forge_token` or with its
  own profiles, is a compile error, and a `forge_token` step still gets no profile from its
  invoke. Compiled steps, and Policies, see one list per step
  ([Reuse: `invoke`](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#reuse-invoke)).
- **A placement profile that grants push access can be kept to the step that pushes.** An
  operator marks it `registry_auth_only: true` in the placement config. Scarab applies it only to
  a step with `registry_auth` that the launch found eligible for a registry credential (a step
  from a library the org trusts with the registry credential, in a Run that is not a fork pull
  request or locked out), whether or not a credential resolves. Named on any other step, the build library's build step included, it fails that step's
  launch; on a fork's push step it is left out. A marked profile cannot be `default`; the server
  refuses that config at boot
  ([Placement & resources](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#placement--resources)).

### Enhancement

- **A waiting GitHub connection names its account.** Under **Waiting for an organization** each
  installation shows its account login, account id and installation id, so two waiting
  connections with nothing enabled no longer look the same, and **Create a new organization from
  it** starts with the login filled in. The server stores the login from each `installation`
  delivery (migration `0094`, filled on the next delivery) and `ConnectionDto` gains
  `account_login`, `account_id` and `installation_id`.

### Bug Fixes

- **Placement layers merge like Kubernetes instead of replacing lists.** The baseline, placement
  profiles and `k8s_overlay` were merged by replacing each list, so a profile's `tolerations`
  dropped the baseline's, and a `k8s_overlay` that named the step container replaced every
  container in the Pod. Containers, initContainers and volumes now merge by name, `env` by name,
  `volumeMounts` by mount path, and tolerations are combined. A layer that names Scarab's own
  containers (other than `step`) or volumes, mounts over the step's paths, changes the step's
  built env or uses a `$` patch directive is refused at launch, with the layer it came from
  ([Placement & resources](https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/#placement--resources)).

- **The Dashboard lines up with every other page.** Its heading is now `/ › Dashboard`, on the same
  line as the other pages' breadcrumbs, and its first block ("Waiting on you" or the "All clear"
  card, which sat 8px under the heading) starts at the height of a repository's tabs. So does the
  organization page's Overview, which sat 14px lower.

### Documentation

- **Push to Amazon ECR from EKS.** A new page sets up an IAM role on its own ServiceAccount (IRSA),
  used only by the build library's push step through a `registry_auth_only` placement profile.
  The flow has not been run on EKS yet
  ([Pushing images to Amazon ECR](https://thulasi-ram.github.io/scarab-ci/usage/pushing-to-ecr/)).

- **Links in the design pages work.** CONTEXT and the ADRs link to each other by repo path, which
  the docs site served as-is, so every such link was a 404 (for example
  `/tech/context/docs/adr/0001-ci-as-durable-execution.md`). The docs build now points each one at
  the page it names, or at the file on GitHub when the site has no page for it, and fails on a
  link to a file that does not exist. Three ADR links that named wrong files are corrected.

## [0.3.2] - 2026-11-11

### Features

- **About Scarab.** The user menu's new entry opens `/about`: the server's version, the commit it
  was built from, when it started, its role and executor, whether sign-in, the GitHub App and
  plugins are on, and links to the release notes, docs, API description and source. It reads the
  new `GET /v1/about`; image.yml records the commit in the server image
  ([About Scarab](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#about-scarab)).

### Enhancement

- **Add forge connection replaces Add Forgejo connection.** The button on an org's Connections
  opens a dialog that asks which forge. For GitHub it lists the steps to install the App on the
  org's account and links to the App's install page, which the server reads from GitHub through
  the new `GET /v1/forges/github/app`; a server without a GitHub App says so. For Forgejo or
  Codeberg it shows the instance URL and token form as before. The New organization page asks
  the same question; after a GitHub install, **Check for the organization** opens the org the
  installation created.
- **Org secrets look like a repository's Secrets tab.** The org's Settings › Secrets shows its
  count in the header, New secret above the list, and one full-width list with a row per name and
  its **edit** and **delete** (they were **Replace** and **Delete**, on rows capped at 520px).
- **Pills are sized from their text, and every breadcrumb starts at home.** Status, state and
  role pills, the org page's repository states and the dashboard's inbox verbs all use the Mono
  text style on a 20px pill, so none is taller than the line beside it (they were up to 28px, some
  in the larger semibold Label style). The repository, run, Run pipeline, organization and
  settings page headings start with `/`, a link to the dashboard: `/ › acme/orders-api › #22`.
- **One pill for every tag.** Status, state, role and kind tags all come from one `Pill`
  component: the Mono text style on a 20px pill, in one of a few tones and looks. The tags that
  were still sized on their own, such as the artifact list's **of record** and provenance, the
  user menu's roles, Run pipeline's `API` and input types, result types and the invoke chain's
  kinds, now match the rest. An artifact's name, pills, size and type sit on one line.

## [0.3.1] - 2026-11-11

### Breaking changes

- **Every user signs in again, and the upgrade needs `Recreate`.** A person now has a random,
  fixed Scarab id (`p_` and 32 hex characters) beside the readable name they sign in under, which
  is now called their handle (`github:alice`, `okta:s.patel@acme.example`). Migration 0093
  gives every person an id, stores it in place of the handle in role bindings, API token owners,
  approver lists, passkeys and pending step-up requests, and deletes every session, step-up
  confirmation and WebAuthn challenge. History (the run event log, the security event log, policy
  decisions, every `updated_by`) is not rewritten; it is shown by handle. A server from before
  0093 cannot run against a database migrated past it: take a backup, upgrade with the
  `Recreate` strategy (the chart's default), and restoring the backup is the way back.
  A role binding written before migration 0052 that names nobody gets a placeholder that the
  first sign-in producing exactly that name through the legacy provider takes over. Handles
  that differ only in case are kept as separate people and logged at every boot; a new handle
  that differs from an existing one only in case gets a `~2` suffix.
  ([ADR-0080](https://github.com/thulasi-ram/scarab-ci/blob/main/docs/adr/0080-principals-have-an-internal-id.md))
- **Member URLs carry the id, not the handle.** The member page is
  `/orgs/<org>/settings/members/<id>/manage`, read through the new
  `GET /v1/orgs/{org}/members/{id}`; old member links stop working. Passkey reset is
  `POST /v1/orgs/{org}/members/{id}/passkeys/reset`.
- **People in API responses are `{id, handle, display_name}`.** `GET /v1/orgs/{org}/bindings`
  and the forge import return `id` and `handle` in place of `subject`; the principal search adds
  `handle`; API token records add `owner`; `GET /v1/me`, `GET /v1/me/security` and
  `POST /v1/auth/login` add `handle`, and their `subject` is now the id. Attribution fields
  (`updated_by`, `created_by`, `pinned_by`, approvals, the run event log's `by`) show the handle.
  Grants, revokes, approver lists, `scarab-server admin grant --subject` and
  `SCARAB_OAUTH_OWNERS` take a handle, as before, or an id.
- **The Actor of a manual or API Run is the dispatcher's handle**, so `when: event.actor == …`
  keeps comparing the name people type. Policy inputs keep their handle fields and gain
  `principal_id`, `trigger.principal_id`, `actor.principal_id`, `approval_ids` and
  `approver_ids` beside them.

- **`GET /v1/repos/{org}/{repo}/environments/{name}/deployments` is removed** and answers 404.
  It listed who approved which Run into an Environment, and the web UI no longer reads it.
  The server no longer keeps that record: each Run keeps its approvals in its own history.
  Migration 0088 drops the `deployments` table and the rows in it.
- **The environment migration report is removed.** `GET /v1/orgs/{org}/environment-migration-report`
  and `POST /v1/orgs/{org}/environment-migration-report/{env}/acknowledge` answer 404, and the
  org Environments page no longer shows "migrated environments to review". Migration 0088 drops
  the `environment_migration_report` table and `legacy_project_environments`, the per-repository
  environments migration 0057 turned into org Environments. The overlays, privilege grants and
  eligibility 0057 made from them stay. A server from before 0088 cannot run against a database
  migrated past it.
- **Server administration is removed from settings.** Account settings and Organization
  settings are separate pages. A global admin creates a new organization from "New
  organization…" in the org dropdown (`/orgs/_new`); `/settings/server/…` redirects there.
- **Installing the GitHub App no longer enables its repositories.** An `installation` or
  `installation_repositories` delivery registers the connection and binds nothing; an org
  admin enables each repository on the new org page. Repositories bound before the upgrade
  stay enabled. A push to a repository that is not enabled is answered `200` with
  `{"ignored": "repository not enabled"}` and starts nothing. `POST
  /v1/connections/{id}/resync` binds nothing either: its response lists the repositories that
  are not enabled in `available` and the projects it moved off a connection that does not
  serve their org in `moved`; the `bound` field is gone. A repository removed from the App's
  installation, or one of an uninstalled or suspended App, is no longer unbound: it keeps its
  project and is marked unreachable (`unreachable_since`), its events are ignored, a manual run
  answers 409, and the mark clears when GitHub covers it again or a re-sync lists it. Disable
  removes it. This needs migration 0089, which adds a column.
  ([Enabling repositories](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#enabling-repositories))
- **Organization settings › Connections no longer lists repositories.** Each connection shows how
  many it has enabled and links to the org page narrowed to it (Manage repositories); Add
  repository, the per-repository Disable and the per-repository Webhook button are gone from it.
  Repositories are enabled and disabled on the org page, and a Forgejo repository's webhook is
  registered again from its Settings tab (Register webhook). `GET /v1/connections` returns
  `enabled_repos`, a count, in place of the `projects` list. `GET /v1/repos` gives each project's
  `connection_id`, and `GET /v1/orgs/{org}/repos` takes `connection=<id>`.
  ([Enabling repositories](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#enabling-repositories))
- **Scarab's libraries and plugins are installed like anyone else's.** The server reads no
  release manifest, marks no library version first-party, and installs nothing into a new
  org: a new org has no libraries until an org admin installs them. To install the clone
  library, fetch `github.com/thulasi-ram/scarab-ci//libs/clone.yaml@<full commit of a release
  tag>` under Organization settings › Libraries, review it and install it as
  `lib:thulasi-ram/clone`. Until then a Pipeline that invokes it fails its check with
  "`lib:thulasi-ram/clone` is not installed in this org".
  ([Install Scarab's clone library](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#install-scarabs-clone-library))
- **The clone library is no longer trusted with forge credentials by default.** On a Forgejo
  or GitHub token connection, a step receives the long-lived token only when an org admin
  trusted its library's source (`POST /v1/orgs/{org}/library-trust`, with a step-up). A Run
  that invokes the clone library from an untrusted source is refused with the reason. On a
  GitHub App connection nothing changes. Migration 0090 adds the trusted source for every org
  that had the clone library from 0.3.0's release manifest, so those orgs keep cloning.
  ([Trusting a library with forge credentials](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#trusting-a-library-with-forge-credentials))
- **Plugins install from a manifest source and an image digest.** There is no compiled-in
  catalog: an org admin gives the manifest's source
  (`<host>/<owner>/<repo>//<path>@<commit>`, read like a library source), reviews what it
  asks for, and installs it with an image pinned `<repository>@sha256:<digest>` from any
  repository (`POST /v1/orgs/{org}/plugin-fetch`, then `POST /v1/orgs/{org}/plugins`). The
  install body takes `source` and `manifest_sha256` in place of `plugin`, and the server
  refuses it (409) when the source no longer reads as the reviewed manifest.
  `SCARAB_DEV_PLUGIN_ALLOW_TAG` admits a tag for every plugin alike.
  ([Install a plugin](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#install-a-plugin))
- **Removed:** the server variables `SCARAB_LIBS_MANIFEST` and `SCARAB_PLUGIN_ARGOCD_REPO`,
  the chart values `scarab.libsManifest` and `plugins.argocdRepo`, and the endpoints
  `POST /v1/orgs/{org}/library-defaults/retry` and `GET /v1/orgs/{org}/plugin-catalog`
  (404). Releases no longer carry a `libs-manifest.json` asset.
- **Migration 0091** adds `manifest_source` and `manifest_connection` to
  `plugin_installations` and a `plugin_installation_versions` table. Existing installations get
  no source and show "source not recorded". A server from before 0091 cannot run against a
  database migrated past it.
- **A plugin manifest may name its image repository** (`"image": "ghcr.io/acme/plugin"`), and an
  install or upgrade with an image of another repository is refused (400). Scarab's Argo CD
  manifest names `ghcr.io/thulasi-ram/scarab-plugin-argocd`, so installing it from a fork's
  repository needs a manifest that names the fork. A Scarab from before this release refuses a
  manifest with the field.
  ([Install a plugin](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#install-a-plugin))
- **Migration 0090** adds the trusted sources above (trusted by `scarab:migration-0090`),
  drops `library_versions.first_party` and the `library_org_defaults` table. Installed
  libraries, their versions and yanks stay. A server from before 0090 cannot run against a
  database migrated past it.

### Features

- **Upgrade a plugin in place.** An installation's page has Upgrade: give a new commit (the
  recorded source's file at it) or another source, and a new image digest. Scarab shows what the
  new manifest adds, removes and keeps, then applies it to the same installation, keeping its
  config, storage and connections. A new image digest is new code, so an upgrade to one asks for
  your passkey, as an install does; so does a manifest that asks for more. Only a narrower
  manifest on the installed image needs no passkey. The server decides which, and refuses an
  upgrade while plugins are off
  (`POST /v1/orgs/{org}/plugins/{name}/upgrade-fetch`, then `…/upgrade`). Each installation now
  records where its manifest was read.
  ([Upgrade a plugin](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#upgrade-a-plugin))
- **Any plugin can be installed.** An org admin fetches a plugin's manifest from a git source
  pinned to a commit, reviews its capabilities, connection slots, config keys and gates, and
  installs it by image digest with a passkey. Scarab's Argo CD plugin installs the same way;
  its image is now published as `ghcr.io/thulasi-ram/scarab-plugin-argocd` with the other
  images, for linux/amd64 and linux/arm64.
  ([Install a plugin](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#install-a-plugin))
- **An org page at `/orgs/<org>`.** It shows the org's enabled repositories, its runs and
  failed runs in the last 24 hours and its runs waiting on a gate, and lists every repository
  the org's connections reach, 25 to a page, each Enabled or Not enabled. Org admins enable a
  repository there, or disable it after typing its name; disabling removes its project, and
  nothing the forge sends enables it again. A repository's Settings tab has the same Disable
  for org admins, beside the unreachable line when the repository is marked so, and goes to the
  org page afterwards. The org name in a repository's header links to it.
  The list is `GET /v1/orgs/{org}/repos`.
  ([Enabling repositories](https://thulasi-ram.github.io/scarab-ci/get-started/deploy-helm/#enabling-repositories))
- **A policy editor in the web UI.** Organization settings › Policies has New policy and Paste
  YAML. The editor checks the draft as you type and shows each finding at its field or at the
  rule's line and column, completes field names from the checkpoint's input schema, starts a
  new policy in audit mode, and "Try it" lists the recent decisions whose outcome the draft
  would change, each linking to its Run. Saving a change to an enforced policy asks for a
  passkey. Each policy has a History tab with a line diff between versions, and the list shows
  how many Runs or Steps each policy denied, or would have denied, in the last 7 days, with
  Enforce on audit-mode rows.
  ([Policies](https://thulasi-ram.github.io/scarab-ci/usage/policies/))
- **Policy decisions on the Run page.** A Run that a policy applied to shows each decision: the
  checkpoint, each policy's outcome and message, and the input the rules saw.
  ([Policies](https://thulasi-ram.github.io/scarab-ci/usage/policies/#on-the-run-page))
- **Library and plugin policy forms.** The libraries allowed and blocked lists are edited under
  Library policies on the Libraries section, the plugin list under Plugin policies on the Plugins
  section, and a project's own under the same two titles on its repository Settings tab.
  ([Policies](https://thulasi-ram.github.io/scarab-ci/usage/policies/#library-and-plugin-policies))
- **Policy API for editors.** `POST /v1/orgs/{org}/policies/check` checks a draft (JSON or YAML)
  without saving it; `GET /v1/policy-checkpoints` serves each checkpoint's input schema;
  `POST /v1/orgs/{org}/policy-test` evaluates `step.admit` and `gate.resolve` inputs too;
  `POST /v1/orgs/{org}/policy-test/replay` runs a draft over recent decisions; the decision
  log takes `checkpoint`, `policy_id` and `since`, and `…/policy-decisions/counts` counts each
  policy's denies since a time.
  ([Policies](https://thulasi-ram.github.io/scarab-ci/usage/policies/#api))

### Enhancement

- **Connections that serve no organization wait on New organization.** A global admin finds
  them under **Waiting for an organization** on `/orgs/_new`, with their forge, installation id,
  credential health and last delivery, and assigns each to an organization, creates a new
  organization from it, or removes it. An org's Connections lists only that org's connections.
  The org dropdown and the command palette count them: **New organization… (1 waiting)**.
- **Repository roles moved to the repository.** Organization settings › Members shows each
  person's org role only, grants and revokes org roles, and links the repositories a person also
  holds a role in. A repository's Settings tab has an **Access** panel: who holds a role on it,
  whether it was granted in Scarab or came from the forge's collaborators, the people its org
  roles reach, grant and revoke at the repository, and **Re-sync collaborators**, which shows
  who was added, changed and removed. The tab's placeholder rows are gone; it shows the
  repository's forge connection. `GET /v1/orgs/{org}/bindings`
  now says on each row whether the role was granted in Scarab (`origin: native`) or seeded by
  a forge import (`origin: import`).
- **A repository's Secrets tab is one key list.** Each key shows where it is set; expand it
  for one line per scope. An Environment filter shows each key's value for that Environment,
  and New secret takes a scope. It replaces the list and the grid.
- **Settings pages each have their own sidebar**, and Organization settings has an org
  dropdown at the top. The user menu links Account settings and Organization settings.
- **An org's security event log is in Organization settings › Security**, below the step-up
  settings, with its filters and paging. **Refusals, last 24h** filters it to recent refusals
  and replaces the recent-refusals table. The Event log section is gone;
  `/orgs/<org>/settings/events` redirects to Security and keeps its filters.
- **The web UI uses four text styles.** Title (Inter 600, 20px), Label (Inter 600, 13px), Body
  (Inter 400, 13px) and Mono (JetBrains Mono 400, 12px) replace the five sizes, four weights and
  display face the pages mixed. Headings and column heads are in sentence case instead of
  upper case, and body text is 13px instead of 15px. The wordmark keeps its own face.
- **Each library and plugin installation has its own page.** Organization settings › Libraries
  and › Plugins list one row per installation, with tags such as yanked, untrusted source,
  killed or probe failed, and **Install library** and **Install plugin** at the top right open
  the install dialog. Upgrading, trusting a source, yanking and removing a library, and killing,
  storage, connections, the broker journal and uninstalling a plugin are on the installation's
  page (`…/settings/libraries/<id>/manage`, `…/settings/plugins/<id>/manage`). Migration 0092
  gives each library installation an id, kept across upgrades; `GET
  /v1/orgs/{org}/library-installations/{id}` and `GET /v1/orgs/{org}/plugin-installations/{id}`
  read one installation by its id. A `…/libraries/<publisher>~<name>/manage` link goes on to the
  installation's id page.
  The library review page lays its facts and checks out on one grid, with Install in one row
  under the diff.
- **Headings read as headings, and Mono is heavier.** Panel headers, section headings and the
  settings sidebar's heading are in the primary ink instead of the muted one. The settings
  sidebar marks the page you are on with its background and colour only, without the green
  edge. Mono text is set at weight 500 instead of 400; the four text styles stay four.
- **A table inside a panel no longer draws a second header band.** Members, a repository's
  Access and Environments, Connections, API tokens, Libraries, Plugins, the security event
  tables and a plugin's broker journal show their column heads as a plain label row over one
  hairline, under the panel header, with their columns at the header's inset. Access's
  "Through their org role" row is a plain label row with a hairline above it.
- **Environments, Policies and Members follow the same list-and-page layout.** Each list has its
  add action at the right of its header (**New environment**, **Paste YAML** and **New policy**,
  **Grant role**) and a **Manage** link per row. Account settings › Security has **Add passkey**
  at the right of the passkeys panel's header; it opens a dialog that asks for the new
  passkey's name. An Environment's read-only page and edit screen
  are one page, `…/settings/environments/<name>/manage`, with the details as one form; the old
  addresses redirect there. A Policy's page gains a **Decisions** tab with the decisions it
  matched; the Policies section drops its org-wide recent decisions and its read-only typed
  settings, which are edited on their own pages. Each member has a page,
  `…/settings/members/<id>/manage`, to change or revoke their org role and see their
  repository roles.
- **One dropdown across the web UI.** Every form picker, the org dropdown in Organization
  settings and the section picker on a narrow screen open the same list as the run filters, in
  place of the browser's own. A list longer than eight entries has a search box; a shorter one
  is a plain list. It works from the keyboard (arrow keys, Home, End, type-ahead, Enter,
  Escape), shows an org you do not administer without letting you pick it, sets New
  organization… apart under a rule, and opens above a dialog. The docs version dropdown is a
  menu of links to each version.
- **Connections, Secrets, API tokens and Security in Organization settings are reorganised.**
  Connections lists one row per connection, with tags such as credential missing or no
  deliveries yet; each connection has its own page (`…/settings/connections/<id>`) with its
  credential, last delivery, app check, Manage repositories, Re-sync and Remove. Secrets has
  New secret at the top right and Replace and Delete (with the name typed) on each row. Both API
  token pages list tokens with the same row and tags (active, expires soon, revoked, never
  used), and New token moves to the header of Account settings › API tokens. On Security, an
  owner resets a member's passkeys from a Reset passkeys dialog instead of an inline list.
- **A repository's schedules have their own tab.** The Schedules tab lists each `on: cron:`
  schedule with its next and last run and how the last one came out, and links to its runs. The
  Runs tab drops the schedules panel and gains a **scheduled** filter, which also keeps a
  schedule's refused runs.
- **A library's page lists its yanked versions.** The table holds every yank of that library,
  earlier versions included, with Lift yank; the Libraries list keeps only yanks of libraries
  that are no longer installed. A yank in the API now carries the library's `publisher`, `name`
  and `source`.
- **Library policies show the sources trusted with the forge token.** The list is read-only and
  links each source to the libraries installed from it; trust is still given on a library's
  page.
- **An open environment shows the org's rules beside the rules in force.** A repository's
  Environments tab sets each rule's org value beside the value that applies, and marks the rules
  the repository changed. The row names the environment once; the name pipelines use is in the
  detail. The This repo cell is plain text, with a warning icon for not eligible and no rules.
- **One Link a forge account button** replaces a Link button per forge in Account settings; with
  more than one forge left to link, it asks which.
- **Tables and run rows are easier to read.** Column heads are lighter than the rows under them,
  and a row's first cell is in the primary ink. A run row shows its Headline as Label and its
  facts in the secondary ink. A secret's scope lines line up across every open key.

### Bug Fixes

- **Finished step Pods are deleted.** The k8s executor kept every step Pod after its step
  ended, and nothing removed them: the demo had 472, and 9 Pending Pods of a dead-lettered
  Run had failed scheduling for 72 days. The server now deletes the Pods of a Run that has
  been finished (succeeded, failed, cancelled or dead-lettered) for longer than
  `SCARAB_RETENTION_STEP_POD_MINUTES` (default 60), along with a shared service's Service
  and NetworkPolicy, up to 200 objects every five minutes. A Pod whose Run is not in the
  server's own database, a Pod of a Run that has not finished, and a running debug Pod are
  left alone. The chart's executor Role gains `list` on Services and NetworkPolicies
  (git-bug 986542a).
- **A step that fails before it starts no longer leaves a Pod that holds CPU.** When a step
  failed before its process ran (Unschedulable, a configuration error, missing inputs), its
  Pod was kept and its Secrets were deleted. A Pod that was Pending was later placed on a
  node, could not mount its Secrets, and held its CPU and memory request in Init. On the
  demo's two-core node, two such Pods made every later step Unschedulable, and every Run
  dead-lettered for nine hours. The engine now deletes the Pod when the step fails this way,
  as it already did when a step did not start within the provisioning deadline. A Pod whose
  step did run is kept, as before.
- After linking an account that merged another one in, the redirect no longer names the merged
  account in its URL; it says `merged=1`, and the page says another account was merged.
- The sign-in screen showed both logos in the light theme.
- A repository row on the dashboard no longer repeats its name when the forge repository is
  named like the project.
- A repository page no longer repeats the Environments tab's and the Secrets tab's actions
  in its header, and the Environments tab no longer lists approval history; each
  Environment links to its Runs.
- On a phone no page scrolls sideways. The top bar keeps Search, Docs, API and the user menu
  in the window (Search shows its icon only); a run's actions wrap onto more lines; the
  dashboard's waiting rows put what they are about on a line of their own; and the Libraries,
  Plugins, Connections and security event tables scroll inside their own box.

### Misc

- The per-Project environment rules that migration 0088 dropped are gone from the code:
  `Environment`, `ProtectionRules` and the conversion `migrate_legacy_environment`, and the
  tests that only checked that conversion. Tests set up Environment baselines, overlays and
  grants directly (#163, git-bug 7ecf4bc).
- A plugin gate test no longer fails when a random installation id has no letter in it (#163).

## [0.3.0] - 2026-11-09

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
  repository is first bound to it. **An org that existed before the upgrade gets nothing
  installed,** and every Pipeline in it that invokes the clone library fails its check with
  "`lib:thulasi-ram/clone` is not installed in this org" until an org admin installs it:
  **Settings › Libraries › Install library**, pick the clone library from the release
  manifest, review it, and confirm with a passkey. Do this in each existing org right after
  the upgrade. A push that fails this check shows as a refused row in the repository's Runs
  list. For a new org whose install could not read the manifest, Settings › Libraries offers
  **Retry**, a mirror, or pasted files; `SCARAB_LIBS_MANIFEST` (a file path, an https URL, or
  `off`) changes where the server reads the manifest.
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
- **Caches start cold.** Cache keys now include the Run's cache scope (see Security), so no
  Cache saved before the upgrade is restored after it. Each Cache fills again on its first
  build. Migration `0081_run_cache_scope.sql` adds the scope to `runs`.
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
  `refuse`), which names the gate to add. A `forge_token: write` Step is no longer refused at Run
  creation for having no `manual` gate upstream; it gets the added gate like any other. See
  [Environments](https://thulasi-ram.github.io/scarab-ci/usage/environments/#the-approval-gate).
- **`manual` gates that guard no authority Step stop needing the approvers.** Only a gate with a
  Step after it that uses the Environment's authority needs the Environment's approvers and a
  step-up; any other `manual` gate in the Run releases on the first approval by anyone with write.
- **The Environment's wait timer is enforced** (git-bug 2b94989): the approval gate releases when
  its approvals are in and the timer has run out, counted from when its `needs` finished. An
  approval given early counts. The API refuses a non-zero wait timer on an Environment with no
  approvers; one stored before does nothing. The deployment history records each approval gate
  once, when its approvals are in, so a gate the timer releases is listed too.
- **A stale approval is refused at launch** (git-bug 220917f). Every launch of a Step that uses
  the authority of an Environment with approvers, reruns and retries included, fails `Config`
  when no released approval gate is upstream of it, the Run's ref is no longer allowed, the
  approvers changed since the gate's approvals, or the gate released more than the Environment's
  maximum approval age ago (24 hours by default). One released approval gate upstream that passes
  these checks is enough. An external gate no longer hands over the
  Environment's authority by its token alone. A Run created before its Environment had approvers
  has no approval gate, so its authority Steps are refused once the Environment has approvers.
  In such an Environment, a Step gets the Environment's OIDC subject only when an approval gate
  upstream passes the same checks, whether or not it uses the authority otherwise; any other Step
  gets `env=none`.
- **Adding an approver needs a step-up when the approval gate needs a count** (`required_approvals`):
  the new approver counts toward it. Where every approver must approve, adding one still needs
  none.

- **Step secrets are files, not environment variables.** A secret listed in `secrets:` is now
  the file `/scarab/secrets/KEY` in the step's container (`$SCARAB_SECRETS` names the
  directory), and `$KEY` is empty. To keep reading `$KEY`, list the secret as
  `{ name: KEY, env: true }`; bare names and entries mix in one list. `scarab validate` warns
  about a command that reads `$KEY` for a secret without `env: true`. A secret name must now be
  a file name (letters, digits, `-`, `_`, `.`, not starting with `.`), and `forge-token` and
  `forge-username` are refused (git-bug bcf2c11). Upgrade every server replica before a pipeline
  uses `env: true`: a replica from an earlier version cannot read that entry in a stored Run.

- **With `plugins.enabled: true`, the chart limits the plugin namespace and the broker port**
  (#133, #135). Every plugin Pod restarts once on the upgrade, for its new 5 s termination
  grace. Migration `0087_plugin_installation_state_version.sql` adds a column to
  `plugin_installations`.
  - The chart puts a ResourceQuota and a LimitRange in the plugin namespace
    (`plugins.quota`): 50 Pods, requests 3 CPU / 4Gi, limits 25 CPU / 13Gi. That leaves room
    for 47 plugins next to the isolation probe's 3 Pods. Raise `pods` and the other four
    together, or Pods are refused before the count is reached.
  - A NetworkPolicy on the server Pods (`plugins.brokerIngressPolicy`, default `true`) admits
    the broker port only from plugin workloads in the plugin namespace. The API port stays open
    to every source. Set it to `false` only if another policy already governs those Pods.
  - The plugin namespace carries the label `scarab.io/plugin-runtime-owner=<release
    namespace>.<release name>`, and the runtime holds every plugin in a namespace labelled for
    another release or not labelled. If you create the namespace yourself
    (`plugins.createNamespace: false`), add the label, and create the namespace before
    `helm install`, since the chart now puts the quota in it. When `<namespace>.<name>` is
    longer than 63 characters, the chart refuses to render: set `plugins.owner`.
  - The isolation probe now also checks the internet over IPv6. On a dual-stack cluster whose
    CNI enforces NetworkPolicy for IPv4 only, the probe fails and plugins are held.
  - A probe that cannot tell (its Pods did not run) leaves running plugins running and starts
    none. Once a plugin's last passed probe is 12 hours old, it is held as a failed probe
    holds it; a passed probe releases it. The installation's `probe_failed` status says so.
- **Eligibility patterns match project names ignoring case** (#141). GitHub and Forgejo treat
  repository names that way, so a stored `Web*` now also admits `web-api`. Check the explicit
  eligibility lists of your Environments for a pattern that now admits more Projects than you
  want. Patterns are trimmed before they are checked and stored. A blank pattern, a pattern made
  only of `*` (choose *all projects* instead), and two patterns that differ only in case or
  surrounding spaces are refused (400). Patterns stored before the upgrade are not trimmed.

### Security

- Secret values were written into the step Pod's spec as literal environment values, where
  anyone who can read Pods (and the Kubernetes audit log) could see them (git-bug bcf2c11).
  They now live in a per-Pod Kubernetes Secret, deleted when the attempt ends, and reach the step
  as files on a read-only in-memory volume; an `env: true` secret is a `secretKeyRef` to that
  Secret.

- A pull request could save a Cache that a Step in an Environment-governed Run then restored,
  so a deploy could ship bytes a pull request wrote (git-bug efcec77). Every Run now has a
  cache scope, fixed when the Run is created and kept by its reruns. In a Run governed by an
  Environment, a Step that an approved Environment approval gate admits uses `env:<name>`: it
  reads and writes only that scope, so its first build is cold. A Step before the gate uses the
  Run's ref, so a push to any branch whose Pipeline names the Environment cannot write what the
  next approved deploy restores (git-bug 31a2533). The test is the one that gives a Step the
  `env=<name>` OIDC subject. Any other Run uses its ref (`refs/heads/feat`, `refs/pull/7/head`): it writes only that
  scope, and when that scope has no entry it reads the default branch's. A pull request never
  uses an Environment's scope, even when its Pipeline names the Environment and the Environment
  admits it: it uses its own ref like any other Run. A pull request reads `main`'s Cache and
  never writes it. Pull requests, which saved nothing before, now save to their own scope. A Run
  with no ref (a comment trigger) has its Cache off. The server asks the forge for the default
  branch when it creates a Run that declares a `cache:`; when the forge does not answer, that
  Run reads only its own scope.
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
- **Opening a debug shell needs a step-up.** A shell into a step Pod (`…/attach`, and a
  debug Pod, `…/debug-pod`) reads the Pod's environment and acts with its credentials, but it
  asked only for `Administer`. It now needs a step-up confirmation, as a secret write does: a
  browser session's 5-minute window satisfies it. An API token or a session presented as
  `Bearer` can no longer open one, since a WebSocket cannot be held for a confirmation. A
  browser's WebSocket cannot send `x-csrf-token`, so the web UI sends the session's CSRF token
  as the `scarab.csrf.<token>` subprotocol, and the server answers with `scarab.v1`. Before it
  opens the socket, the UI makes a plain `GET` of the same URL, which asks for the window when
  the session has none (git-bug aeba20a).
- An Environment secret could be written under a name with no Environment, outlive its
  Environment, and be picked up by a later Environment of the same name; one written into a
  Duplicate's target while the Duplicate ran was overwritten or deleted by it (git-bug
  52964b4). `POST /v1/secrets` now answers 409 for an Environment that does not exist, and
  deleting an Environment answers 409 while any Project holds a secret in it. `GET
  /v1/orgs/{org}/secrets/orphaned` lists, by name, the secrets already left under Environment
  names that no longer exist, so an admin can delete them.
- A secret value that is not UTF-8 (a keystore, a binary key) reached the Step changed: it was
  read as text and invalid bytes were replaced. Values are now bytes from the store to the
  file; the per-Pod Kubernetes Secret uses `data`. An `env: true` secret must be UTF-8 text,
  and a Step whose `env: true` secret is not fails with a configuration error that names it
  (git-bug c1f61c9).
- **Passkey counter checks.** Two uses of one passkey verified at the same time could flag it
  as a suspected clone when the later count landed first. A use whose count was ahead of the
  stored one when it was checked, and differs from the count stored since, is now recorded;
  the same count twice still flags. A synced passkey (backup-eligible), whose devices do not
  share a counter, is no longer flagged when its counter does not move; the use is recorded
  as `passkey.counter_regressed`. Once every passkey of a user is flagged, a fresh login no
  longer enrolls a new first passkey (a thief holding the forge account can make a fresh
  login): an Owner must reset the user's passkeys first, and a step-up refusal says so.
- **Merging in an account that has passkeys needs one of its passkeys.** Linking a sign-in that
  is the only way into another Principal folds that Principal into yours: its roles, approver
  entries and tokens become yours. Someone who controlled only that forge login could take
  them over, confirming with their own passkey (git-bug bbb5204, ADR-0079). When the folded
  Principal holds any passkey, the link's callback now changes nothing and sends you to
  confirm the merge with one of that Principal's passkeys (`GET` and `POST
  /v1/me/identities/merges/{id}`, migration 0084); Settings → Profile asks for it. A Principal
  with no passkey merges as before. A merge deletes the folded Principal's passkeys instead
  of moving them, and records each as a `passkey.removed_on_merge` security event naming
  both Principals.
- **Unpinning a Run's snapshots needs a step-up.** `DELETE /v1/runs/{id}/snapshots-pin`
  shortens how long the Run's snapshots are kept, so it now needs a step-up confirmation
  (window). Pinning does not.
- **The CredentialBroker keeps a plugin's credential to its upstream** (#131, #136). A response
  body that holds the bound credential is cut before any part of it reaches the plugin, however
  the chunks split it. The broker connects only to an address that is not link-local, loopback
  or a cloud metadata address, checked once per connection, NAT64 and IPv4-compatible forms
  included; private ranges stay allowed. It no longer forwards the plugin's `Accept-Encoding`,
  and refuses a response encoded anyway (502 `encoded body`), because it cannot scan a
  compressed body. A cached credential is read from the secret store again once it is 60 s
  old, so a rotated secret is used within a minute and a deleted one is refused (409,
  journaled `credential missing`). An installation the plugin runtime holds is refused. Each
  installation may make 600 requests a minute (429 `rate`), and the broker takes 256 at once
  (503 `broker busy`). A `pc-*` secret that no binding names is deleted after 10 minutes.
- **A withdrawn privilege grant no longer applies to reruns** (#138). Run creation checked a
  Step's `privileged`, `add_capabilities` and raw `k8s_overlay` against the Project's privilege
  grant in the Environment, but a launch checked only that the Environment still admitted the
  Project, so a rerun of an old Run kept a grant an admin had narrowed since. Every launch now
  checks the Step against the grant as it stands then, and a refused Step fails `Config` with
  the reason. `run_as_root` is self-service and is not checked again. Adding an overlay for a
  Project also checks eligibility in the same transaction, so an eligibility change made at the
  same moment answers 409 instead of leaving an overlay for a Project that is no longer admitted.

### Features

- **Environment display name and Duplicate.** An Environment's name stays fixed: pipelines and
  the OIDC subject (`…/env/<name>/…`) use it. It now has an optional display name, which lists,
  headings and pickers show with the name beside it; changing it needs no step-up. **Duplicate**
  (`POST /v1/orgs/{org}/environments/{env}/duplicate`, under step-up) creates a new
  Environment with the old one's baseline, eligibility, every project's overlay and privilege
  grant, and its secrets with their values, and lists what it did not copy: the projects whose
  Runs used it, the policies that name it, and the OIDC subject cloud trust policies match. The
  old Environment is not changed. A second Duplicate to a name already being duplicated to is
  refused (409) before it writes anything, and so is creating that name with
  `PUT /v1/orgs/{org}/environments/{env}` while the Duplicate runs. A display name may not hold invisible bidi or
  zero-width characters. Migration 0080 adds the display name column (git-bug d002a0c).
  [Environments](https://thulasi-ram.github.io/scarab-ci/usage/environments/#duplicate-an-environment)
- **Approval gate settings on the Environment page.** The edit page has an **Approval gate**
  section: what to do when a pipeline has no approval gate (**Add an approval gate** or **Refuse
  the run**), the added gate's expiry, how many approvers must approve, and the maximum approval
  age. The wait timer is disabled while the Environment has no approvers, and an Environment
  with a timer and no approvers shows a warning (git-bug cd43bf3, 2b94989).
  [Environments](https://thulasi-ram.github.io/scarab-ci/usage/environments/#the-rules-it-holds)
- **The Run page marks Environment approval gates.** Their gate card, DAG node and step pane say
  **Environment approval gate** and name the Environment by its display name with its name in
  brackets; the gate the Environment added says **added by** it. After the approvals are in, a
  gate with a wait timer shows when it releases, and approving it says the approval was recorded
  and when the gate releases. `GET /v1/runs/{id}` gains `environment` (`name`, `display_name`)
  (git-bug cd43bf3, 2b94989).
  [Environments](https://thulasi-ram.github.io/scarab-ci/usage/environments/#the-approval-gate)
- **Environment page wording.** "Who may deploy" is now "Projects that may use <Environment>":
  the list admits every Run that names the Environment, not only deploys. The "Projects" panel
  is now "Project overlays and grants" (git-bug c2f97b4).
  [Environments](https://thulasi-ram.github.io/scarab-ci/usage/environments/#view-and-edit-an-environment)
- **Settings › Policies.** One read-only page per org lists its CEL Policies, with what each
  applies to and how often it matched, denied and (in audit mode) would have denied in the
  newest 200 decisions, the newest decisions, and the typed settings (each Environment's rules,
  the library and plugin lists, API token scopes) with links to where each is edited (git-bug
  c2f97b4). [Every rule in one place](https://thulasi-ram.github.io/scarab-ci/usage/environments/#every-rule-in-one-place)
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
- **Broker journal** (#139). The CredentialBroker records every request a plugin makes through
  it and keeps the rows 14 days. An org admin now reads them under Settings › Plugins, where
  each installation has a **broker journal** table (time, phase, method, path, rule, status,
  bytes, duration, reason) with a phase filter, a filter for refused or cut-short requests, and
  **Load older**. The API is `GET /v1/orgs/{org}/plugins/{name}/broker-journal` with `limit`,
  `before`, `phase` and `denied`. It returns the names of a request's query parameters, never
  their values.
  [Waiting with the Argo CD plugin](https://thulasi-ram.github.io/scarab-ci/usage/deploying-with-argo-cd/#waiting-with-the-argo-cd-plugin)
- **Delete an Environment with what it holds** (#141). While Projects hold overlays, privilege
  grants or secrets in an Environment, deleting it is refused, and the 409 now lists them as
  JSON, with a count and the names for each kind. `DELETE
  /v1/orgs/{org}/environments/{env}?cascade=true` deletes the Environment's secrets, then its
  overlays, grants, eligibility and the Environment in one transaction. If a step fails, the
  Environment stays and a retry finishes the delete. The delete dialog lists what blocks it
  and offers to delete it all behind the typed-name confirmation, and the step-up confirmation
  shows `cascade = true`.
  [View and edit an Environment](https://thulasi-ram.github.io/scarab-ci/usage/environments/#view-and-edit-an-environment)

### Enhancement

- `PUT /v1/orgs/{org}/environments/{env}/protection` takes the approval gate settings under
  `approval_gate` (`missing_gate_mode`, `added_gate_expiry_s`, `required_approvals`,
  `max_approval_age_s`), the shape its `GET` returns, so a client can send back what it read.
  The same fields at the top level are refused (git-bug 16c48e1).
- `POST /v1/runs/{id}/gates/{step}/approve` answers with a typed body (`released`,
  `approvals`, `releases_at`) described in the OpenAPI spec, which also lists its 403.
- The `run.create` and `step.admit` Policy inputs list a Step's `env: true` secrets as
  `step.env_secrets`. Decisions recorded earlier lack it, so read it as
  `has(step.env_secrets) && 'NAME' in step.env_secrets`.

- **The Environment's approval-gate rules run in the policy engine.** The gate the Environment
  adds (`run.create`), the release rules of an Environment approval gate (`gate.resolve`) and
  the check at each launch (`step.admit`) are built-in rules of the engine. Each consultation
  is recorded in the decision log, with the rule by name, even when no Policy is stored. Org and
  Project Policies can now be saved at `gate.resolve` and `step.admit`, with `deny` rules only:
  a denied approval answers 403 and is not recorded, and a denied launch fails the Step
  `Config` (git-bug c2f97b4). A `step.admit` Policy applies to every launch its `match`
  selects, in Runs that no Environment governs too. An approval that races another one is
  checked again over both approvals, and a deny then answers 403 with the approval kept.
  `step.admit` decides once per Attempt: a launch of the same Attempt delivered again (after a
  crash between the decision and the Pod) acts on the decision recorded first, including
  whether an approval admitted the Step (its OIDC subject and cache scope), so a Policy
  stored or an approval aged out in between applies from the next Attempt (git-bug 7c2514a).
  Decisions were recorded under attempt 1 for every Attempt, and the `step.attempt` a Policy
  reads was always 1; both now use the Attempt's own number.
  [Environments](https://thulasi-ram.github.io/scarab-ci/usage/environments/#the-approval-gate)
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
- **A refused deploy shows on the commit** (#140). When a push starts a pipeline that names an
  Environment that does not exist, or one that does not admit the Project, the server now posts
  a `failure` commit status on the pushed commit under the pipeline's `scarab/<pipeline>`
  context. Its description says what to change, and it links to the org's Environments page or
  to the Environment. A push from a branch the Environment's `allowed_refs` leave out posts no
  status, so a deploy pipeline limited to `main` does not mark every pull request failed.
- **Open Environments with no baseline rule are flagged** (#143). The Environment list counts,
  in a warning, every Environment that admits at least one Project and whose baseline has no
  approver, no ref rule, no wait timer and no reason. Those rows say **permissive baseline**, and
  the Environment's page asks for a baseline. Before, only the migration's split report warned,
  so an Environment created after it was never flagged. The split report's banner now counts
  only the lines left to review.
- **The Argo CD plugin's `wait-live` gate** (#142) reads an Application outside Argo CD's own
  namespace when `app` is written `<namespace>/<name>`, as the `argocd` CLI takes it; a plain
  name works as before. For a multi-source Application, the `revision` output is each source's
  revision in the order of `spec.sources`, joined by commas; it was empty. The plugin reads four
  gate waits at a time, so one slow Argo CD read no longer holds up the others. The docs name
  the `OnDelete` blind spot: Argo CD can report an Application Healthy while its old Pods still
  run.
- **Plugin runtime** (#133). A runtime that shuts down releases its lease, so another server
  replica takes over at once. An installation that is stopped or held, and already scaled down,
  costs no Kubernetes API writes per pass. Each NetworkPolicy the runtime applies is read back,
  and plugins do not start when the stored policy differs. The orphan sweep removes only objects
  of its own release. A kubelet a plugin can reach is a note on the probe status, not a failure.

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
- With two server replicas, a finished Run's commit status could end `pending`: one replica
  posted the verdict while another re-posted the Run's earlier `pending`. A `pending` post is
  now dropped whenever the Run has finished since, whatever state its verdict post is in.
- A Run waiting on a gate whose dependents had all finished without it (a sibling Step
  failed, so they were skipped) stayed Suspended until someone acted or the gate expired. The
  gate now ends Skipped ("no longer needed") and the Run settles at once. A gate with no
  dependents keeps waiting: its verdict is the Run's outcome. A rerun that makes a dependent
  wait again puts the gate back to waiting. The skip applies only once a Step of the Run has
  failed or been cancelled: a gate whose dependents `when:` excluded is a sign-off, so it waits
  for someone instead of being skipped before its own `needs` run (git-bug 0130969).
- Two approvals that met an Environment's rules at the same moment each wrote a record to the
  approval history (`GET /v1/repos/{org}/{repo}/environments/{name}/deployments`), so it showed
  one approval twice. A record now names the gate wait it belongs to, and the history holds at
  most one per wait; a rerun of the gate still gets its own (git-bug 1d06f2d, migration 0085).
- Two admins writing one org Environment at the same time could get a 500 when Postgres
  reported a deadlock (`40P01`) or a serialization failure (`40001`). Those are now a 409 that
  says to retry. `PUT /v1/orgs/{org}/environments/{env}` takes `If-None-Match: *` (create
  only) and `If-Match: <etag>` (replace only the Environment that was read), answers 412 when
  either does not hold, and returns the Environment's `ETag`, as does its GET. Without either
  header it creates or replaces as before.
- A Step whose image sets `USER` to a name failed with only `CreateContainerConfigError`.
  The failure now says to use a numeric user: Steps run as non-root, and the kubelet can
  check only a numeric one. Scarab does not override the image's user.
- **A Step's `timeout:` counts from when its process starts** (#123). It counted from when the
  Attempt was claimed, so waiting for a node, pulling the image and fetching the workspace used
  it up, and a Step that never ran failed as `Timeout`. Until the Step runs, a 30-minute limit
  applies instead: a Step that has not started by then fails as never-started infrastructure,
  with a message that says what it was waiting for, and is retried automatically. On Kubernetes
  the Pod's `activeDeadlineSeconds` is now the timeout plus those 30 minutes. Migration
  `0086_attempt_execution_started_at.sql` adds `attempts.execution_started_at`.
- A launch that created its Pod and then failed on every delivery (on Kubernetes, the Pod was
  accepted and the Secret write failed) dead-lettered its Run and left the Pod running. The
  Run's teardown now finds and deletes it (#121).
- An operator's cancel that was interrupted after its Steps were cancelled and before the Run
  was written (a crash or a database error) ended the Run Failed, with a failure commit status.
  It now ends Cancelled, unless a rerun or retry was asked for after the cancel (#125).
- **Forgejo pushes stopped starting Runs after the webhook secret changed.** Binding a
  repository again kept the existing hook, which still carried the old secret, so every
  delivery got a 401. Forgejo does not change a hook's secret when the hook is edited, so
  registering a hook now deletes the repository's hook for Scarab's callback and creates it
  again with the current secret. A push made between the two is not delivered (#137).

### Tests

- The fault tier (`just test chaos`, ADR-0077) runs ranks 1, 2 and 4: a Depot Pod deleted
  mid-drain and mid-feed, two server replicas sharing the work, a server replica killed while
  it drains, and the scheduler leader paused past its lease. All six scenarios passed on
  2026-11-01 (`just test chaos all 3`). `pause-node`, last in `all`, freezes the kind node
  mid-drain for longer than the lease TTL and shorter than the Pod's grace period; every Run
  must still succeed. C1 checks that a Cache restores from its home replica when no fault
  hit that replica.
- `just test e2e` gains `results_outputs`, where a Step writes two results and the next reads
  them through `${{ outputs.<id>.<name> }}` in its `env:`, and a check that a delivery under the
  current Forgejo webhook secret verifies after a hook made with an older one (#137). The plugin
  runtime's cluster suite tests the `cidr:` broker peer, with a negative case, and the Argo CD
  pass checks that a Pod outside the plugin namespace cannot reach the broker port (#133).
- The Environment migration equivalence tests cover Projects whose Runs name an Environment
  only other Projects define, and Projects that never ran a deploy. One fixture file checks
  both the server's effective-protection merge and the mock UI's (#144).

### Documentation

- The Installation page describes forge connections per org (Settings › Connections, Connect a
  forge, `org:` in `scarab.connections`, a connection with no org) and the settings page with
  its addresses. The Libraries, Secrets and Environments pages name the new settings addresses,
  the per-org connection rule for private library sources, inherited secret keys, and the
  Environment view and edit pages.

### Misc

- `rust-toolchain.toml` names an exact Rust release (1.97.0) and `rustfmt.toml` pins the style
  edition, so what `cargo fmt --check` accepts no longer changes with each Rust release. The
  workspace is formatted, and CI (`openapi-drift`), `just check all` and the dogfood `lint`
  step check it (git-bug 0b78f43).

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
- **New mark** (#130, #132). A rhinoceros beetle seen from the side, in ink and gold (cream and
  gold on dark surfaces), replaces the halftone mark in the web UI, the docs site, the README
  and the favicons, which use a one-colour version. `ui/brand/mark/` holds the source artwork
  and the trace, and `just brand [check | png]` regenerates the SVGs and PNGs.

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

[Unreleased]: https://github.com/thulasi-ram/scarab-ci/compare/v0.8.3...HEAD
[0.8.3]: https://github.com/thulasi-ram/scarab-ci/compare/v0.8.2...v0.8.3
[0.8.2]: https://github.com/thulasi-ram/scarab-ci/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/thulasi-ram/scarab-ci/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/thulasi-ram/scarab-ci/compare/v0.7.7...v0.8.0
[0.7.7]: https://github.com/thulasi-ram/scarab-ci/compare/v0.7.6...v0.7.7
[0.7.6]: https://github.com/thulasi-ram/scarab-ci/compare/v0.7.5...v0.7.6
[0.7.5]: https://github.com/thulasi-ram/scarab-ci/compare/v0.7.4...v0.7.5
[0.7.4]: https://github.com/thulasi-ram/scarab-ci/compare/v0.7.3...v0.7.4
[0.7.3]: https://github.com/thulasi-ram/scarab-ci/compare/v0.7.2...v0.7.3
[0.7.2]: https://github.com/thulasi-ram/scarab-ci/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/thulasi-ram/scarab-ci/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/thulasi-ram/scarab-ci/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/thulasi-ram/scarab-ci/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/thulasi-ram/scarab-ci/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/thulasi-ram/scarab-ci/compare/v0.4.4...v0.5.0
[0.4.4]: https://github.com/thulasi-ram/scarab-ci/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/thulasi-ram/scarab-ci/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/thulasi-ram/scarab-ci/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/thulasi-ram/scarab-ci/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/thulasi-ram/scarab-ci/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/thulasi-ram/scarab-ci/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/thulasi-ram/scarab-ci/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/thulasi-ram/scarab-ci/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/thulasi-ram/scarab-ci/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/thulasi-ram/scarab-ci/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/thulasi-ram/scarab-ci/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/thulasi-ram/scarab-ci/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/thulasi-ram/scarab-ci/releases/tag/v0.1.0
