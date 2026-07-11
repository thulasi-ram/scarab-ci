# 0028. UI stack: SolidJS + TS + generated client

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

A CI UI is **graph-heavy and real-time** — its hardest surfaces are the live DAG
visualization, log tailing, and the time-travel timeline scrubber. The best DAG-layout/graph
libraries (cytoscape, reactflow, elkjs/dagre) are JS-native. The dogfooding rule
([0012](0012-api-surface.md)) holds regardless: the UI consumes the same REST/OpenAPI + SSE.

## Decision

**SolidJS + TypeScript + a generated OpenAPI client.** Fine-grained reactivity (near-Leptos,
ideal for SSE-driven live updates), full access to the JS graph-viz ecosystem, lean; the
**generated client kills type drift** (the usual argument against a JS frontend). The one
argument for full-Rust (Leptos) — shared types — is neutralized by codegen, while Solid keeps
the mature viz ecosystem the DAG needs.

## Consequences

- A TS toolchain alongside Rust (mitigated by codegen from the OpenAPI the server emits).
- Best-in-class DAG/timeline UX, the thing that differentiates us from crude CI UIs.
- Roadmap Slice 6.

## Alternatives considered

- **Leptos (full-Rust + WASM)** — ultimate no-drift/shared-types, but DAG-viz is DIY + thinner
  ecosystem.
- **React + TS** — biggest ecosystem, heavier + less reactive for live streams than Solid.
- **htmx / server-rendered** — simplest, but fights an app-like interactive DAG/timeline UX.
