//! Intentionally empty. `scarab-e2e` is an integration-test-only crate — the
//! full-stack E2E scenarios live under `tests/`, gated on `SCARAB_E2E=1` and
//! driven by `just e2e` (which brings up the kind release in its e2e mode). There is
//! no library surface; nothing may depend on this crate.
