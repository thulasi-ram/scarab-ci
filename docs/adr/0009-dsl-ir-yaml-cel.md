# 0009. DSL: typed Pipeline IR + YAML frontend + CEL

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

Every YAML-native CI grows a messy embedded expression language (GHA's `${{ }}`), because
YAML has no types, functions, or submit-time validation. Every "just use a real language"
system (Dagger/Pulumi-style) trades away the forge-centric "commit a file, done" simplicity
and forces a toolchain.

## Decision

Avoid the syntax argument by separating the semantic core from the surface syntax:

1. **The real DSL is a typed, versioned Pipeline IR** — a JSON-Schema/serde-backed data model
   the engine consumes. This is the artifact we design and version (`ir_version`).
2. **YAML is the default human frontend** that deserializes into the IR, **validated at
   submit time** (not run time).
3. **All dynamic expressions use CEL** (Common Expression Language): *total* (always
   terminates), typed, sandboxed, k8s-ecosystem-standard, Rust implementations exist. It
   avoids GHA's Turing tarpit. Used for `when:`, interpolation, matrix predicates,
   trigger filters.
4. **Multi-frontend by construction:** because the engine speaks the IR, CUE/Starlark/code
   frontends can be added later without making one meta-language official.

## Consequences

- The **API pipeline schema *is* the IR** ([0012](0012-api-surface.md)); the **UI reads/writes
  the same IR**, so the UI dogfoods the API with one type system end to end.
- Early, schema-driven validation; surface syntax can evolve without engine churn.
- The real design work is the IR schema plus a deliberately *small* CEL binding.

## Alternatives considered

- **Code-first SDK (Dagger/Pulumi)** — the strongest typing, but "pipeline = program with a build
  step"; heavier than the 80% want. May become a *frontend* later.
- **CUE as primary** — powerful, steep curve/small community.
- **Plain YAML + `${{ }}` templating** — familiar, but rebuilds the mess we're avoiding.
