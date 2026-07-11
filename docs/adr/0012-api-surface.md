# 0012. API surface: REST/OpenAPI + SSE + internal gRPC

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

"API-first, UI dogfoods the API" only counts if the UI eats the **same** API third parties
(CLI, automation, webhooks, bots) eat. So we must pick **one** primary API that serves both a
rich nested UI and casual third-party automation. GraphQL suits the UI but is a poor lingua
franca for CLIs/curl/webhooks; gRPC-first needs grpc-web proxying and is hostile to casual
consumers. REST is the universal lingua franca and good enough for the UI with `expand`/`include`.

## Decision

- **REST/OpenAPI is the single primary, versioned, dogfooded API** (`/v1`). **Code-first**
  via `axum` + `utoipa`: Rust types are the source of truth, OpenAPI is generated, TS (UI) +
  Rust (CLI) clients are generated from it. The **pipeline resource schema *is* the IR**
  ([0009](0009-dsl-ir-yaml-cel.md)).
- **SSE** for server→client streams (live logs, run/step status, live DAG) — simpler than
  WebSockets, auto-reconnect, HTTP/2-friendly. WebSockets reserved for a future interactive
  step terminal.
- **gRPC internal-only** — control-plane ↔ executor ↔ (future) remote agent, where typed
  streaming + performance matter and there is no browser.

## Consequences

- True dogfooding: UI + integrators share REST; codegen kills client drift.
- One type system from IR → API → generated clients.
- Real-time is covered without dragging integrators into GraphQL.

## Alternatives considered

- **GraphQL primary** — great for UI, worse for CLI/webhooks; forces integrators into GraphQL.
- **gRPC-first everywhere** — grpc-web proxy + weak public-API ergonomics.
