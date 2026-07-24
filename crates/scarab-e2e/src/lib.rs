//! Intentionally empty. `scarab-e2e` is an integration-test-only crate — the
//! full-stack E2E scenarios live under `tests/`, gated on `SCARAB_E2E=1` and
//! driven by `just e2e` (which owns the proc-mode stack lifecycle). There is
//! no library surface; nothing may depend on this crate.
