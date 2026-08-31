# Handoff — CI does too much on every push

**Branch:** `docs/ci-simplification`.
**Tickets:** git-bug `b007837`, `6729fc2`, `b69c286`, `7fbf56d`, `602940c` (all `type:chore`, `area:infra`; each carries the same cross-reference comment).
**Evidence PR:** [#3](https://github.com/thulasi-ram/scarab-ci/pull/3) — a UI/art-only change, which is what made the waste legible.

---

## Outcome (2026-08-31)

Four of the five are done on `docs/ci-simplification`; the fifth is still the
open trade this document parked.

| ticket | outcome |
|---|---|
| `b007837` ratchet red on main | **fixed, with no baseline edit** — `scarab-executor-local` is 90.4% against its unchanged 80.0 baseline (floor 79.5). The drop was five never-tested behaviours; five acceptance tests cover 11 of the crate's 83 lines and take it 64/83 → 75/83 |
| `6729fc2` no path filter | **fixed** — `.github/path-filters.yml` + a `changes` job in `ci.yml`; nothing is filtered on a push to `main` |
| `7fbf56d` duplicate vitest job | **fixed** — `ui-unit` deleted, `ui-tests` kept (it is the name in `required-checks.txt`) |
| `602940c` all four images rebuilt | **fixed** — `image.yml`'s matrix is now selected per-image from the same filter file; all four still build on push/tag |
| `b69c286` Rust suite runs twice | **fixed** — merged into one `ci/test-and-coverage` job; the `test` job is deleted |

Three things this document got wrong or did not see, corrected in the code:

1. **`openapi-drift`'s 0.8 min was not waste.** Its second half runs
   `npm run gen && npm run typecheck`, which makes it the ONLY TypeScript
   typecheck in CI. Filtering it to Rust-only would have dropped `tsc` from
   every UI PR. Its filter is `rust ∪ ui ∪ openapi.json`.
2. **`pr_gate.py` passed a skipped `required` check at any tier.** A
   workflow-level `paths:` filter makes a check *absent*; a job-level `if:`
   makes it *skipped*, and only the first was treated as "did not run". Adding
   filters without fixing this would have made the whole tier decorative.
   Skipped and missing are now the same verdict, and `ci/changes` — which
   always runs — is `required`, so a PR cannot pass having validated nothing.
3. **`just coverage` did not mirror the CI `coverage` job** despite saying so:
   no MinIO, no S3 env. It is the recipe that *regenerates the baseline*, so a
   silently narrowed run there writes a permanently lowered floor. It now starts
   both services and sets `SCARAB_TEST_REQUIRE_*`.
4. **The whole baseline is loose, and that is the more interesting bug.** The
   re-measurement put most crates well ABOVE their committed floor —
   `scarab-storage-s3` +15.7pp, `scarab-secrets-postgres` +14.2,
   `scarab-forge-github` +10.9, `scarab-server` +10.5 — which lines up exactly
   with the missing-MinIO drift in (3). A floor 15pp low does not catch a
   regression. It is **not** regenerated in this change: raising a floor from a
   laptop run reds `main` on whatever the two environments disagree about.
   `coverage` now uploads its lcov `if: always()`, so the next green run on main
   yields numbers worth committing. See the note in
   `docs/audits/coverage-baseline.toml`.

The `if: always()` matters on its own: the upload step sat *after* the ratchet
step, so the lcov was published only on green runs — never on the run whose
numbers you need. `b007837` had to be reconstructed by reading percentages out
of four job logs by hand because of it.

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

1. ~~**`b007837`** — settle the ratchet.~~ **DONE**, by the first branch: tests, not a lowered
   baseline. The 1.9pp drop was two `Ok(None)` evidence-port stubs added by `13de34f` and `a8ade2e`
   (`infra_condition`, `workspace_provisioning`) landing untested; going after them surfaced that
   the crate had never tested its three *launch rejections* either — the clone / build / sidecar
   contracts whose whole job is to refuse rather than run a step without the thing that makes it
   that kind of step. Those are the tests that were missing, and the coverage number was a symptom.
2. ~~**`6729fc2`**~~ **DONE.** A `changes` job emitting `rust` / `ui` / `coverage` / `contracts`
   from `.github/path-filters.yml`, each job gated on it, the newly-skippable checks moved to
   `required-if-run` in the same commit, plus a new always-runs `ci/changes` at `required` as the
   anchor. Two deviations from the plan above: `api` became `contracts` and *includes* `ui`
   (the typecheck, see finding 1), and **a push to `main` filters nothing** — trunk is what every
   `sha-` image derives from, and the minutes were being burnt on PR pushes anyway.
3. ~~**`b69c286`**~~ **DONE — merged into one job**, `ci/test-and-coverage`. The trade turned out
   not to be a trade. The case for keeping a separate `test` was insurance against a
   cargo-llvm-cov / `llvm-tools-preview` breakage taking the test signal with it — but
   `ci/coverage` was *already* `required`, so such a breakage already blocked every merge whether
   or not `test` sat beside it green. The suite was being run twice to buy a hedge that did not
   exist. `cargo llvm-cov nextest` exits non-zero on a red suite, so the merged job reports a
   strict superset of what `test` did.

   Two measured facts settled it (`--ignore-run-fail` was the crux):

   | invocation with one red test | result |
   |---|---|
   | plain `cargo llvm-cov nextest` | exit 100, **no lcov written at all** |
   | `--ignore-run-fail` | exit **0**, full lcov, `LF:83 LH:75` — identical to the green run |

   So the job MUST NOT carry `--ignore-run-fail`, and the workflow comment says so at the point of
   temptation. (It also refutes the guess that a red run would distort the ratchet: the flag
   implies `--no-fail-fast`, so every test still executes and still covers its lines.)

   `just test` keeps its plain un-instrumented `cargo nextest` — the local inner loop should not
   pay for instrumentation. This change is about CI paying twice, not about how you run tests.
4. ~~**`7fbf56d`, `602940c`**~~ **DONE.** `ui-unit` deleted. `image.yml` selects its build matrix
   per-image from the same filter file — and its workflow-level `pull_request: paths:` was
   *removed* rather than kept, because that was the second filtering mechanism: an outer union
   that has to track four inner filters, and it was already out of step (it never listed
   `rust-toolchain.toml`, which both cargo-built images read).

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
