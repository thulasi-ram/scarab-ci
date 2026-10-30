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

use crate::{cel, ParamSpec, ParamType, ValueSource};

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
///
/// Stored on every Step the invocation inlines, so it holds only what
/// rendering needs: no description, and no declaration at all for a value
/// already checked when the Pipeline compiled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// For a [`BoundValue::Template`], the input as the library declares it,
    /// without its description or default: its type, `options` and
    /// `validate:`, which the template's value is checked against at launch.
    /// `None` for a [`BoundValue::Literal`], converted and checked already. A
    /// template without one does not render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<Box<ParamSpec>>,
    pub value: BoundValue,
}

impl Binding {
    /// A literal `with:` value or default, converted and checked already.
    pub fn literal(value: serde_json::Value) -> Self {
        Binding {
            param: None,
            value: BoundValue::Literal(value),
        }
    }

    /// A caller's template, checked against `param` when it renders. Only
    /// the parts of `param` that check a value are kept.
    pub fn template(template: String, param: &ParamSpec) -> Self {
        Binding {
            param: Some(Box::new(ParamSpec {
                name: param.name.clone(),
                r#type: param.r#type,
                required: true,
                default: None,
                options: param.options.clone(),
                validate: param.validate.clone(),
                description: None,
            })),
            value: BoundValue::Template(template),
        }
    }
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

/// Where a value a template reads comes from, as the command boundary sees
/// it (ADR-0076, 2026-10-30). Ordered: an expression is as far outside as
/// the furthest of its reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Source {
    /// The Pipeline's own: a literal, `matrix`, a literal `with:` or a
    /// default.
    Pipeline,
    /// From outside, of a fixed type: an input declared `choice`, `boolean`
    /// or `number`, or an `event` field of a fixed shape
    /// ([`crate::event::FIXED`]: the commit SHA, a number, `fork`, the
    /// trigger kind). Its text may reach a command once checked at launch.
    Fixed,
    /// Any other value from outside: free text, a Step's results, any other
    /// event field. It reaches a Step only as an environment variable's
    /// value.
    Free,
}

/// The shape a fixed value from outside must have when the expression is
/// one read and nothing else (`inputs.region`, `event.sha`), checked when it
/// renders. Any other expression over fixed values is checked as a boolean
/// or a non-negative decimal number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape<'a> {
    /// A `choice` input: one of the values the Pipeline listed.
    Choice(&'a [String]),
    /// `event.sha`: `^[0-9a-f]{40}([0-9a-f]{24})?$`.
    Sha,
    /// `event.number`, `event.issue`: `^[0-9]+$`.
    Whole,
    /// `event.fork`: `true` or `false`.
    Bool,
    /// `event.kind`: one of [`crate::event::TRIGGER_KINDS`].
    Kind,
}

impl Source {
    /// The two-way view the script check uses.
    pub(crate) fn value_source(self) -> ValueSource {
        match self {
            Source::Pipeline => ValueSource::Pipeline,
            Source::Fixed | Source::Free => ValueSource::Outside,
        }
    }
}

/// One input of a scope: where its value comes from and, for a `choice`
/// from outside, the values the Pipeline listed.
#[derive(Debug, Clone)]
struct Input {
    source: Source,
    options: Option<Vec<String>>,
}

impl Input {
    /// An input from outside, by its declared type.
    fn outside(param: &ParamSpec) -> Self {
        match param.r#type {
            ParamType::Choice => Input {
                source: Source::Fixed,
                options: Some(param.options.clone().unwrap_or_default()),
            },
            ParamType::Boolean | ParamType::Number => Input {
                source: Source::Fixed,
                options: None,
            },
            ParamType::String => Input {
                source: Source::Free,
                options: None,
            },
        }
    }
}

/// Where each value a scope's templates read through `inputs` comes from
/// ([`Source`]), for the checks that treat an outside value differently
/// from the Pipeline's own: the command boundary at launch and at compile,
/// and the script check.
#[derive(Debug, Clone)]
pub(crate) struct InputSources {
    inputs: BTreeMap<String, Input>,
    /// The source of `inputs` read whole (`size(inputs)`).
    whole: Source,
}

