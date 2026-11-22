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
>
> **2026-11-22, after the design red team.** The red team could not separate the streaming
> algorithm from the reference (3.3M exhaustive runs, 32,000 fuzzed value sets). It found that the
> reference stored a part that lies wholly inside a value, and that the automata had no memory
> bound or build site. §3 now adds the whole-part interval, decides how and where the automata are
> built, caps a unit's values at 256 KiB (over it the log is not stored), and names the
> assumption that one read is contiguous. The cap adds a `not_stored` reason and migrations 0102
> and 0103, so §2's "No schema change" holds for §2 only. The Context's "a line over 16 KiB" is not the
> common way a part starts inside a value: a rotation between two lines of a multi-line value (a
> PEM key, a kubeconfig) is. The status stays Proposed.

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
- at a confirmed end only, the whole-part interval `[0, |S|)`, when `|S| >= MIN_SUFFIX` and
  `S` occurs in some value with at least one byte of that value after it: a part that lies
  wholly inside a value (lines 2-3 of a PEM key, the process ended). A value that began before
  the part either ends inside it (the leading interval) or runs past its end (this interval); a
  value that begins inside the part and is cut is the trailing interval. A shorter part is
  stored (R-1). `redact_text` (a failure cause) has none;
- at a confirmed end only, the trailing interval `[p, |S|)`, where `p` is the smallest `p`
  such that `S[p..]` is a non-empty proper prefix of some value (any length, as the 0.7.0
  prefix rule). `redact_text` (a failure cause) has it too: the end of a cause is its known end,
  and a cause the executor cut at 1,024 bytes ends with `…`, before which the end is taken.

Value bytes that start a part, stop before the value's end, and have more output after them are
no interval and are stored, as in 0.7.0: the part does not end inside the value, and its start
ends no value.

Intervals that share a byte form one span, transitively. Intervals that only touch (one ends
where the next starts) stay separate spans. The stored output is `S` with each span replaced
by one `***` and every byte outside a span copied. Touching spans stay separate because
0.7.0 stores `abcdef` with values `abc` and `def` as `******`, and a value printed twice in a
row as `******`; merging them would change the digests of ordinary logs. The exception is a
value whose end begins it again (a repeated word): `abab` printed twice shares bytes with the
occurrence across the join, so it is one `***` (0.7.0: `******`), and `aaaa` on nine `a` is
one `***`.

`MIN_SUFFIX` (8) is a field of the redactor, not a constant inside the rules, so tests search
small inputs with it lowered to 1, 2 or 3.

**Assumption: one read is contiguous.** Every rule assumes the bytes of one read are one
unbroken run of the container's log. If the kubelet's follow skipped bytes inside one stream (a
reader that lags across two rotations and is moved to the newest file), the first bytes of a value
before the gap would be released when the bytes after it do not continue the value, and a value's
tail after the gap is not at a part's start, so it would be stored at any length. This is not
verified against the kubelet; `Read::pump` awaits the object store between source reads, so a
slow store makes the reader lag. A gap would need a check on the stream (a later ADR), not a
redactor rule.

**Per value set** (built at most once per ledger generation, see Building the automata below):

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

**`finish_confirmed`**: a pending `lead` with `k >= MIN_SUFFIX` adds the whole-part interval
`[0, k)` (pending at the end means `S` is inside a value with at least one more byte of it after
`S`, and `[0, k)` covers the best `n`); a pending `lead` with fewer bytes adds `[0, n)` if
`n > 0`. Then the trailing interval `[k - pend[node], k)` is added if `pend[node] > 0`, and step
7 runs with `h = k`, so an open span closes and every held byte is emitted or starred. Nothing is
emitted while `lead` is pending, so the whole-part interval does not affect the prefix property. **`redact_text`** runs the
same steps with no leading interval, the trailing interval at the end (before a final `…`), and
`h = k` at the end.

