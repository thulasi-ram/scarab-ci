//! # scarab-policy — ADR-0075 spike: a type checker for a CEL subset
//!
//! Pure crate: no I/O, no clock, no RNG. It holds what the spike in ADR-0075's
//! implementation horizon (step 1) asks for, and nothing of the engine yet:
//!
//! - [`schema`]: a checkpoint's typed input schema, generated from the Rust
//!   types that build the input ([`policy_struct!`], [`policy_enum!`]).
//! - [`run_create`]: the `run.create` checkpoint's input.
//! - [`check`]: the type checker over the AST `cel` 0.15 parses, with the
//!   `has()` guard warning, literal regexes and the static cost bound.
//! - [`eval`]: evaluation with `cel` 0.15, any error denying.

pub mod check;
pub mod eval;
pub mod run_create;
pub mod schema;