impl InputSources {
    /// The sources of `scope`'s inputs (`None`: a Step written in the
    /// Pipeline file). `declared` is the Run's declared inputs
    /// (`interface.inputs`), whose types decide which launch parameters are
    /// of a fixed type. Built from the outermost caller in, once per scope,
    /// so the work is the size of the chain's bindings, however the templates
    /// read each other.
    pub(crate) fn of(scope: Option<&Scope>, declared: &[ParamSpec]) -> Self {
        let run = || InputSources {
            inputs: declared
                .iter()
                .map(|p| (p.name.clone(), Input::outside(p)))
                .collect(),
            whole: Source::Free,
        };
        let Some(scope) = scope else {
            return run();
        };
        let chain: Vec<&Scope> = scope.chain().collect();
        let mut caller: Option<InputSources> = None;
        for s in chain.into_iter().rev() {
            let here = match &s.inputs {
                ScopeInputs::Run => run(),
                ScopeInputs::Bound(bindings) => {
                    let inputs: BTreeMap<String, Input> = bindings
                        .iter()
                        .map(|(name, b)| {
                            let read = match &b.value {
                                BoundValue::Literal(_) => Source::Pipeline,
                                // A template with no caller scope fails at
                                // launch; until then it is from outside.
                                BoundValue::Template(t) => match &caller {
                                    Some(c) => c.of_template(t),
                                    None => Source::Free,
                                },
                            };
                            let input = match (read, b.param.as_deref()) {
                                (Source::Pipeline, _) => Input {
                                    source: Source::Pipeline,
                                    options: None,
                                },
                                // Converted to its declared type and checked
                                // when it renders.
                                (_, Some(param)) => Input::outside(param),
                                (_, None) => Input {
                                    source: Source::Free,
                                    options: None,
                                },
                            };
                            (name.clone(), input)
                        })
                        .collect();
                    let whole = inputs
                        .values()
                        .map(|i| i.source)
                        .max()
                        .unwrap_or(Source::Pipeline);
                    InputSources { inputs, whole }
                }
            };
            caller = Some(here);
        }
        caller.unwrap_or_else(run)
    }

    /// Where the input `name` comes from: from outside, as free text, when
    /// the scope does not have it.
    pub(crate) fn input(&self, name: &str) -> Source {
        self.inputs.get(name).map_or(Source::Free, |i| i.source)
    }

    /// Where the value of the expression `expr` comes from: as far outside
    /// as its furthest read. `matrix` is the Pipeline's own; an `event`
    /// field of a fixed shape is fixed; `outputs` (a Step's results), any
    /// other `event` read and any name not known here are free text from
    /// outside, and so is an expression that does not parse.
    pub(crate) fn of_expr(&self, expr: &str) -> Source {
        let Ok(reads) = cel::reads(expr) else {
            return Source::Free;
        };
        reads
            .iter()
            .map(|r| match r.var.as_str() {
                "matrix" => Source::Pipeline,
                "inputs" => match &r.first {
                    Some(name) => self.input(name),
                    None => self.whole,
                },
                // `event.sha`, not `event.sha.x`, `event[k]` or `event`.
                "event" => match (&r.first, &r.second) {
                    (Some(field), None) if crate::event::FIXED.contains(&field.as_str()) => {
                        Source::Fixed
                    }
                    _ => Source::Free,
                },
                _ => Source::Free,
            })
            .max()
            .unwrap_or(Source::Pipeline)
    }

    /// Where the text a template renders to comes from: as far outside as
    /// its furthest `${{ … }}`. An unterminated `${{` is free text from
    /// outside.
    pub(crate) fn of_template(&self, template: &str) -> Source {
        let (found, unterminated) = cel::scan_interpolations(template);
        if unterminated.is_some() {
            return Source::Free;
        }
        found
            .iter()
            .map(|i| self.of_expr(template[i.inner.clone()].trim()))
            .max()
            .unwrap_or(Source::Pipeline)
    }

    /// The shape of the fixed value `expr` reads, when `expr` is that read
    /// and nothing else: a `choice` input from outside (`inputs.region`),
    /// with the values the Pipeline listed, or an `event` field of a fixed
    /// shape (`event.sha`).
    pub(crate) fn shape(&self, expr: &str) -> Option<Shape<'_>> {
        match cel::path(expr)?.as_slice() {
            [var, name] if var == "inputs" => {
                self.inputs.get(name)?.options.as_deref().map(Shape::Choice)
            }
            [var, field] if var == "event" => match field.as_str() {
                "sha" => Some(Shape::Sha),
                "number" | "issue" => Some(Shape::Whole),
                "fork" => Some(Shape::Bool),
                "kind" => Some(Shape::Kind),
                _ => None,
            },
            _ => None,
        }
    }
}
