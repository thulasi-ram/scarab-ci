# 0082. A log part redacts a value's tail at its start

- **Status:** Proposed (owner rule for the log and redaction path: this design is red-teamed
  before it is built)
- **Date:** 2026-11-22
- **Deciders:** thulasi.ram (architect), pending review
- **Builds on:** [0013](0013-history-and-observability.md) (log tail),
  [0051](0051-multi-replica-operation.md) (tail lease), the 0.7.0 log design (CONTEXT.md
  **Log stream**, **Log marker**, **Value ledger**)
- **Tickets:** git-bug 2599a7b (closed by §1); fa9c02b (the `start_unread` note) stays open,
  see [Deferred](#deferred-the-start_unread-note-fa9c02b)

> **2026-11-22, after the red team.** The first draft of this ADR was titled "A late first
> read of a log gets a note" and decided a probe of the first log entry's time and a new
> `start_unread` marker kind. The red team found that the suffix rule it relied on stores a
> value's tail depending on how the source is chunked (2599a7b), and that the note needed a
> second probe design, a migration pair, a rolling-update rule and a rollback file. This
> ADR now decides the redaction fix and the suffix rule on every part, which need no schema
> change. The note and the red team's required changes to it are under Deferred.

## Context

The tail reads a container's log from its first byte on every read (`follow_params`,
`LogFrom::Start` only, `scarab-executor-k8s`) and compares what it reads with the stream's
**current part**, the output rows after the newest `source_restarted` row. A difference
stores a `source_restarted` note and the whole read again (`log_tail.rs`, `PumpEnd::Mismatch`).

The kubelet serves only the current log file. When it rotates the file (at
`containerLogMaxSize`, 10 MiB by default) while no read is attached, the next read starts in
the new file. If the part already holds rows, the compare fails and the note is written. If
the part holds no output row, there is nothing to compare with, and the log starts at the
oldest byte the kubelet kept, with no note. `run-logs.mdx` states this limit.

**The suffix rule.** `StreamRedactor` writes `***` for the longest leading proper suffix of
a ledger value, 8 bytes or longer (`MIN_SUFFIX`), at the first byte of a part. It is on
only for a part after a `source_restarted` row: `read_stream` (`log_tail.rs`, about line
845) calls `Read::pump(chunks, values, part, part_start > 0)`, `part_start` is the
`part_start_seq` of the newest `source_restarted` row (0 for the first part), and `pump`
passes that flag to `StreamRedactor::new(values, after_restart)`, which sets
`suffix_pending`. A first part that starts in a rotated file can begin inside a value (a
line over 16 KiB that the runtime wrote as several entries, with the rotation between
them). That tail is stored today.

**The chunking bug (2599a7b).** `leading_suffix` (`crates/scarab-server/src/logs.rs`, line
866) returns `Undecided` only while a longer *suffix* could still match. Its test for "a
longer whole value at the start wins" is `buf.starts_with(v)`, which is false while `buf` is
shorter than `v`. With V1 = `abcdefghijkl` and V2 = `efghijklMNOPQRST`, a part that starts
with V2 delivered as `efghijkl` then `MNOPQRST`:

1. push 1: V1's 8-byte suffix `efghijkl` matches, no longer suffix can, V2 is not yet a full
   match, so it returns `Len(8)`; `***` is emitted and `suffix_pending` turns off;
2. push 2: `MNOPQRST` is no value and is stored in clear.

The stored part is `***MNOPQRST`, the last 8 bytes of V2. Delivered as one chunk, the same
source stores `***`. The output depends on chunking, so a re-read can also differ from the
stored digests and store a `source_restarted` note and a second copy. 0.7.0 has this bug
after a `source_restarted` row. Turning the suffix rule on for every part without fixing it
would make it reachable at byte 0 of every log.

## Decision

### 1. `leading_suffix` waits while a longer whole value could still match

When the source has not ended (`at_end` false), `leading_suffix` returns `Undecided` while
any value `v` has `v.len() > buf.len()` and `v.starts_with(buf)`, in addition to its current
test for a longer suffix. The redactor holds the bytes and decides on the next push. At a
confirmed end (`finish_confirmed`, `at_end` true) no more bytes come, so it decides with the
bytes held. The held bytes are at most the longest value less one byte, as for the rest of
the scan.

