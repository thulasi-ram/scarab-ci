# 0029. Workspace content-addressing: per-file merkle CAS

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

We committed to content-addressed workspaces ([0004](0004-execution-topology.md),
[0007](0007-data-passing-model.md)), and smart invalidation ([0027](0027-restart-semantics.md))
*depends* on how fine-grained the hashing is. A tar-snapshot per step re-transfers the entire
workspace (`node_modules`, `target/`) on any one-file change and has no cross-step/run dedup.

## Decision

**Per-file merkle CAS** (git/Nix/BuildKit-style): hash each file, build a directory merkle
tree, store files individually keyed by content hash; a snapshot = the **root tree hash**.
Downstream materializes by pulling only files it lacks.

- **Dedup** across steps *and* runs; **incremental transfer** (only changed files move);
  snapshotting is just hashing.
- This is the exact substrate skip-if-unchanged and smart-invalidation need.
- **v1 pragmatism:** a plain tar-snapshot is acceptable as a throwaway for the Slice-1
  walking skeleton, upgraded to merkle CAS in Slice 2 when multi-step passing appears.

## Consequences

- Fast "restart build, only changed files flow, downstream skips if identical."
- A CAS store + local node cache + GC to build (in `scarab-storage` + `scarab-storage-s3`).

## Alternatives considered

- **Tar-snapshot per step** — trivial, coarse, no dedup; weak content-addressing story.
- **Content-defined chunking (restic-style)** — best sub-file dedup, most complex; later
  optimization for large-binary workspaces.
