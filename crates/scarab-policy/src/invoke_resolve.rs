//! The `invoke.resolve` PolicyCheckpoint's input (ADR-0075 §2): consulted at
//! compile time, once per `invoke`.
//!
//! `tags` and `version` are not in the ADR's abridged table. The spike adds
//! them to exercise a capped list (for `exists`/`all` and the cost bound) and
//! an optional field inside a struct (for `has()`).

use crate::schema::{Capped, Schema, Timestamp};
use crate::{policy_enum, policy_struct};

pub const CHECKPOINT: &str = "invoke.resolve";
pub const SCHEMA_VERSION: u32 = 1;

/// The most tags a library or plugin may declare.
pub const MAX_TAGS: usize = 32;

policy_enum! {
    /// Where an `invoke` target comes from (ADR-0038).
    pub enum TargetKind {
        Local => "local",
        Library => "library",
        Plugin => "plugin",
    }
}

policy_struct! {
    pub struct InvokeTarget {
        pub kind: TargetKind,
        pub name: String,
        pub content_hash: String,
        /// A published version; a `local` target has none.
        pub version: Option<String>,
        pub tags: Capped<String, MAX_TAGS>,
    }
}

policy_struct! {
    pub struct Invoke {
        pub target: InvokeTarget,
    }
}

policy_struct! {
    pub struct Project {
        pub name: String,
    }
}

policy_struct! {
    pub struct Environment {
        pub name: String,
    }
}

policy_struct! {
    /// The root: each field is a CEL variable.
    pub struct InvokeResolveInput {
        pub invoke: Invoke,
        pub project: Project,
        /// Absent when the Pipeline names no Environment.
        pub environment: Option<Environment>,
        pub time: Timestamp,
    }
}

pub fn schema() -> Schema {
    Schema::for_root::<InvokeResolveInput>(CHECKPOINT, SCHEMA_VERSION)
}
