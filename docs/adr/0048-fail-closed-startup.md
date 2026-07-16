# 0048. Fail-closed startup: validated config, boot refusal, mandatory Postgres

- **Status:** Proposed
- **Date:** 2026-07-16
- **Deciders:** thulasi.ram (architect)

## Context

The composition root is **insecure-by-default and silent** (2026-07-16 audit):
a missing `SCARAB_MASTER_KEY` yields a random ephemeral KEK (secrets become
undecryptable after restart, no warning — `secrets-postgres/src/lib.rs:44`);
empty S3 creds via `unwrap_or_default()` (`main.rs:145-148`); no authenticator
wired, so `authorize()` grants every caller `Owner` (`lib.rs:1864-1870`); and
there is no startup validation at all. It also ships a **DB-less "API-only"
mode** that carries dead complexity (`PostgresDb::new()` unconnected +
`DbError::Unavailable` on every op, `db-postgres/src/lib.rs:41-44,62-63`) for a
mode that contradicts the durable-core wedge.

Motto: **fail fast, fail early.** Unsafe or pointless configurations should stop
the process at boot, not surface as silent facades or confusing runtime errors.

## Decision

A **validated config module** (replacing today's scattered `env::var` reads)
runs a startup gate **before the socket binds**, and prints a **startup report**
of what is enabled / disabled / degraded.

### Postgres is mandatory for every serving role — no API-only mode

Postgres is the moat and the coordination bus (CONTEXT §5); a DB-less server
contradicts the wedge. **No `SCARAB_DATABASE_URL` → refuse to boot** (any serving
role: converged/api/scheduler/executor/webhook). The unconnected-DB /
`DbError::Unavailable` code path and the API-only mode are **deleted**, not
guarded. This requirement is **not** relaxed by the dev escape hatch — even local
dev runs a Postgres.

- **Carve-out:** the `--emit-openapi` (and any pure tooling) path prints and
  exits **before** the DB check — it must stay DB-free.

### Hard-fail on enabled-but-unsafe

Refuse to boot when a feature is *enabled* but unsafely configured:

- Secrets store active but `SCARAB_MASTER_KEY` missing/invalid (kills the
  silent-random-KEK facade).
- S3 selected (bucket set) but creds empty.
- OIDC issuer enabled but no **persistent** key source (today it regenerates the
  signing key every boot → federation silently breaks on restart/replica).

### Auth default-deny = boot refusal

No authenticator wired and the dev flag **not** set → **refuse to boot** ("no
authenticator configured; wire auth or set `SCARAB_DEV_INSECURE=1`"). A server
that can authenticate no one shouldn't pretend to be up and `401` everything —
that is a confusing dead server. Boot-refusal unifies auth with the fail-closed
principle; production cannot boot auth-off by accident.

### One loud escape hatch for *security only*

`SCARAB_DEV_INSECURE=1` downgrades the **security** hard-fails (KEK, auth) to
loud boot warnings for local dev (`⚠ AUTH DISABLED — all callers are Owner`;
`⚠ EPHEMERAL SECRET KEY`). It does **not** relax the Postgres requirement. The
dev harness (`just up`/`just demo`) sets it, so local dev is unchanged. Insecure
is **opt-in and screaming**, never the silent default — the inverse of today.

## Consequences

- Deletes the API-only / unconnected-DB complexity; a validated config module
  replaces scattered `env::var` reads with one place that documents every knob.
- Production cannot boot in a silently-unsafe state; the startup report makes the
  operational posture legible on line one.
- The dev harness must set `SCARAB_DEV_INSECURE=1` (a one-line change).

## Alternatives considered

- **Graceful degradation / runtime 401 for auth-off:** leaves a useless server
  running and hides the misconfiguration. Rejected for boot-refusal.
- **Keep API-only mode (warn, don't fail):** carries dead complexity for a mode
  that contradicts the durable-core wedge. Rejected — deleted instead.
- **Separate escape hatches (auth vs crypto):** finer-grained, but a blunt
  dev-only flag is simpler; split later only if a real need appears (CONTEXT §8).
