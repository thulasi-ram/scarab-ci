# Handoff — ADR-0062 s4 wired; ADR-0063/0064/0065 decided and unbuilt

**Written:** 2026-07-28
**Repo:** `/Users/thulasiram/workspace/personal/scrarab-ci` (product/crates/CLI are all **`scarab`**; only the directory is `scrarab-ci`)
**Branch:** `docs/adr-0061-workspace-data-path` — **everything is pushed**, 0 unpushed commits, tree clean
**PR:** https://github.com/thulasi-ram/scarab-ci/pull/93 (open, base `main`) — carries all of it
**Tracker:** git-bug, **all 208 bugs pushed to origin**

---

## Read this first, in this order

1. `docs/adr/0062-workspace-export-lazy-without-node-driver.md` — the Export design. Long, and you need it.
2. **`docs/adr/0063-step-logs-on-the-data-depot.md`**, **`0064-durability-tiering-and-the-write-path.md`**, **`0065-retention-cache-and-rederivation.md`** — decided this session, **none of them built**.
3. `docs/adr/0061-workspace-data-path.md` — **part 4 and the retention table are both amended in place** (marked, with reasoning). Read the amendments, not just the originals.
4. `CONTEXT.md` §4.1–4.4 — new terms: **Data Depot**, **RetentionProfile**, **Step logs**, `outputs:`, and a **Cache** entry that now describes something we intend to build.

`just adr0062-substrate` re-asserts the 16 kernel/PodSecurity facts ADR-0062 rests on and fails loudly naming the section to rewrite. **Run it first** — two minutes, and with GitHub Actions out of credits it is one of the few gates that still runs.

---

## The one thing that will bite you

**ADR-0062 s4's code is wired, green, and encodes a write path ADR-0064 supersedes.**

`crates/scarab-server/src/workspaced.rs` settles through `TieredCas`, which writes **cold first, per blob**, and its `reingest_cold_then_warm` deliberately mirrors that ordering. ADR-0064 replaces it with **warm-first + one batched flush**. So the code is correct against the *old* decision and stale against the *new* one. It is not broken — nothing regressed — but do not read the passing tests as agreement with the ADRs.

git-bug **`ac21752`** is that slice. Do it early; everything else in 0064 sits behind it.

---

## State

**Committed and pushed (12 commits this session).** `git log --oneline -12` tells the story; the messages are long on purpose.

| | state |
|---|---|
| Farm lease seam (`lease`/`holders`/`evict`/`FarmLease`, + `release_lease`/`all_leases`/`sweep_residue`) | **done**, 28 tests |
| change-set → CAS fold (`settle.rs`) | **done**, 11 tests |
| Export lifecycle + capability fence (`export.rs`) | **done**, 32 tests |
| Export routes on the service, startup sweep, reaper (`workspaced.rs`) | **done**, and see the warning above |
| ADR-0063 / 0064 / 0065 | **written, zero code** |
| The rename to **Data Depot** | **decided, not started** |

**Verification at the last code commit:** `cargo test -p scarab-server --lib` **178 passed**; `cargo clippy -p scarab-server --lib` adds no new warnings; `just adr0062-substrate` 16/16.

**Not run this session:** the full `just test`, `just coverage`, and the `test-orchestrator` gate. I deliberately held the gate until the composed slice existed, then the design moved under it. **Run `test-orchestrator` before trusting the s4 suite.**

---

## What was decided this session, and why it is not relitigable

Each of these came out of a long grill; the ADRs carry the reasoning. Summarised so you do not re-derive them:

- **Snapshot durability did not change.** `TieredCas` already wrote cold-first and fatally (`tiered.rs:250`: *"Cold first: this is the leg that licenses `Succeeded`"*). What changed is the **write path** (warm-first + batched flush, still gating `Succeeded`) — faster, same guarantee.
- **"Durable" is a property of the backing, not the tier's name.** Test is `st_dev` of the cold dir against the warm dir. A second PVC counts; a `LocalDir` beside the CAS does not. *"Is it object storage?"* is wrong in both directions.
- **Warm-only deployments are supported**, loudly, with the weaker guarantee **stamped on the Attempt** — a startup log cannot explain a Run a month later.
- **ADR-0061 part 4 is amended, not overturned.** The invariant is *never put a claim in the record the record cannot back*; cold-before-`Succeeded` was a mechanism for it. Its separate prohibition on tiering onward **asynchronously** still stands, and 0064 does not do that.
- **Logs move to the Depot, not the CAS**, and are the **only** class that cannot be re-derived. External systems are additional sinks, never the system of record.
- **The eviction pin belongs to logs, not snapshots.** For snapshots eviction is recoverable; for logs it is final.
- **No `Corrupted` Attempt state.** ADR-0061's widened rerun already covers evidence loss; the Attempt genuinely succeeded and its *evidence* is what is gone.
- **No purity property in the engine.** Re-derivation is human-triggered, so nothing re-runs a `push` autonomously. The hazard survives as a **disclosure** duty: the affordance must name the whole cascade.
- **No automatic Cache detection** (`165c2dc` closed). `target/` is Cache-shaped or an output depending on the next Step — behavioural, not nameable.
- **Checkout is not a special case.** Per-file merkle CAS means source dedupes across commits, so it is cheap to keep *and* cheap to re-derive; build output is expensive at both.
- **Data plane implicit-inherit default stays.** The "fresh workspace per Step, explicit outputs" model was the architect's first preference and was **rejected on the governing principle** — it taxes authors with substrate knowledge. Recorded in 0065's alternatives; the case for it is real, so read that section before reopening it.

