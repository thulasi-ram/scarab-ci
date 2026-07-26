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