**Why it stores the reference.** An interval found later is an occurrence that ends after
`k`, the trailing interval, or the leading interval (only while `lead` is pending, when
nothing is emitted). Each starts at `h` or later. So no later interval touches a byte
before `h`, a span that starts at or before `h` keeps its start, and a span that ends at or
before `h` cannot merge with a later one. What a redactor has emitted after any push is
therefore a prefix of the reference output of every continuation of the source, a confirmed
end at that point included. How the source is cut changes when bytes are emitted, never
which.

**Held bytes, after each push.** While `lead` is pending, `S[..k]` is a substring of a value
with at least one more byte inside it, so `k <= longest - 1`. After that, `held` is `S[F..k]`
with `F >= h`, and `k - h = pend[node] <= longest - 1`. So after each push a redactor holds at
most the longest value less one byte, as today, plus the list of spans inside those bytes.
During a push, `held` and the spans also hold that push's chunk (value `a` against one 8 KiB
chunk of `a`: 8,192 bytes and 8,192 touching spans until the emit); the tail reads chunks of at
most 8 KiB (`LOG_CHUNK_BYTES`), and `redact_text` and the test paths pass whole texts. The bytes of a
span are not held once its `***` is out. That is what bounds the hold when occurrences
overlap without end (value `aa` against a megabyte of `a`); holding a span until its end is
known would hold the whole run.

**Latency.** The first bytes of a part are held while a leading interval can still match,
as in §1. After that, a push whose bytes end in no value's first bytes is emitted whole, as
today.

**Cost.** Building: the trie has at most `Σ|v| + 1` nodes and the suffix automaton at most
`2·Σ|v|` states and `3·Σ|v|` transitions; transitions are kept in flat sorted arrays, so the
build is `O(Σ|v| · log 256)` time and `O(Σ|v|)` memory, once per generation. Per read: `O(|S|)`
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

**Building the automata.**

- *Lazily, where a redactor needs them.* Loading a ledger builds nothing, so a launch
  (`record_launch_values`) builds nothing. A failure cause (`ValueSet::text_automaton`) builds the
  trie only. A stream redactor builds both. The tail builds them before it opens the source.
- *Only while a reader holds them.* `ValueSet` holds each automaton through a `Weak`: a stream
  redactor or a cause being redacted holds it, and the last one to end frees it with its share of
  the budget. The value cache (600-second TTL) keeps the values, not the automata. A reload of
  the same generation with the same values shares the automata its readers hold; a new
  generation builds again. A read that breaks and is retried with no other reader builds again
  (about 100 ms at the cap).
- *A process budget and build slots.* Before it builds, a set reserves the most its automata can
  keep (`21n + 64` bytes for the trie and `31n + 64` for the suffix automaton, `n` value bytes)
  from a budget of 128 MiB per process by default (`SCARAB_LOG_AUTOMATA_BUDGET_MIB`, chart value
  `scarab.logAutomataBudgetMib`, 16 to 65536), waiting while it is full; a read that loses its tail
  lease while it waits stops (checked every 200 ms and again when it gets room) and reads nothing; a stream reserves both in one wait, so two readers
  never each hold half of what they need. Then it takes one of 2 build slots
  (`CONCURRENT_BUILDS`), which bounds the build's temporary memory (about 36 MiB each at the cap)
  to 2 builds. A read waits for room and is never read without its automata. At the cap a unit
  reserves 13 MiB, so at the default budget 9 units at the cap are read at once and the 10th waits for one to end.
  The process keeps at most the budget of automata and 2 × 36 MiB while building.
- *Off the async workers.* A build of over 16 KiB of values runs on a `spawn_blocking` thread,
  under 16 KiB inline. A build that does not finish (a panic, the runtime shutting down) is an
  error: the read fails and is retried, and nothing is built on an async worker in its place.
- *A keyed hash.* The suffix automaton build's map from `(state, byte)` to edge hashes the key
  xor a random per-process seed, so a value's author cannot pick bytes whose keys collide.
