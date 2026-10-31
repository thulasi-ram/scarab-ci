//! # scarab-policy — the policy engine (ADR-0075)
//!
//! Pure crate: no I/O, no clock, no RNG.
//!
//! - [`policy`]: the authored Policy: checkpoint, typed `match`, mode, and
//!   CEL rules or one typed setting.
//! - [`ir`]: Policy IR and [`compile`](ir::compile), which validates a
//!   Policy for its level and type-checks its CEL.
//! - [`evaluate`](mod@evaluate): the evaluator, producing a
//!   [`PolicyDecision`](evaluate::PolicyDecision) with its PolicySetDigest.
//! - [`store`]: the `PolicyStore` port.
//! - [`library`]: the `LibraryStore` port (ADR-0076) and the yank
//!   PolicyInvariant.
//! - [`schema`]: a checkpoint's typed input schema, generated from the Rust
//!   types that build the input ([`policy_struct!`], [`policy_enum!`]).
//! - [`run_create`]: the `run.create` checkpoint's input.
//! - [`check`]: the type checker over the AST `cel` 0.15 parses, with the
//!   `has()` guard warning, literal regexes and the static cost bound. It and
//!   every evaluation run on a large-stack thread.
//! - [`eval`]: binding a checkpoint input as CEL variables and evaluating
//!   one checked `when` with `cel` 0.15 (`matches()` compiled once per
//!   pattern per evaluation); [`evaluate`](mod@evaluate) is its caller.

pub mod check;
pub mod eval;
pub mod evaluate;
pub mod ir;
pub mod library;
pub mod policy;
pub mod run_create;
pub mod schema;
pub mod store;
