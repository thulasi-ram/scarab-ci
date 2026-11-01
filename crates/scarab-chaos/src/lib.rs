//! The fault tier's harness (ADR-0077 §2, §3): a seeded nemesis (ranks 1, 2
//! and 4), replayable scenario files and the invariant checker. `just test chaos`
//! drives the binary against the kind release in its chaos mode
//! (deploy/kind/chaos.sh).

pub mod cache;
pub mod check;
pub mod env;
pub mod nemesis;
pub mod node;
pub mod pods;
pub mod scenario;
pub mod servers;
pub mod time;