- *Flat arrays, no recursion.* Each automaton is a few `Vec`s: an offset per node or state, one
  array of edge bytes and one of targets, and the per-node fields. No node owns a heap block. The
  trie is built from the sorted values (each node's children come in byte order, so a counting
  sort by parent gives each node's edges sorted) and its failure links breadth first. The suffix
  automaton is built with one edge array, a list of edges per state (to copy a state when it is
  cloned) and one hash map from `(state, byte)` to edge, then frozen into sorted arrays and the
  map dropped. `room` is filled in decreasing state length (a transition goes to a longer state),
  by a counting sort, without recursion.
- *Build traps* the red team listed, kept: `suffix` is marked after the whole build, by walking
  each value and its suffix-link chain (a clone a later value makes can sit on an earlier value's
  chain); the generalised construction keeps the branch for a transition that already exists (a
  value that is a prefix of an earlier one); a merge from the back pops every span the new
  interval reaches, and one from the front absorbs every span that starts before the growing end;
  an interval extends the open span only when `s < open`, and a span closes at `c <= h`.
- *Size.* Kept: the trie takes 21 bytes per node (offset, edge byte and target, failure link,
  `best`, `pend`; every array allocated at its length), the suffix automaton 8 bytes per state (offset, `room` with `suffix` in its top
  bit) and 5 per transition. At most 52 bytes per value byte (2 states and 3 transitions per
  byte); measured with the model on 64 KiB of random bytes, base64, hex and PEM lines, 1.17 to
  1.35 states and 2.17 to 2.34 transitions per byte, about 44 bytes per value byte. While the
  suffix automaton is built, its edge lists and hash map take up to about 100 more bytes per value
  byte, dropped when it is frozen.

**A cap on a unit's values: 256 KiB, fail closed.** `MAX_VALUE_BYTES` is 256 KiB of value bytes
per unit (every distinct value, added up). At 52 bytes per value byte that keeps the automata of
one unit under 13 MiB, and the build under about 36 MiB while it runs; the process budget above
bounds the sum over units. A PEM key is about 3 KiB,
a kubeconfig under 10 KiB, the Mozilla CA bundle about 220 KiB. A unit over the cap is never
redacted in part (*Capping the length of a value that is redacted* is rejected below):

- the tail checks the size before it opens the source and closes the stream with one
  `not_stored` marker, reason `values_too_large`: "the rest of this log was not stored: this
  step's secret values add up to more than 256 KiB, more than a log is checked against". No
  source byte is read. It is the 0.7.0 fail-closed path for a unit without a complete ledger
  (`close_not_stored`), with a new reason. Values only grow across generations, so a unit that
  passes the cap stays over it, and every replica finds the same;
- `redact_text` gives `CAUSE_NOT_STORED`, as for a unit with no ledger;
- `LogService::append` and `pump_log_stream` (tests and tools) return an error.

The reason is a new value of the `log_chunks_reason` check, so migration 0102 drops the check and
adds it again `NOT VALID` (no scan under its ACCESS EXCLUSIVE lock), and migration 0103 validates
it again in its own transaction (sqlx runs one per migration), under SHARE UPDATE EXCLUSIVE, so
0101's validation (64bc5c6) holds at head. A rollback to 0.7.0 deletes versions 101 to 103 from
`_sqlx_migrations`; a 0.7.0 replica reads `values_too_large` as a `not_stored` marker with no
reason it knows and shows the `no_ledger` text. `rollback/0099_revert.sql` deletes versions 102
and 103 too.

**What the Verifier needs** (`log_tail.rs`, `Verifier::check`). A re-read compares what its
redactor emits with the stored rows' digests, as a prefix: the stored part must be a prefix
of what the re-read emits. A read that broke stored only what its redactor had emitted, and
the re-read reads the same file from its first byte. Since emitted output is a prefix of the
reference of every continuation, the stored part is a prefix of the re-read's output however
either read was cut. The `***` of an open span can be stored before the span's end is read;
the re-read emits the same `***` at the same place. A re-read that ends inside the last
stored row (`ends_inside_matches`) checks that what it emitted for that row, at its own
confirmed end, is a prefix of the row's body. That is the reverse of the prefix property: the
row came from a longer source, and the re-read's output with its trailing `***` can differ from
it (stored `tokio`, a re-read that ends at `tok` emits `***`). Then the result is `Mismatch`: a
`source_restarted` marker and a second copy, never a stored value byte.

**What changes against the code §1 and §2 describe.** Nothing, for a source in which no two
marked intervals share a byte: secrets printed apart from each other and from the start and
end of the part, and logs with no secret. Those keep their digests. A source in which two
intervals share a byte stores one `***` for the run, where the earlier code stored a value's
end in clear (the four rows above; value `aa` on `aaa` stored `***a`) or several `***`
(values `abc`, `bcde`, `de` on `abcde` stored `******`, now `***`). Against 0.7.0 the same
holds, plus §2's change at the start of a first part. A re-read of a part stored by 0.7.0
that holds such an overlap gets one `source_restarted` note and a second copy, as in §2.
Rows already stored are not rewritten, including a value's end that 0.7.0 stored.

**Mixed versions.** During a rolling upgrade, or after a rollback, a 0.7.0 replica that takes the
tail lease of a part this version stored with an overlap finds a mismatch and stores its own copy
with the 0.7.0 scan, after a `source_restarted` marker, which is the output that keeps a value's
end. The lease can move back and forth, one marker and one copy per move. Overlapping values are
redacted in full once every replica runs this version; the CHANGELOG says so.

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

**Test plan additions after the design red team** (built with §3):

- The whole-part interval in the brute force; an interior part of 8 or more bytes at a confirmed
  end is `***`, one of 7 bytes is stored; lines 2-3 of a PEM-like value.
- Generators: alphabets of 1 and 2 letters and ones with `*` and `\n`; free values, prefix
  chains, suffix chains, repeats of a word, infixes of one value, lengths `MIN_SUFFIX - 1`,
  `MIN_SUFFIX`, `MIN_SUFFIX + 1` and 1, rotations; the minimum lowered to 1, 2 and 3 as well as 8.
- After every push of every chunking (not only one byte per push): the output is a prefix of the
  reference of the whole source and of the bytes read so far with a confirmed end, it equals what
  one byte per push emitted after the same bytes, and the held bytes are at most the longest value
  less one byte.
- An exhaustive pass with the minimum at 2: every set of one or two values of 1 to 4 bytes over
  `ab` against every source of up to 8 bytes over `ab`, one byte per push, one push and a middle
  cut.
- Fixed pins: `ab`, `abX`, `cd` fed `ab` then `cd` is `******`; `XXXXabcdefgh`, `tok` on
  `abcdefghtok!` is `******!`; `tok`, `abZ` on `tokab` is `******`; `XXabcdefghij`, `bc`, `ef`,
  `ijKL` on `abcdefghijKL.` is `***.`.
- Tail: a read broken with a span open stores a row that ends inside the `***` and no byte of the
  span, and the re-read in other chunks matches with no `source_restarted`; a read broken while
  the lead is pending stores nothing; a first part that is lines 2-3 of a value; a unit over the
  cap gets one `values_too_large` marker and its source is not opened.
- Build: per value byte at most 2 states, 3 transitions and 52 kept bytes, for random bytes, a
  repeated byte, `abb…bc` and many short tokens, at 256 KiB (which also shows the build uses no
  recursion); a launch builds no automaton, a failure cause builds the trie only, a reload of
  the same generation shares the automata its readers hold, and the last reader frees them and
  their budget; a build waits for room in the budget and for a build slot (polled, no sleeps).

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
