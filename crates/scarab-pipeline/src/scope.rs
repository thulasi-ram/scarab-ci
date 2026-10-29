//! What a library Step's `${{ … }}` templates read (ADR-0076): its own
//! **scope**.
//!
//! A library invocation is a scope, the way a Terraform module or a Go
//! template call is. Inside a library Step, `inputs` is that invocation's
//! bound inputs, `outputs` is a view of the library's own Steps, and `matrix`
//! is the Step's own coordinate. A library never sees the caller's `matrix`
//! or the Run's launch parameters; a caller passes what a library needs
//! through `with:`.
//!
//! A `with:` value is a typed literal, or the caller's template. A template is
//! a closure: it is rendered in the **caller's** scope ([`Scope::caller`])
//! when the Step launches, then converted to the input's declared type and
//! checked against its `validate:`. Nothing here is ever pasted into another
//! template's text, and rendered output is never parsed again.
//!
//! The compiler writes a [`Scope`] on each inlined Step; the engine stores it
//! with the Step's launch spec, so a rerun, retry or restart renders the same
//! way. A Step without a scope (written in the Pipeline file itself) renders
//! as it always has: `inputs` is the Run's launch parameters, `outputs` the
//! results of its direct `needs`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ParamSpec;

/// The names a Step's templates read, and where each value comes from.
///
/// Every field is checked on read (`deny_unknown_fields`): a scope written by
/// a newer server, with a field this one does not understand, fails to load
/// instead of rendering without it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    /// What `inputs` is.
    pub inputs: ScopeInputs,
    /// What `outputs.<name>` is, for each name the scope's templates read.
    /// A name that is not here is not readable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, OutputSource>,
    /// What `matrix` is: this Step's own coordinate (for a caller scope, the
    /// coordinate of the invoke Step that wrote the `with:`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub matrix: BTreeMap<String, String>,
    /// The scope the [`BoundValue::Template`] values in `inputs` render in:
    /// the scope of the invoke Step that wrote them. `None` for a scope whose
    /// `inputs` are the Run's launch parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<Box<Scope>>,
}

/// What `inputs` means in a [`Scope`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ScopeInputs {
    /// The Run's launch parameters: the scope of a Step written in the
    /// Pipeline file.
    Run,
    /// A library invocation's inputs, each bound by the caller's `with:` or
    /// the input's default.
    Bound(BTreeMap<String, Binding>),
}

/// Where the value of `outputs.<name>` comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputSource {
    /// The results of the Step with this id.
    Step(String),
    /// An invoke's exposed outputs (ADR-0041 §2): `outputs.<name>.<out>` is
    /// the result `<out>` of the Step mapped to `<out>`.
    Invoke(BTreeMap<String, String>),
}

/// One bound input of a library invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// The input as the library declares it: its type, `options` and
    /// `validate:`, which a template's value is checked against at launch.
    pub param: ParamSpec,
    pub value: BoundValue,
}

/// The value of a [`Binding`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum BoundValue {
    /// A value known when the Pipeline compiled: a caller's literal or the
    /// input's default, already converted to the declared type and checked.
    Literal(serde_json::Value),
    /// The caller's template, rendered in [`Scope::caller`] when the Step
    /// launches. An exact `${{ e }}` gives `e`'s typed value; any other
    /// template gives text.
    Template(String),
}

impl Scope {
    /// The scope of a Step written in the Pipeline file: `inputs` are the
    /// Run's launch parameters.
    pub fn run() -> Self {
        Scope {
            inputs: ScopeInputs::Run,
            outputs: BTreeMap::new(),
            matrix: BTreeMap::new(),
            caller: None,
        }
    }

    /// This scope and each scope it was called from, innermost first.
    pub fn chain(&self) -> impl Iterator<Item = &Scope> {
        std::iter::successors(Some(self), |s| s.caller.as_deref())
    }

    /// The scope `hops` callers out from this one (`0` is this scope).
    pub fn at(&self, hops: usize) -> Option<&Scope> {
        self.chain().nth(hops)
    }
}
