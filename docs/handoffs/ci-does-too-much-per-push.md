# Handoff — CI does too much on every push

**Branch:** none yet — this is analysis, no code written.
**Tickets:** git-bug `b007837`, `6729fc2`, `b69c286`, `7fbf56d`, `602940c` (all `type:chore`, `area:infra`; each carries the same cross-reference comment).
**Evidence PR:** [#3](https://github.com/thulasi-ram/scarab-ci/pull/3) — a UI/art-only change, which is what made the waste legible.

---

## The one-line version

A pull request that changed only `ui/**` and one shell script cost **26.9 runner-minutes across 15
jobs**, and `ci/coverage` failed on it for a reason that has nothing to do with it.

## Read this first, in this order

1. **git-bug `b007837`** — the coverage ratchet is red on `main`. Nothing else can merge until this
   is settled, so it is not optional context.
2. **git-bug `6729fc2`** — the structural fix (path filters). Doing this first changes how much the
   others matter.
3. **`.github/required-checks.txt`** — read the header. It already defines the `required-if-run`
   tier *for exactly this case*, and `scripts/pr_gate.py` already implements it. The fix applies an
   existing convention; it does not need a new one.

---

## Where the time goes

Measured from the GitHub API on PR #3 (`gh api .../actions/runs/<id>/jobs`, `completed_at` minus
`started_at`):

| job | min | did it need to run? |
|---|---:|---|
| `coverage` | 9.2 | no Rust changed |
| `test` | 9.0 | no Rust changed |
| `ui-e2e` | 4.6 | advisory tier |
| `openapi-drift` | 0.8 | no API changed |
| 8 × `image/build` | 3.0 | 6 of the 8 images contain no UI |
| `ui-unit` + `ui-tests` | 0.6 | the same job, twice |
| **total** | **26.9** | **15 jobs** |

The image builds are individually cheap — the layer cache is doing its job. The problem there is
check-list noise, not minutes.

## The four findings

**1. `ui-unit` and `ui-tests` are byte-identical** (`7fbf56d`). Same `working-directory`, same
`npm ci && npm test`, same `node-version: 22`, same cache path. Only the comments differ, and they
claim different test-strategy phases for what is one vitest suite. `ci/ui-tests` is in
`required-checks.txt`; `ci/ui-unit` is not — so the duplicate is also the ungated one, which is why
it shows up under "not in required-checks.txt (ungated)" in `just pr-gate`.

**2. The Rust suite runs twice** (`b69c286`). `test` runs `cargo nextest run --workspace`;
`coverage` runs `cargo llvm-cov nextest --workspace`. The second *executes the same tests* — the
`--ignore-filename-regex` shapes the report, not the run — and each job stands up its own Postgres
service and its own MinIO container first.

**3. `ci.yml` has no `paths:` filter at any level** (`6729fc2`). `kind.yml` already path-filters
correctly, which is exactly why `kind/cluster-tests` was the single job that correctly sat out
PR #3. Everything else ran regardless of content.

**4. `image.yml`'s PR filter includes `ui/**`** (`602940c`), and the build is a matrix over four
images × two arches — so a stylesheet edit rebuilds `scarab-clone`, `scarab-results-sidecar` and
`scarab-wsfetch`. Only `scarab-server` embeds the UI.

## The blocker that is not a simplification

`ci/coverage` is **required** and fails on `main` (`b007837`):

```
coverage ratchet: scarab-executor-local: 77.1% < baseline 80.0% - 0.5pp
```

Read out of the `coverage` job logs on `main` itself, not inferred:

| commit | run | `scarab-executor-local` |
|---|---|---:|
| `71408823` | 33171953337 | 79.0% |
| `731bc427` | 33173297743 | 79.0% |
| `6bf00bb` | 33271279118 | **77.1%** |
| `d8e748f4` | 33297280454 | **77.1%** |

Floor is 79.5% (baseline 80.0 in `docs/audits/coverage-baseline.toml`, 0.5pp tolerance). It slipped
under some time ago at 79.0%, then dropped a further 1.9pp. PR #3 reproduces the message
byte-for-byte with zero Rust changed, which is how it was confirmed inherited rather than caused.
The most recent commit touching that crate is `13de34f feat(server): add the infra observer
(ADR-0068)` — check there first.

## Work order

1. **`b007837`** — settle the ratchet. Either add tests to `scarab-executor-local` or lower the
   baseline deliberately. While `main` is red the ratchet protects nothing and the required-check
   gate is pure noise.
2. **`6729fc2`** — a `changes` job emitting `rust` / `ui` / `api`, each job gated on it, the
   newly-skippable checks moved from `required` to `required-if-run`. Expected: a UI-only PR goes
   from ~27 min / 15 jobs to roughly **1 min / 3 jobs**; a Rust PR is unchanged and still fully
   gated.
3. **`b69c286`** — decide `test` vs `coverage` *after* the filters land, because the filters remove
   the case for most PRs and leave only Rust-only changes paying double. This is a real trade:
   merging saves ~9 min per Rust PR but renames the required checks and couples the test signal to
   the coverage tooling. **Open question, deliberately not decided here.**
4. **`7fbf56d`, `602940c`** — cheap tidy-ups once the `changes` job exists; `602940c` should reuse
   it rather than adding a second filtering mechanism.

## Traps

- **Path filters can hide a real break.** `Cargo.lock`, `openapi.json`, the workflow files
  themselves, and anything the server image embeds must all count as `rust`/`api`. Getting this
  wrong is worse than the problem being solved.
- **A skipped required check hangs a real ruleset.** That is precisely why `required-if-run` exists
  in `required-checks.txt` — read its header comment before adding filters, and move the affected
  lines in the same commit as the workflow change, or `just pr-gate` will start failing on
  correctly-skipped jobs.
- **Branch protection is 403 on this repo** (private, free plan), so `.github/required-checks.txt` +
  `just pr-gate <n>` *is* the gate. There is no server-side enforcement to fall back on if the file
  drifts from the workflows.
- **`git bug bug ls` is broken here.** Enumerate with `git for-each-ref refs/bugs` and read fields
  with `git bug bug show --field <name> <id>`.
- **`git bug bug new -F <file>` ignores `-t`** and takes the file's *first line* as the title. Write
  the title as line 1 of the file, blank line, then the body. (Learned the hard way; five tickets
  were created malformed and recreated.)

## Status of the PR this came out of

PR #3 (`feat/beetle-motion-and-empty-state`) is green on `ci/test`, `ci/openapi-drift`,
`ci/ui-tests` and the advisory `ci/ui-e2e`; `kind/cluster-tests` correctly skipped. It is blocked
only by `b007837`. Its content — the dung roller's sub-cell bob, the 55° pose, and the leg-weight
work — is unrelated to any of the above.