In the example above, push 1 holds `efghijkl` because V2 starts with it; push 2 sees all of
V2, the whole value is longer than the 8-byte suffix and wins, and `***` is stored, as for
one chunk.

**Acceptance test:** a property test in `logs.rs` tests (proptest is already a workspace
dependency; `scarab-server` gains it as a dev-dependency) over generated value sets and
sources, including value pairs where an 8-byte or longer tail of one is the head of
another: every split of the source into chunks emits the same bytes as the whole source in
one chunk, with the suffix rule on and off. The 2599a7b two-chunk case is a fixed test
beside it. 2599a7b closes when this test passes.

### 2. The suffix rule applies at the start of every part

Every redactor starts with the suffix rule on. `StreamRedactor::new` loses its
`after_restart` flag and sets `suffix_pending` to true; `LogService::redactor` and
`Read::pump` lose the same parameter, and `read_stream` stops passing `part_start > 0`. The
two other callers follow the same rule, so tests exercise what production runs:
`pump_log_stream` (`log_tail.rs`, about line 1422, for tests and tools) and
`LogService::append` (`logs.rs`, about line 580, not a production path).

Whether a read began at the container's first byte no longer affects what is stored. Every
read of a part uses the same rule, so with §1 its digests compare equal across reads.

**No schema change.** The rule lives in the redactor's state (`suffix_pending` and the held
bytes). No row kind, column, migration, OpenAPI field or marker text changes. A rollback to
0.7.0 needs no revert step.

## Consequences

- A first part that starts inside a value stores `***` for its tail when the tail is 8 bytes
  or longer. A shorter tail is still stored, as after a `source_restarted` row today
  (`MIN_SUFFIX`).
- A container whose output begins with 8 or more bytes that end a value gets `***` there.
  Before, only a part after `source_restarted` did. It tells a reader that some value ends
  with those bytes, as every `***` tells a reader where a value was.
- The first bytes of a part can be held until the next chunk shows whether a longer value
  matches, so the first line of a live log can appear one chunk later.
- After the upgrade, a re-read of a first part stored by 0.7.0 differs only when that part
  begins with 8 or more bytes that end a value. It then gets one `source_restarted` note and
  a second, redacted copy. Nothing new is stored in clear.
- A log whose start was rotated away before any read stored it still has no note.
  `run-logs.mdx` keeps that limit, and fa9c02b stays open for the note.

## Rejected options

- **Turn on §2 without §1.** It moves the 2599a7b leak from a part after `source_restarted`
  to byte 0 of every log.
- **The suffix rule only on a first part a probe marks as late.** What is stored would then
  depend on a probe that misses some rotations and fires without one (Deferred, items 10
  and 11).
- **Build the note now, in this ADR.** The red team's list below changes the probe, the
  migrations, the upgrade and the rollback; the redaction fix does not wait on it.

## Test plan

No network.

- **Redactor** (`logs.rs` tests): the 2599a7b two-chunk case stores `***`; the property test
  of §1; a part that begins with V1's 8-byte suffix and no longer value still gets `***`; a
  held start released at `finish_confirmed` with only a suffix match gets `***`.
- **Tail** (`log_tail.rs` tests, fake executor): a first part whose source begins with the
  last 12 bytes of a ledger value stores, and the raw download returns, a part that starts
  with `***` and never holds those bytes; a re-read of that part matches its digests (no
  `source_restarted`); a first part that begins with no value's tail stores its bytes
  unchanged.

## Deferred: the `start_unread` note (fa9c02b)

The first draft wrote a `start_unread` marker before the first output row of an empty first
part when a probe showed that the oldest kept log entry was written more than 10 seconds
after the container's `startedAt`. Its text: "lines before this point may be missing: the
server first read this log after the kubelet could have rotated it". With §2 the note does
not affect what is stored. A later ADR that builds it starts from this list.