---

## Open tickets from this session

Filed and pushed. Suggested order top to bottom.

| id | what |
|---|---|
| `ac21752` | **ADR-0064 s1** — warm-first + one batched flush; supersede `TieredCas` cold-first. **Do first**; the s4 code depends on the outcome |
| `981fc6b` | ADR-0064 s2 — `st_dev` cold-tier proof, graceful warm-only, durability stamped per Attempt (new column) |
| `f10a566` | ADR-0063 s1 — the **rename** to Data Depot. One commit, no intermediate state with both names. Operator-facing breaking |
| `72dfe6f` | ADR-0063 s2 — logs off the object store onto the Depot |
| `e2fdbde` | **ADR-0062 bug** — `settle_change_set`'s future is not `Send`; the fold runs on a blocking thread via `spawn_blocking` + `block_on`. One-line-ish fix in `settle.rs` |
| `8035498` | ADR-0062 — the stat-cache capture instant is not persisted, so a Depot restart degrades the re-ingest drain to "trust nothing" |
| `1c49660` | ADR-0062 — Export routes accept **any** valid workspace token, so one Step can drive another's Export |
| `dbe05e5` | ADR-0065 s1 — implement **Cache** (explicit, keyed, directory not mount) |
| `82c5775` | ADR-0065 s2 — `RetentionProfile`; factor the profile machinery out of ADR-0055 rather than copying it |
| `4afaa3e` | ADR-0065 s3 — widened-rerun affordance names the whole cascade |

**Still open from before, unchanged:** `cba7165` (warm space bound — the *lease* half is done, the bound is not), `0ad393c` (**the overlay rung has never executed** — see below), `99a63d8` (NFS client per Step node), `65d7d85` (`fsGroup` vs NFS), `690b81a` (revocation/`ESTALE`/`nfsdcld`), `20a908a` (fence weaker than "capability"), `6d5deb8` (zone preference vs ADR-0055), `1a9df08` (warm gauge help text), `0a0f972` (identity walk cost), `94e0345` (reflink Export rung), `119af26` (change set drops metadata), `71d3d59` (third copy of the mtime conversion), `0ea908b`/`955e597`/`b5e5d40` (s6/s7/s9).

---

## The honesty item that matters most

**The exact change-set path has never executed.** Every green test exercises the **copy** rung, whose change set is the `(size, mtime, ctime)` approximation. The **overlay** rung — the only configuration where the change set is *exact*, and the entire basis of ADR-0062 part 3's argument — has run nowhere: the dogfood node has no Rust toolchain, Actions is quota-blocked, darwin cannot mount `overlayfs`. This is written into ADR-0062's Open section and tracked as `0ad393c`.

Treat "the change set is exact" as **designed and unverified**, not as working.

---

## How to work here

**Act as a thin orchestrator.** Do not read large amounts of code yourself. Delegate, take conclusions, keep your context for decisions. What worked:

1. **Recon agent** (`Explore`) with numbered questions, a word limit, `file:line` anchors, conclusions not code dumps.
2. **Implementation agents in parallel**, each with an explicit **file-ownership allowlist** ("EDIT ONLY these paths; another agent works in parallel; read freely"). Declare shared module registrations *yourself* first — I added `pub mod export;`/`pub mod settle;` in its own commit before fanning out, for exactly this reason.
3. **A report-only reviewer over the combined diff.** It found six confirmed defects the implementers' own reports had missed, including the worst one this session.
4. **Fix agents**, on disjoint files.
5. **`test-orchestrator`** last, as the gate. *(Not run this session — do it.)*

**Tell every agent, every time:** never run `cargo fmt` in any form; do not run the full suite; do not commit; **prove every test can fail by removing or inverting the logic it asserts, and report the exact mutation.**

