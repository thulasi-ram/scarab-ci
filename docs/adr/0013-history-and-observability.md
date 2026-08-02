# 0013. History & observability: state tables + append-only event log

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

The durable core needs an append-only record serving four consumers: crash recovery,
time-travel UX (the visible wedge payoff), immutable audit, and real-time SSE. The question is
how much event-sourcing to adopt, given we already committed to a transactional outbox
([0003](0003-durability-substrate-postgres.md)).

## Decision

**State tables are the source of truth (queryable current state); every transition is also
appended as an immutable, versioned event via the outbox.** That one event stream drives SSE,
the timeline, audit, and enough time-travel — without full event-sourcing (projections,
event-schema replay-to-read complexity).

Implementation defaults:

- **Logs:** step stdout/stderr streamed live to SSE **and** persisted to **object storage**
  (chunked, compressed) with only a per-step byte-offset **index** in Postgres. Never store
  log bodies in Postgres.

  > **Amended 2026-08-03 by [0066](0066-the-depot-is-a-cache.md).** *"Never store log bodies in
  > Postgres"* becomes **"bodies never, EXCEPT as the bounded fallback when no durable object store
  > exists."** The rule was written to prevent **log bloat** in the durable core, and that concern is
  > real and unchanged — but [0063](0063-step-logs-on-the-data-depot.md) establishes that logs are the
  > **only** class with no recompute path, and 0066 makes object storage a **soft, recommended**
  > requirement (git-bug `42d997c`). Guarantee what cannot be recreated; degrade what can. With no
  > object store, the alternatives are a volume that dies with a Pod or nothing at all, and both lose
  > the one class whose loss is final.
  >
  > **The exception is bounded on four axes, and each one is load-bearing:**
  >
  > - **Compressed** — chunks are gzipped before they reach the row, as they already are on the object
  >   store path.
  > - **Size-capped** — per Attempt, so one runaway Step cannot fill the durable core. Truncation must be
  >   **LOUD** per [0063](0063-step-logs-on-the-data-depot.md) part 5: a truncated log says it was
  >   truncated, never quietly ends.
  > - **Time-partitioned** — bodies live in their own partitioned table so retention is a **partition
  >   drop**, not a mass `DELETE` with its vacuum bill. This is what keeps "bounded Postgres growth"
  >   true rather than aspirational.
  > - **`STORAGE EXTERNAL`** on the body column — watch **TOAST double-compression**: the default
  >   `EXTENDED` will try to compress bytes that are already gzipped, burning CPU on both write and read
  >   for no gain and occasionally growing them.
  >
  > **Object storage remains the recommended sink and the default where configured.** This is the
  > fallback, not the new normal, and a deployment using it should be able to tell.
- **Retention/GC:** configurable TTL per run/artifact/log/cache, background durable GC
  sweeper + object-store lifecycle rules ([0030](0030-operational-defaults.md)).
- Events carry a `version` field (upcast-on-read) per [0022](0022-upgrades-and-versioning.md).

## Consequences

- Time-travel and audit for free from the event stream; SSE is a tail of it.
- Bounded Postgres growth (no log bloat).
- Not full ES — replay-to-rebuild-state is out of scope; state tables remain authoritative.

## Alternatives considered

- **Pure event sourcing** — maximal replay, but projection lag + schema-evolution machinery;
  over-built for a bounded state machine.
- **State + minimal audit log only** — weak time-travel; wedge stays invisible.
