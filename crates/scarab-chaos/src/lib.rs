//! The fault tier's harness (ADR-0077 §2, §3): a seeded nemesis, replayable
//! scenario files and the invariant checker. `just test chaos` drives the binary
//! against the kind release in its chaos mode (deploy/kind/chaos.sh).

pub mod check;
pub mod env;
pub mod nemesis;
pub mod pods;
pub mod scenario;
pub mod time;
