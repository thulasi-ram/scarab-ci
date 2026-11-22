# 0082. A late first read of a log gets a note

- **Status:** Proposed (owner rule for the log and redaction path: this design is red-teamed
  before it is built)
- **Date:** 2026-11-22
- **Deciders:** thulasi.ram (architect), pending review
- **Builds on:** [0013](0013-history-and-observability.md) (log tail),
  [0051](0051-multi-replica-operation.md) (tail lease), the 0.7.0 log design (CONTEXT.md
  **Log stream**, **Log marker**, **Value ledger**)
- **Tickets:** git-bug fa9c02b; the chaos check in dbb1d7c can assert the note

## Context

The tail reads a container's log from its first byte on every read (`follow_params`,
`LogFrom::Start` only, `scarab-executor-k8s`) and compares what it reads with the stream's
**current part**, the output rows after the newest `source_restarted` row. A difference
stores a `source_restarted` note and the whole read again (`log_tail.rs`, `PumpEnd::Mismatch`).

The kubelet serves only the current log file. When it rotates the file (at
`containerLogMaxSize`, 10 MiB by default) while no read is attached, the next read starts in
the new file. If the part already holds rows, the compare fails and the note is written. If
the part holds **no output row**, there is nothing to compare with, and the log starts at the
oldest byte the kubelet kept, with no note. `run-logs.mdx` states this limit today.

That case needs 10 MiB written before the first row of the part is stored: a fast writer
while the tail starts, or a time when no replica could read (all replicas down, a ledger that
could not be loaded). The kubelet's log API says nothing about rotation, and Scarab has no
node access.

There is a redaction gap beside it. The **suffix rule** (`StreamRedactor`, `after_restart`)
writes `***` for a leading proper suffix of a value (8 or more bytes) only after a
`source_restarted` row. A first part that starts in a rotated file can begin inside a value
(a line over 16 KiB split by the runtime, with the rotation between the halves), and that
suffix is stored today.

Two facts make a better signal than wall time. Container `startedAt` (Pod status, second
precision) and the timestamp the runtime writes on each log entry (`timestamps=true`) both
come from the node's clock. And the tail is started at launch and retries every 3 seconds
until the container runs, so in normal operation it opens within a few seconds of `startedAt`.

## Decision

### 1. The suffix rule applies at the start of every part

The redactor is built with the suffix rule on for every part, the first one included, for
every read of it. Whether a read began at the container's first byte no longer affects what
is stored, and the note below is informational only: a wrong decision about the note, either
way, cannot cause secret bytes to be stored. Every read of a part uses the same rule, so its
digests compare equal across reads.

### 2. A probe of the first entry's time

A new `Executor` method returns the timestamp of the first entry in the container's current
log file, or none when the file is empty. The k8s executor reads
`pods/log?container=<c>&timestamps=true&limitBytes=64` (not followed), parses the text before
the first space as RFC 3339, and drops the rest of the bytes inside the method. Log bytes
never cross the port: not in the result, not in an error, not in a trace. A body that does
not parse is reported as "the first entry's time could not be read" and nothing else.

`LogSourceState::Running` and `Ended` gain `started_at` (the container's `startedAt`).

### 3. When the note is written

On a read of the stream's **first part** (no `source_restarted` row yet) that has no output
row and no `start_unread` note, before the main read opens, the tail reads the source state
(S = `started_at`), takes the probe time O, and probes (T1). It writes the note when both hold:

- T1 is earlier than O: the entry was in the file before this read, and no read stored it.
- T1 is more than 10 seconds after S: the oldest entry the kubelet kept was written well after
  the container started.

The note is written before any output row of the read, fenced on the tail lease like every
row, at most once per stream. When the state names another container than the probe saw, or
the probe fails with a source error, the read is a broken read and is retried, as a failed
open is today.

It is **not** written:

- when the part holds an output row (the compare decides, as today);
- after a `source_restarted` row (that note already says lines may be missing);
- when the file is empty at the probe (a quiet step's re-read after the 5-minute read
  timeout, or a tail waiting for the first line);
- when the first kept entry is within 10 seconds of the start (the normal case after a server
  restart: early output is still in the file);
- when the first entry's time cannot be parsed (logged as a warning).

### 4. The note

A new marker kind `start_unread`, text:

> lines before this point may be missing: the server first read this log after the kubelet could have rotated it

It needs a migration that widens the `log_chunks_kind` check (coordinate with 64bc5c6,
which validates that check), the `LogRowKind` and `marker_text` entries, the OpenAPI marker
kind, the web UI note row, `scarab logs`, and the `[scarab] <text>` raw-download line. A
server older than the migration refuses to start on the newer schema, so no old reader sees
the kind (an old reader would parse it as output).

## Consequences

- A log whose start was rotated away before any read stored it says so, in the common cases.
- The suffix rule now covers the first part. A container whose output begins with 8 or more
  bytes that end a secret value gets `***` there; before, only a later part did.
- One extra small `pods/log` request per read of an empty first part.
- Known misses, documented in `run-logs.mdx`: a rotation within the first 10 seconds of a
  container (10 MiB in that time, with no read attached) gets no note; a step that prints
  nothing for its first 10 seconds and whose first read opens after its first line (all
  replicas down) gets a note when nothing was lost.

## Rejected options

- **A note on every first read more than 30 seconds after the container started** (the
  triage proposal). It fires after every server restart of a step with no stored row and on
  every 5-minute re-read of a quiet step, because those parts are empty too.
- **Read the main stream with `timestamps=true`.** The kubelet prefixes every runtime entry,
  partial entries included, so a timestamp lands inside a line over 16 KiB. A value split
  there would no longer match unless the prefixes were stripped exactly, and the stored bytes
  and digests of every in-flight stream would change.
- **Use `containerLogMaxSize`.** Scarab cannot read kubelet config, and the bytes a read
  returns do not tell whether the file was rotated.
- **Node stats (`/stats/summary` log bytes) through `nodes/proxy`.** That permission reaches
  the kubelet API on every node, too much for a log note, and the numbers are not exact.
- **A note on every first part.** Every log would start with it.
- **Reuse `source_restarted`.** Its text says the source restarted and lines may repeat;
  neither holds at the start of a log.

## Test plan

No network; the executor parse is tested on recorded kubelet output.

- **Executor:** the probe parses `2026-11-22T10:00:15.123456789Z hello` to that time; an empty
  body is none; a body that does not parse gives the fixed error, and a test asserts that a
  secret-looking body appears in neither the error nor the result. `container_log_state`
  carries `started_at` from recorded running and terminated Pod statuses.
- **Tail** (`log_tail.rs` tests, fake executor and clock):
  - empty first part, T1 = S + 60 s, T1 < O: one `start_unread` row, before the first output
    row;
  - T1 = S + 1 s: no note; empty file at the probe, then output: no note; two re-reads of a
    quiet step: no note;
  - a second read of the same part after the note: no second note;
  - a part after `source_restarted`, and a part with output rows: no probe made (the fake
    counts calls);
  - a source that begins with the last 12 bytes of a ledger value, with and without the note:
    the stored part and the raw download start with `***` and never hold those bytes; a re-read
    of that part matches its digests (no `source_restarted`);
  - a container id change between state and probe: the read is retried, nothing stored.
- **Migration:** the check accepts `start_unread`; `?markers=none` leaves the note out.
- **Live:** the log case in `just test chaos` (dbb1d7c) scales the server to zero while a step
  writes more than `containerLogMaxSize`, then back: the log starts with the note and the
  secret value is in neither the raw download nor the stream.
