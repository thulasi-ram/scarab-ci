# Handoff — the drain is the bottleneck now, and the wire is where it lives

**Branch:** `docs/adr-0061-workspace-data-path` (PR #93). **Head:** `bf5c106`, clean, pushed.
**Companion handoff:** [`adr-0066-depot-is-a-cache.md`](adr-0066-depot-is-a-cache.md) — read that one
first for the architecture; this one is the performance work that falls out of it.
**Primary ticket:** git-bug `3b734ac`.

---

## Read this first, in this order

1. **git-bug `4ce7f2c`** — the measurement. Every number below comes from it, and the benchmark that
   produced them is committed at `crates/scarab-workspace-client/tests/feed_cost.rs` (`#[ignore]`d).
2. **git-bug `3b734ac`** — the batch protocol, framing, bounds, and work order.
3. **[ADR-0066](../adr/0066-the-depot-is-a-cache.md) point 11** — why blake3 is deliberately *not*
   adopted yet, and the two triggers that would change that. Trigger #1 is reachable from this work.

---

## First principles — these decide the design

Violate one of these and the work is wrong, not merely suboptimal.

**1. The cost is per-request, not per-byte.** The cold drain moved 8.19 MB in 5360 ms. That is
1.7 MB/s over **loopback** — absurd as bandwidth, exactly right as 2000 requests at ~2.4 ms each.
Optimise request *count*. Anyone proposing a throughput fix has misread the measurement.

**2. Wire batching is not storage packing.** They are independent and have opposite reversibility.
Batching changes only the HTTP surface: no migration, no format change, revert at will. Packing
(`a0c28aa`) changes the frozen on-disk canonical form guarded by a cross-binary skew tripwire, and
needs its own ADR. **Expect a reviewer to conflate them; don't.**

**3. The integrity boundary does not move.** The Depot re-hashes every member and refuses a mismatch;
without that a client could store bytes under an address it does not own. Batching makes the envelope
bigger. It must not make verification coarser — **verify each member independently, report per
member.**

**4. Content addressing is what makes batching safe.** Every member is idempotent, so any subset can
be retried and a re-send is a no-op. There is no partial-batch *corruption* state to design around —
only a partial-batch *report*.

**5. Measure with the client and the Depot in separate processes.** The 2026-08-02 width sweep
saturated at 4× (1→1800 ms, 4→434, 16→434, 128→431) with both in **one process sharing ten cores**.
That saturation is very likely a harness artefact. **Re-sweep isolated before concluding anything
about concurrency width** — otherwise the first thing this work does is tune a benchmark.

**6. A fence token must not become an unbounded upload credential.** Bound members and bytes per
batch explicitly, in the route. Same discipline the `/v1/drains` route already applies.

**7. There is a syscall floor the wire cannot go below.** Only 434 ms of the feed's 1076 ms is
transfer; **590 ms (55%) is local filesystem writes and metadata restore.** No wire change touches
that half. State the floor rather than promising past it.

**8. `/have` already proves the shape.** The batched existence question exists and works. This
completes a half-built pattern; it does not introduce a new one.

---

## The numbers, so nobody re-derives them

| leg | 2000 files | ms/file | note |
|---|---|---|---|
| feed `materialize` | 1076 ms | **0.54** | 49 ms manifest + 434 ms GETs + **~590 ms filesystem** |
| drain `ingest` cold | 5360 ms | **2.68** | the bottleneck — 5× the feed |
| drain `ingest` deduped | 642 ms | 0.32 | the batched `/have` already earns its keep |

8.19 MB constant, uniform file sizes, release build, real router + real client over loopback, mean of
3, on an M5. **Bias: understates production** (a real network adds an RTT per object) but the
filesystem half is unaffected by that.

At 50k files: feed ≈ 27 s, cold drain ≈ 134 s.

---

## The protocol, in one screen

```
Upload — POST /v1/cas/blobs:batch     (streamed frames, read to EOF)

    [32B raw sha256][u64 len BE][len bytes] …repeat…

    200 { "accepted": ["<hex>",…],
          "rejected": [{"hash":"<hex>","reason":"digest mismatch"}] }

Fetch  — POST /v1/cas/blobs:fetch     (request: newline-delimited hex)

    [32B hash][u8 status][u64 len][bytes]   status 0 = ok
    [32B hash][u8 status]                    status 1 = not found
```

Frame budget: seal at **8 MiB or 256 members**, whichever first — bounds memory both sides, keeps a
retry cheap, stays under typical proxy body limits. Frames go N-wide concurrently exactly as single
PUTs do today. Single-object routes **stay**, for compatibility and for the image/control-plane skew
window.

**The one thing not to get wrong:** trees may share this route, but **the ledger append stays
per-tree, never per-batch.** A batch containing one foreign hash must not launder it into the fence's
ledger — that ledger is the cross-fence exfiltration fix from `e58ce1f`.

---

## Order of work

1. **Re-measure isolated** (principle 5), re-sweep width. First, or everything after it is guesswork.
2. `blobs:batch` + `blobs:fetch` with per-member results and explicit bounds.
3. Client adopts them in `wsfetch fetch` and `wsfetch drain`.
4. **Parallelise tree PUTs** — a serial `for` loop today while blobs are 16-wide. Free; same slice.
5. `CONCURRENCY` (hard-coded 16, `crates/scarab-workspace-client/src/lib.rs:64`) becomes configurable,
   informed by the re-sweep. Note it is **not** `SCARAB_CAS_CONCURRENCY` — that is the Depot's own
   object-store legs, and confusing the two will waste an afternoon.
6. Check what `reqwest`/`axum` actually negotiate. 16 HTTP/1.1 connections means 16 handshakes and 16
   congestion windows; **h2** multiplexes over one. Possibly a config-only win.
7. Re-measure in `4ce7f2c`'s shape so the numbers stay comparable.

**Sequenced after `e140121`** (resilience at Init). Batching a transport that dead-letters runs on a
replica reschedule is optimising the wrong thing first.

---

## Out of scope, with reasons

- **Storage packing** — `a0c28aa`. Separate ADR, frozen-format change, its own migration story.
- **zstd on the wire** — a *loss* on loopback, a win over a real network. Must be negotiated, and our
  loopback harness will systematically under-value it. Needs a realistic-RTT measurement first.
- **The feed's filesystem half** — parallel writes, `fchmod`/`futimens` on the open fd instead of
  re-resolving the path, folding `widen_for_the_group`'s chmod walk into the write, possibly
  `io_uring`. Real work, own ticket, and **no wire change helps it.**
- **blake3** — with an honest connection worth knowing: every byte is hashed **twice** (the client
  computes the address, the Depot verifies it, and that verify cannot go). That is ADR-0066 point 11's
  trigger #1, and it has nothing to do with laziness. If batching lands and hashing becomes the
  visible cost, that is the measurement that tags blake3 in — via the algorithm-tagged address
  convention already recorded, not a new mechanism.

---

## What "done" looks like

Batching plus parallel tree PUTs should take the drain from ~2.7 ms/file to **well under 1**. At that
point hashing and syscalls are the visible costs, and the next decision is **blake3 versus `io_uring`**
— not more wire work. If you find yourself designing a third transport optimisation, re-measure
instead.

---

## Repo rules that will cost you time if ignored

- **Never run `cargo fmt`.** `main` is not fmt-clean and even `cargo fmt -- <file>` reformats the
  workspace. Hand-format.
- **Commit messages go in a file** — `git commit -F <file>`. Backticks in a `-m` string get executed
  by zsh; it happened, and it printed the shell environment including AWS credentials into the
  transcript.
- **`kubectl config current-context` must be exactly `colima`** before any cluster command.
- **`git bug bug ls` is broken here** — enumerate via `git for-each-ref refs/bugs`; comments need the
  id *before* the flags.
- **A darwin `cargo check` never compiles `changeset.rs`.** Use `just overlay-tests-colima`, which
  runs it on a real kernel.
