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

use crate::{cel, ParamSpec, ValueSource};

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

/// A `when:` that decides whether an inlined Step or gate runs, evaluated when
/// the Run is created (ADR-0076): the library Step's own `when`, and the
/// `when` of each invoke Step it was inlined through. Each reads the scope it
/// was written in, named by its distance from the Step's own scope, so the
/// two never share text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guard {
    /// Where the expression was written, in callers out from the Step's
    /// [`scope`](crate::StepSpec::scope): `0` is the library Step's own
    /// `when`; `n` is the `when` of the invoke Step `n` levels out, read in
    /// that invoke Step's scope.
    pub caller_hops: usize,
    /// The CEL expression.
    pub when: String,
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

    /// Mutable form of [`at`](Self::at).
    pub(crate) fn at_mut(&mut self, hops: usize) -> Option<&mut Scope> {
        let mut cur = Some(self);
        for _ in 0..hops {
            cur = cur.and_then(|s| s.caller.as_deref_mut());
        }
        cur
    }
}

/// Where each value a scope's templates read through `inputs` comes from
/// ([`ValueSource`]), for the checks that treat an outside value differently
/// from the Pipeline's own: the script check at compile and the leading-`-`
/// check when an argument renders.
#[derive(Debug, Clone)]
pub(crate) enum InputSources {
    /// `inputs` are the Run's launch parameters: every one is from outside.
    Run,
    /// A library invocation's inputs, each by name. A literal `with:` value
    /// or a default is the Pipeline's own; a caller's template is from
    /// outside when what it reads, in its caller's scope, is.
    Bound(BTreeMap<String, ValueSource>),
}

impl InputSources {
    /// The sources of `scope`'s inputs (`None`: a Step written in the
    /// Pipeline file). Built from the outermost caller in, once per scope, so
    /// the work is the size of the chain's bindings, however the templates
    /// read each other.
    pub(crate) fn of(scope: Option<&Scope>) -> Self {
        let Some(scope) = scope else {
            return InputSources::Run;
        };
        let chain: Vec<&Scope> = scope.chain().collect();
        let mut caller: Option<InputSources> = None;
        for s in chain.into_iter().rev() {
            let here = match &s.inputs {
                ScopeInputs::Run => InputSources::Run,
                ScopeInputs::Bound(bindings) => InputSources::Bound(
                    bindings
                        .iter()
                        .map(|(name, b)| {
                            let source = match &b.value {
                                BoundValue::Literal(_) => ValueSource::Pipeline,
                                // A template with no caller scope fails at
                                // launch; until then it is from outside.
                                BoundValue::Template(t) => match &caller {
                                    Some(c) => c.of_template(t),
                                    None => ValueSource::Outside,
                                },
                            };
                            (name.clone(), source)
                        })
                        .collect(),
                ),
            };
            caller = Some(here);
        }
        caller.unwrap_or(InputSources::Run)
    }

    /// Where the value of the expression `expr` comes from: from outside
    /// when any read is. `matrix` is the Pipeline's own; `outputs` (a Step's
    /// results), `event` and any name not known here are from outside, and
    /// so is an expression that does not parse.
    pub(crate) fn of_expr(&self, expr: &str) -> ValueSource {
        let Ok(reads) = cel::reads(expr) else {
            return ValueSource::Outside;
        };
        reads
            .iter()
            .map(|r| match (r.var.as_str(), self) {
                ("matrix", _) => ValueSource::Pipeline,
                ("inputs", InputSources::Run) => ValueSource::Outside,
                ("inputs", InputSources::Bound(m)) => match &r.first {
                    Some(name) => m.get(name).copied().unwrap_or(ValueSource::Outside),
                    None => m.values().copied().max().unwrap_or(ValueSource::Pipeline),
                },
                _ => ValueSource::Outside,
            })
            .max()
            .unwrap_or(ValueSource::Pipeline)
    }

    /// Where the text a template renders to comes from: from outside when
    /// any of its `${{ … }}` is. An unterminated `${{` is from outside.
    pub(crate) fn of_template(&self, template: &str) -> ValueSource {
        let (found, unterminated) = cel::scan_interpolations(template);
        if unterminated.is_some() {
            return ValueSource::Outside;
        }
        found
            .iter()
            .map(|i| self.of_expr(template[i.inner.clone()].trim()))
            .max()
            .unwrap_or(ValueSource::Pipeline)
    }
}
