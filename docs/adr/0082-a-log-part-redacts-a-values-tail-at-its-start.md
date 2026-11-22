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
>
> **2026-11-22, after the batch review.** The reviewer, the auditor and the red team broke §1
> as built with four sources that store bytes of a value, one of them also on main. §1 and
> the current scan decide one match at a time and skip past it, so a value that starts inside
> a starred span and ends after it keeps its end. [§3](#3-union-of-matches) replaces
> `leading_suffix`, `redact_scan` and `Scan` with one definition (the union of every match)
> and one streaming algorithm. §1's mechanism is superseded; its acceptance test stays. The
> status stays Proposed until §3 is red-teamed.

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

### 3. Union of matches

**What broke.** §1 as built, and the scan it feeds, still store bytes of a value:

| Values | Source | Stored (§1 as built) | Stored (§3) |
|---|---|---|---|
| `abcdefghijkl`, `efghijklMNOPQRST` | `efghijklMNOP`, then a confirmed end | `***MNOP` | `***` |
| `abcdefghijkl`, `ijklXYZW1234` | part starts `efghijklXYZW1234 rest` | `***XYZW1234 rest` | `*** rest` |
| `user:hunter2pw`, `hunter2pw-and-more-secret` | `dsn=user:hunter2pw-and-more-secret` | `dsn=***-and-more-secret` (also main) | `dsn=***` |
| `abcdefghijkl`, `efghijklMNOPQRST` | part starts `cdefghijklMNOPQRST` | `***MNOPQRST` | `***` |

Each comes from deciding one match and jumping past it: the scan replaces the longest value
at a position and resumes after it (row 3); the leading tail is starred and drained before
the scan sees the bytes (rows 2 and 4); at a confirmed end the tail is decided first and the
prefix rule runs on what is left (row 1). The fix is to find every match first and star
their union.

**The reference output.** For the bytes `S` of one read of a part (one redactor), the
marked intervals are:

- every occurrence of every non-empty value `v`: `[i, i+|v|)` for each `i` where
  `S[i..i+|v|] == v`;
- the leading interval `[0, n)`, where `n` is the largest `n >= MIN_SUFFIX` (8) such that
  `S[..n]` is a suffix of some value. A whole value at the start counts too; it is also an
  occurrence, so the union is the same as with proper suffixes only. `redact_text` has no
  leading interval;
- at a confirmed end only, the trailing interval `[p, |S|)`, where `p` is the smallest `p`
  such that `S[p..]` is a non-empty proper prefix of some value (any length, as the 0.7.0
  prefix rule). `redact_text` has none.

Intervals that share a byte form one span, transitively. Intervals that only touch (one ends
where the next starts) stay separate spans. The stored output is `S` with each span replaced
by one `***` and every byte outside a span copied. Touching spans stay separate because
0.7.0 stores `abcdef` with values `abc` and `def` as `******`, and a value printed twice in a
row as `******`; merging them would change the digests of ordinary logs.

**Per value set** (built once per ledger generation and kept in the `Arc<ValueSet>`):

- An Aho–Corasick automaton of the values: a trie with failure links. Each node has `best`,
  the length of the longest value that ends the node's string (0 if none), and `pend`, the
  length of the longest suffix of the node's string that is a proper prefix of some value
  (the depth of the first node on its failure chain, itself included, that has a child).
  Only the longest value ending at a position is needed: a shorter one ending there lies
  inside it.
- A suffix automaton of the values. Each state has `suffix` (its strings end some value) and
  `room` (the length of the longest path out of it: how far its strings extend inside some
  value). Its only use is the leading interval.

**Redactor state.** `k`, the bytes read; the trie node; `lead`, either pending (a suffix
automaton state and the best `n` so far) or done; `held`, the bytes `S[F..k]`; `spans`, the
known spans that lie in `[F, k)`, sorted and merged; `open`, the end `e` of a span whose
`***` was already emitted and that may still grow.

**Per byte**, at position `k` before it is read:

1. Step the trie node with the byte, `k += 1`, append the byte to `held`.
2. While `lead` is pending: step the suffix automaton from its root, never along suffix
   links. If the state exists, is a `suffix` and `k >= 8`, the best `n` is `k`. If no state
   exists or `room < max(1, 8 - k)`, no longer leading interval can match: `lead` is done
   and `[0, n)` is added if `n > 0`.
3. If `best[node] = m > 0`, add `[k - m, k)`.

Adding `[s, e)`: if `open = e0` and `s < e0`, the interval extends the emitted span: `open`
becomes `max(e0, e)`, every span in `spans` that starts before the new end joins it (and
may extend it further), and the held bytes up to the new end are dropped. Otherwise the
interval is merged into `spans`. An occurrence and the trailing interval end at `k`, after
every known span, so they merge from the back; the leading interval starts at 0, where
nothing has been emitted, so it merges from the front.

**After each push** (not per byte):

4. While `lead` is pending, nothing is emitted.
5. `h = k - pend[node]`: the earliest position where a value can start and still end after
   `k`, which is also where a trailing interval would start. Every interval found later
   starts at `h` or after.
6. If `open = e0`: when `e0 > h` nothing more is emitted, because a value starting in
   `[h, e0)` can still extend the span. Otherwise the span is closed and `open` is cleared.
7. Emit the held bytes before `h` and before the next span in clear. For a span `[a, c)` with
   `a <= h`, its start is final: emit `***`. If `c <= h` it is closed (touching does not
   merge), drop its bytes and go on. If `c > h`, drop the held bytes up to `c`, set
   `open = c` and stop. A span with `a > h` stays held: a value starting in `[h, a)` could
   still extend it to the left.

**`finish_confirmed`**: a pending `lead` is decided with the best `n` so far; the trailing
interval `[k - pend[node], k)` is added if `pend[node] > 0`; then step 7 runs with `h = k`,
so an open span closes and every held byte is emitted or starred. **`redact_text`** runs the
same steps with no leading and no trailing interval and `h = k` at the end.

**Why it stores the reference.** An interval found later is an occurrence that ends after
`k`, the trailing interval, or the leading interval (only while `lead` is pending, when
nothing is emitted). Each starts at `h` or later. So no later interval touches a byte
before `h`, a span that starts at or before `h` keeps its start, and a span that ends at or
before `h` cannot merge with a later one. What a redactor has emitted after any push is
therefore a prefix of the reference output of every continuation of the source, a confirmed
end at that point included. How the source is cut changes when bytes are emitted, never
which.

**Held bytes.** While `lead` is pending, `S[..k]` is a substring of a value with at least
one more byte inside it, so `k <= longest - 1`. After that, `held` is `S[F..k]` with
`F >= h`, and `k - h = pend[node] <= longest - 1`. So a redactor holds at most the longest
value less one byte, as today, plus the list of spans inside those bytes. The bytes of a
span are not held once its `***` is out. That is what bounds the hold when occurrences
overlap without end (value `aa` against a megabyte of `a`); holding a span until its end is
known would hold the whole run.

**Latency.** The first bytes of a part are held while a leading interval can still match,
as in §1. After that, a push whose bytes end in no value's first bytes is emitted whole, as
today.

**Cost.** Building: the trie has at most `Σ|v| + 1` nodes and the suffix automaton at most
`2·Σ|v|` states and `3·Σ|v|` transitions; transitions are kept in sorted vectors, so the build
is `O(Σ|v| · log 256)` time and `O(Σ|v|)` memory, once per generation. Per read: `O(|S|)`
amortized. A failure-link walk lowers the trie depth, which grows by at most 1 per byte, so
the walks add up to at most one step per byte across pushes. The suffix automaton takes one
step per byte for at most `longest - 1` bytes. Each interval is merged or popped once. No step
multiplies a value's length by the bytes held, which also removes the quadratic
`leading_suffix` loop the auditor found (m1).

**Dependencies.** `aho-corasick` 1.1.4 is already in `Cargo.lock` (through `regex`), but it is
not used: its `Automaton` trait has `start_state`, `next_state`, `is_match`, `match_len`,
`match_pattern` and `pattern_len`, and no state depth or failure link. `pend` cannot be read
from it, and the longest match at a state means walking all matches at that state on every
byte (values `a`, `aa`, `aaa`, … give states with many). Both automata are written in
`scarab-server` (about 200 lines, a `redact` module beside `logs.rs`), each with a test of
its per-node fields against brute force. No new crate; `Cargo.lock` does not change.

**What the Verifier needs** (`log_tail.rs`, `Verifier::check`). A re-read compares what its
redactor emits with the stored rows' digests, as a prefix: the stored part must be a prefix
of what the re-read emits. A read that broke stored only what its redactor had emitted, and
the re-read reads the same file from its first byte. Since emitted output is a prefix of the
reference of every continuation, the stored part is a prefix of the re-read's output however
either read was cut. The `***` of an open span can be stored before the span's end is read;
the re-read emits the same `***` at the same place. A re-read that ends inside the last
stored row (`ends_inside_matches`) compares a prefix of that row's body with the re-read's
output at a confirmed end, which is one of those continuations.

**What changes against the code §1 and §2 describe.** Nothing, for a source in which no two
marked intervals share a byte: secrets printed apart from each other and from the start and
end of the part, and logs with no secret. Those keep their digests. A source in which two
intervals share a byte stores one `***` for the run, where the earlier code stored a value's
end in clear (the four rows above; value `aa` on `aaa` stored `***a`) or several `***`
(values `abc`, `bcde`, `de` on `abcde` stored `******`, now `***`). Against 0.7.0 the same
holds, plus §2's change at the start of a first part. A re-read of a part stored by 0.7.0
that holds such an overlap gets one `source_restarted` note and a second copy, as in §2.
Rows already stored are not rewritten, including a value's end that 0.7.0 stored.

**Model run.** A throwaway Python model of this section (brute-force reference, the
streaming algorithm with both automata, and a port of the §1 code) agreed with the reference
on 15,000 generated sources (overlap-heavy value sets: free values, a tail of one value as
the head of another, a value inside another, a chain, a value of one repeated byte; sources
that glue values with their overlap shared, start inside a value and end inside one), each
in one chunk, one byte per chunk and four random chunkings with empty chunks, and on 20,000
more with the minimum lowered to 3 at every two-chunk cut. The hold never passed the longest
value less one byte, and output after every push was a prefix of the reference with a
confirmed end at that point. It differed from the §1 port in none of the sources without two
intervals sharing a byte. A 32,000-byte value of one repeated byte against 128,000 bytes of
it in 7-byte chunks took 224,000 trie steps (1.75 per byte).

**Rejected.**

- *Filtering candidate suffixes per value for the leading interval* (what `leading_suffix`
  does): `O(longest²)` on a value of one repeated byte, run on a tokio worker for every
  stream.
- *Capping the length of a value that is redacted*: the bytes past the cap are stored.
- *Merging touching spans*: changes the stored output and digests of ordinary logs.
- *Holding a span until its end is known*: no bound under a long run of overlapping matches.
- *Patching each break* (§1 again): every patch so far left a neighbouring shape open, and
  the reviews found the next one each time.

**Test plan for §3** (`logs.rs` tests; it replaces the redactor half of the Test plan
below, and the tail half stays):

- A brute-force `reference(values, src, lead, trail)` in the test module: compare every
  window with every value, every suffix length for the leading interval, every start for the
  trailing one, merge on a shared byte, render. No automaton.
- Fixed cases: the four rows above; `aa` on `aaa` gives `***`; `abc`, `bcde`, `de` on
  `abcde` give `***`; `abc`, `def` on `abcdef` give `******` and `tok` on `toktok` gives
  `******` (touching spans pinned); the existing redactor tests keep their expected output.
- Automaton fields: for generated value sets, every trie node's `best` and `pend` and every
  suffix automaton state's `suffix` and `room` equal brute force over the node's strings.
- Properties (values over 3 letters, 9 to 16 bytes for some so the leading interval fires;
  filler from bytes no value has):
  - glued sources (values joined with their longest overlap shared, a value inside another,
    a chain of tails and heads, a value of one repeated byte, a start inside a value, an end
    inside one), cut at random with empty chunks: the output equals the reference;
  - free values and free sources: every chunking equals one chunk equals the reference;
  - after every push, the held bytes are at most the longest value less one byte (a
    `#[cfg(test)]` accessor);
  - fed one byte per push, the output so far is a prefix of the reference of the whole source
    and of the reference of the bytes read so far with a confirmed end;
  - `redact_text` equals the reference with no leading and no trailing interval.
- Cost: a 64 KiB value of one repeated byte, and the same with a different first byte,
  against 256 KiB of that byte in 7-byte chunks stores `***` with at most two automaton steps
  per source byte, counted by a `#[cfg(test)]` counter, not timed.
- Tail (`log_tail.rs`, fake executor): a part holding two overlapping values, stored from one
  read and re-read one byte per chunk, matches its digests with no `source_restarted`.
- Docs: "Edges of a value" in `run-logs.mdx` gets a sentence that where values overlap in a
  log, the run they cover is one `***`; a CHANGELOG line.

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