1. **The probe reads bytes, not text.** Not `Api::logs`: kube-client 4.0 `request_text`
   decodes the body with `String::from_utf8`, a byte limit can cut a multi-byte character,
   and `FromUtf8Error`'s `Debug` prints the bytes, which puts log bytes into a trace. Use
   `log_stream` / `request_stream` with `timestamps=true&limitBytes=36` (a 35-byte RFC 3339
   time with an offset, and a space), find the first `0x20`, and parse only the bytes before
   it as ASCII, accepting offsets other than `Z`. The method returns `Option<Timestamp>` or
   an error with no fields.
2. **A failed probe means no note.** A decode or parse failure is the same on every read of
   that file. It is never a broken read that is retried, or the log would not be stored.
3. **Order in the read.** The probe runs after `load_values` returned a ledger, with the
   same `scarab.io/value-ledger` label and Pod uid check as `open_ledger_log`
   (`scarab-executor-k8s`), so no source byte is read of a unit without a ledger. A probe
   404 on a final read of a Pod that is gone takes the same branch as a failed open
   (`gone_unconfirmed` / `close_incomplete`, `check_budget`), so the stream still closes.
4. **No `startedAt`, no note.** `container_log_state` has no `started_at` for a container
   with no status yet, a waiting container, and `Restarting`. For `Restarting` the kubelet
   serves the previous instance's file, so there is no probe.
5. **Same container on both sides.** `pods/log` names no container, so the read is state,
   probe, state, with the same container id and `restartCount` on both sides. A missing id
   on either side means no note.
6. **One clock.** Drop the `T1 < O` condition, which compares the node's clock with the
   server's. Keep only "first entry more than 10 seconds after `startedAt`"; both are node
   time.
7. **Rolling update.** A 0.7.0 replica reads an unknown kind as output
   (`LogRowKind::parse(..).unwrap_or_default()`, `scarab-db-postgres/src/lib.rs` line
   6456): it serves the note as an empty output chunk, and its `Verifier` finds a mismatch
   and stores the log a second time. Either the release needs `Recreate` (a Breaking changes
   entry), or the reader ships one release before the writer. Separately, an unknown kind is
   read as a marker with a generic text, never as output; that change can ship first.
8. **Two migrations, widen first.** Migration 0101 validated the four-kind
   `log_chunks_kind` check. Widening it is one migration (`DROP CONSTRAINT` and `ADD ... NOT
   VALID`, with `SET LOCAL lock_timeout`), and `VALIDATE` is a later one, never in the same
   transaction: there the scan would run under the `ALTER`'s ACCESS EXCLUSIVE lock.
9. **A revert file.** Going back to the release before the note needs its own file: delete
   `start_unread` rows, restore the four-kind check, delete the new version rows, plus a
   CHANGELOG paragraph. `rollback/0099_revert.sql` already deletes versions 99 to 101; the
   new numbers are chosen when the note is built.
10. **Known misses to document.** A burst, a rotation, then a quiet step: the new file is
    empty at the probe, so no note; later lines give the part rows and the probe never runs
    again, and the first 10 MiB is gone with no note. This is the ticket's own case.
11. **False positives to document by the rule, not by one cause.** The note fires when the
    first part is empty, the first kept entry is more than 10 seconds after `startedAt`, and
    a read opens after it. That includes the 45-second tail lease after its holder is killed
    with a second replica up, the 3-second backoff after the 5-minute read timeout on a
    quiet step, a kube API error on open, an object store outage (every append fails and
    each retry probes), and a ledger that could not be loaded on this replica. None of them
    stores a secret.
12. **Tests the draft lacked.** A body whose 36th byte is inside a UTF-8 sequence; an
    unlabelled Pod makes no probe; a final read of a gone Pod with a probe 404 closes the
    stream; a state with no `started_at`; the row parser on an unknown kind; the revert file.
    The live case depends on `just test chaos`, which runs in no CI workflow (dbb1d7c).

Options the first draft already rejected for the note: a note on every first read more than
30 seconds after the container started (fires after every server restart and on every
5-minute re-read of a quiet step); reading the main stream with `timestamps=true` (a
timestamp lands inside a line over 16 KiB, and every stored digest changes); reading
`containerLogMaxSize` (Scarab cannot read kubelet config); node stats through `nodes/proxy`
(reaches the kubelet API on every node); a note on every first part; reusing
`source_restarted` (its text says the source restarted and lines may repeat).
