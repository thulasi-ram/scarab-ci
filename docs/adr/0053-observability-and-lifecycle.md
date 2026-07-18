# 0053. Observability & lifecycle: metrics, structured logs, readiness, graceful shutdown

- **Status:** Accepted
- **Date:** 2026-07-17
- **Deciders:** thulasi.ram (architect)
- **Implements:** [0030](0030-operational-defaults.md) (operational defaults)

## Context

The server is operationally blind (2026-07-16 audit): no metrics (no
metrics/Prometheus dep anywhere), bare `tracing_subscriber::fmt::init()` with no
`EnvFilter`/JSON/request-ids, `/healthz` is a static string that ignores DB
health (readiness == liveness), and there is **no graceful shutdown** (the driver
`JoinHandle` is discarded; SSE and in-flight work are cut on SIGTERM).

## Decision

- **Metrics:** a Prometheus **`/metrics`** endpoint (pull; k8s-native,
  ServiceMonitor-ready). Key series: runs/steps by status, admission latency,
  outbox depth, log-tail lag, GC swept/retained. OTel export deferred (can layer
  on the same instrumentation later).
- **Structured logging:** `tracing-subscriber` **JSON** formatter + `EnvFilter`
  (honor `RUST_LOG` properly) + a **request-id / trace middleware** so every log
  line and response correlates.
- **Health split:** **`/healthz`** = liveness (cheap process-up); **`/readyz`** =
  readiness, checks **DB + object-store reachability**. The chart's readiness
  probe points at `/readyz`, liveness at `/healthz` (today both hit the static
  `/healthz`).
- **Graceful shutdown:** a SIGTERM handler + `axum::with_graceful_shutdown` that
  **drains in-flight SSE** and **stops the driver cleanly** (hold its
  `JoinHandle`, signal it, await the current tick). Safety still rests on
  crash-idempotency, but a clean drain avoids torn work on every rollout.

## Consequences

- A metrics crate + instrumentation; tracing config; `/readyz` checks; shutdown
  wiring; chart probe split + a ServiceMonitor.
- The operational posture becomes legible (dashboards, alerts) and rollouts stop
  cutting live connections.

## Alternatives considered

- **OpenTelemetry now:** richer/portable, but heavier for v1; Prometheus pull is
  the k8s-native default and the same instrumentation can export OTel later.
- **Keep `/healthz`-only:** leaves k8s unable to tell "up" from "can serve" — a
  server with a dead DB stays in rotation. Rejected.
