# 0022. Upgrades & schema evolution: version-tolerant from day one

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

If upgrading the control plane breaks in-flight runs, "durable" is a lie. And `gate`
([0008](0008-step-contract.md)) is a durable suspend that can wait **weeks** — so a gated run
is *guaranteed* to outlive many deploys. Resume-across-upgrades is therefore the **normal
case**, not an edge case, which rules out drain-then-upgrade. Version tolerance is cheap now
and brutal to retrofit (you cannot version history you did not version).

## Decision

**Version-tolerant from commit one:**

- **Versioned, append-only event log + versioned IR, upcast-on-read.** Version stamps and
  immutability discipline exist from day one; upcasters/archival tooling come later.
- **Self-describing runs:** each Run stores `{ir_version, event_schema_version}`. The engine
  advertises a supported-version *window*; a run outside it is explicitly **parked** with a
  diagnostic, never silently corrupted.
- **Expand-contract (parallel-change) migrations:** every migration is backward-compatible so
  old+new binaries coexist during a rolling deploy; breaking changes are multi-phase (add →
  dual-write → backfill → switch → drop). Forward-only; CI tests old-binary×new-schema and
  new-binary×old-schema overlap.
- **Version-tagged work claiming:** a work item carries the engine-version range that can
  handle it; a worker only claims compatible work (mixed versions coexist during rollout).

## Consequences

- Rolling, drain-free deploys; gated runs survive arbitrary upgrades.
- Slightly more schema/event ceremony from the start — deemed mandatory.
- Underpins the "survives its own deploys" credibility of the wedge.

## Alternatives considered

- **Basic migrations now, versioning later** — gate-outlives-deploy bites early; painful retrofit.
- **Drain-based deploys** — impossible with weeks-long gates; contradicts the wedge.