### Verify agents rather than believing them

**Do not trust "all mutations killed".** I independently re-applied one mutation per agent's module and they held — but two agents' *first* attempts had holes they only found by being pushed:

- A **thread-race test never reached its own window** — 64 rounds of `spawn` versus `evict`, zero overlaps. It passed whether or not the guard existed, which is worse than no test. Replaced with a `#[cfg(test)]` hook that hits the interleaving exactly. See `farm.rs`'s module docs.
- A test **helper wrote `record["version"] = OLDEST_READABLE_RECORD`**, so narrowing the constant moved the fixture with it and the test could not disagree with the code. Pinned to a literal. That is the **third** time in this ADR's history a helper restating the code under test hid a hole.
- **`fs::copy` on macOS is `copyfile(COPYFILE_ALL)`** and carries timestamps — so a mutation deleting an explicit mtime restore left the suite **green** on darwin while the restore is load-bearing on Linux. The agent fixed the *code* (stream into a fresh `File::create`), not the test.

---

## Traps that cost real time

- **A probe proves what it exercised, not what it suggests.** I burned an hour on this: I read the chart's `emptyDir` cold-tier *fallback* and concluded deployments use it, which nearly amended a core invariant. `deploy/local-helm/deploy.sh` defaults `S3_BUCKET` and stands up MinIO on a PVC; `just up` runs MinIO under compose. **Check the deploy scripts, not just the templates.**
- **Two slices can each be correct and jointly wrong.** The reviewer found `SettleInputs` prescribing `read_change_set` for *both* rungs — but on the copy rung `upper/` is the whole workspace, so `deleted` is always empty and **every file the Step deleted silently reappears**. Same shape as the `redirect_dir` defect this repo already shipped. Fixed by making the wrong pairing unrepresentable (`SettleDrain`), not by documenting against it.
- **`tracing` caches callsite `Interest` process-wide**, so a log-capture assertion silently observes nothing if another thread registered the callsite first. Symptom: passes under `nextest` (process per test), fails under `cargo test --lib` at ≥8 threads. `export.rs` has a working warm-up precedent.
- **`git bug bug ls` is broken here.** Enumerate with `git for-each-ref refs/bugs`, read with `git bug bug show --field title|status <id>`. `git bug bug new -F -` **silently ignores `-t`** and takes the first body line as the title.
- `rg` is rewritten to `grep` by a shell hook and rejects `--glob`; use `rtk proxy grep …` for unfiltered output.
- **`kubectl config current-context` must be exactly `colima`** before any cluster command. Production EKS contexts sit beside it.
- **GitHub Actions is out of credits.** `just pr-gate 93` reports all required checks red regardless. Not a signal; do not chase it.

---

## Repo rules — non-negotiable

- **NEVER run `cargo fmt`.** `main` is not fmt-clean and `cargo fmt -- <file>` reformats the whole workspace regardless of arguments. Hand-format. If you slip: `git checkout HEAD -- .` and redo by hand.
- **Use the `just` recipes**, never raw `docker`/`kind`/`kubectl`/`helm`. If a recipe is missing, **add one**.
- **A live-tier test that can silently not-run is as severe as a wrong assertion.** Fixtures must `panic!` when the tier is opted in, never `else { return }`.
- Testing is **classical, not mockist** (`CONTEXT.md` §8, ADR-0017) plus the feature-acceptance addendum.
- **Commit in logical units, frequently.** End messages with
  `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`
- The pre-existing Helm dogfood stack on colima **is not yours — leave it.** `just down` runs `compose down -v`.

---

## Suggested skills

- **`/grill-with-docs`** — the architect prefers it over freehand design and it produced this session's three ADRs. **Prefers prose questions over `AskUserQuestion` menus** during design discussion (standing preference). 0063/0064/0065's decisions are settled; do not re-grill them.
- **`test-orchestrator`** (agent type) — the gate, every time. **Owed for s4.**
- **`/tdd`** — for `981fc6b`'s durability stamping and `1c49660`'s token scoping, where failure modes are security- and evidence-shaped.
- **`/diagnose`** — if an Export misbehaves at the filesystem layer; those failures are mount- and permission-shaped, not logic-shaped.
- **Skip `/gortex-*`** — the MCP server is inactive for this directory.

---

## First move

1. `just adr0062-substrate` — two minutes, confirms the design's foundations still hold.
2. Read the three new ADRs and ADR-0061's two amendments.
3. Run **`test-orchestrator`** over the s4 code, since I did not.
4. Then `ac21752` (warm-first + batched flush), because the s4 write path is stale against it and everything else in 0064 queues behind it.
