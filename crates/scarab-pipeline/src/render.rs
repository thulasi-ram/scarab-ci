//! Rendering a Step's `${{ … }}` templates (ADR-0041, ADR-0076).
//!
//! **Templates are code, values are data. Render once.** A template is parsed
//! when the Pipeline compiles, for diagnostics only, and rendered once, by
//! [`render`], when the Step launches. What a template renders to is never
//! scanned for `${{` again: not here, not in a caller, not in the executor.
//!
//! [`render`] is the one function that turns a Step's templates into text.
//! The engine calls it when a Step launches ([`Facts::Launch`]); Run creation
//! calls it to preview the image a Policy and privilege admission check
//! ([`Facts::Preview`]). With the same values, both produce the same text.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::scope::{BoundValue, InputSources, OutputSource, Scope, ScopeInputs, Source};
use crate::{cel, params, ParamSpec};

/// The results of a launching Step's `needs`, by Step id, then result name.
pub type Results = BTreeMap<String, BTreeMap<String, Value>>;

/// What is known when a Step's templates render.
#[derive(Debug, Clone, Copy)]
pub enum Facts<'a> {
    /// The Step is launching: the Run's launch parameters, the results of
    /// the Step's `needs` (a Step that is not a need is absent), and the
    /// `event` values stored with the Run (ADR-0076 §9, [`crate::event`]).
    Launch {
        params: &'a BTreeMap<String, Value>,
        /// The Run's declared inputs (`interface.inputs`): their types decide
        /// which launch parameters may reach a `command` (see [`arguments`]).
        declared: &'a [ParamSpec],
        results: &'a Results,
        event: &'a Map<String, Value>,
    },
    /// The Run is being created and no Step has run. Only values known then
    /// render: the Run's launch parameters, literal `with:` values and
    /// defaults, `matrix`, and an input bound to a caller's template that
    /// reads only these. A `${{ … }}` that reads anything else (an output,
    /// an input bound to a template that reads one) is left as its template
    /// text, so a leftover `${{` means "not known yet". The Run's `event`
    /// values are known then.
    Preview {
        params: &'a BTreeMap<String, Value>,
        declared: &'a [ParamSpec],
        event: &'a Map<String, Value>,
    },
}

/// The templates of a Step that render: its image, command and env values.
#[derive(Debug, Clone, Copy)]
pub struct Templates<'a> {
    pub image: &'a str,
    pub command: &'a [String],
    pub env: &'a [(String, String)],
}

/// A Step's rendered image, command and env. At launch, `env` begins with a
/// `SCARAB_PARAM_<NAME>` variable for each of the Step's inputs (ADR-0008),
/// added after rendering and never rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub image: String,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Render a Step's templates in its `scope` (`None`: a Step written in the
/// Pipeline file, whose `inputs` are the Run's launch parameters, `outputs`
/// the results of its needs and `matrix` its `matrix_values`).
///
/// At launch an error is the Step's failure reason: a template that does not
/// evaluate, a caller's value that does not convert to its input's type or
/// fails its `validate:`, or a value from outside the Pipeline that may not
/// be in a `command` (see [`arguments`]). A preview fails only for that last
/// reason: for any value that is free text, and for a fixed-type value it
/// knows that is not of its shape.
pub fn render(
    templates: Templates<'_>,
    scope: Option<&Scope>,
    matrix_values: &BTreeMap<String, String>,
    facts: Facts<'_>,
) -> Result<Rendered, String> {
    match facts {
        Facts::Launch {
            params,
            declared,
            results,
            event,
        } => {
            let bound = bind(
                scope,
                matrix_values,
                params,
                event,
                Some(results),
                Mode::Launch,
            )?;
            let ctx = Value::Object(bound.ctx);
            let one = |t: &str| cel::interpolate(t, &ctx).map_err(|e| e.to_string());
            let image = one(templates.image)?;
            // What the container runtime is handed must be an image
            // reference; an empty image is a Step that runs none (clone,
            // build).
            if !image.is_empty() && !crate::oci::is_image_reference(&image) {
                return Err(format!(
                    "image `{}` is not an OCI image reference",
                    image.escape_debug()
                ));
            }
            let sources = InputSources::of(scope, declared);
            let command = arguments(templates.command, &sources, true, |expr| {
                cel::eval(expr, &ctx).map(Some).map_err(|e| e.to_string())
            })?;
            // The Step's inputs reach it as `SCARAB_PARAM_*` too. Added after
            // rendering, so a value holding `${{` stays text; prepended, so
            // the Step's own env of the same name wins.
            let mut env: Vec<(String, String)> = bound
                .inputs
                .iter()
                .map(|(k, v)| {
                    (
                        format!("SCARAB_PARAM_{}", k.to_uppercase()),
                        params::stringify(v),
                    )
                })
                .collect();
            for (k, v) in templates.env {
                env.push((k.clone(), one(v)?));
            }
            Ok(Rendered {
                image,
                command,
                env,
            })
        }
        Facts::Preview {
            params,
            declared,
            event,
        } => {
            let bound = bind(scope, matrix_values, params, event, None, Mode::Preview);
            let one = |t: &str| match &bound {
                Ok(b) => preview(t, &b.ctx, &b.known),
                Err(_) => t.to_string(),
            };
            // The launch's rule: free text from outside is refused whether
            // or not its value is known; a fixed-type value is checked when
            // it is known and otherwise left as written.
            let known = bound
                .as_ref()
                .ok()
                .map(|b| (Value::Object(b.ctx.clone()), &b.known));
            let sources = InputSources::of(scope, declared);
            let command = arguments(templates.command, &sources, false, |expr| {
                Ok(known.as_ref().and_then(|(ctx, k)| {
                    k.covers(expr)
                        .then(|| cel::eval(expr, ctx).ok())
                        .flatten()
                }))
            })?;
            // An image the launch refuses is not known: a policy must not
            // read a digest from text that never runs.
            let mut image = one(templates.image);
            if !image.is_empty() && !crate::oci::is_image_reference(&image) {
                image = templates.image.to_string();
            }
            Ok(Rendered {
                image,
                command,
                env: templates
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), one(v)))
                    .collect(),
            })
        }
    }
}

/// Render a Step's `command`, one argument at a time, each `${{ … }}` by
/// `value` (`Ok(None)`: not known yet, left as written). `strict` (at
/// launch) refuses an unterminated `${{`.
///
/// The command boundary (ADR-0076, 2026-10-30): Scarab never puts a value
/// from outside the Pipeline into a Step's command, except a `choice` value
/// the Pipeline listed, `true`/`false`, or a non-negative decimal number.
/// Every other outside value (free text, a Step's results, a library input
/// bound to one) reaches the Step only as an environment variable's value.
/// Programs read their arguments as code in ways no list can cover (an
/// option, `@file`, `+ref`, a word a shell re-joins), and the fixed shapes
/// hold none of them.
///
/// Where each value comes from is decided by what the template reads
/// ([`InputSources`]), never by the rendered text. A fixed-type value is
/// checked against its shape once it renders: a choice is one of its listed
/// values, a number matches `^[0-9]+(\.[0-9]+)?$`. The Pipeline's own values
/// are not checked.
fn arguments(
    command: &[String],
    sources: &InputSources,
    strict: bool,
    mut value: impl FnMut(&str) -> Result<Option<Value>, String>,
) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(command.len());
    for (index, template) in command.iter().enumerate() {
        let (found, unterminated) = cel::scan_interpolations(template);
        let mut arg = String::with_capacity(template.len());
        let mut at = 0;
        for i in found {
            arg.push_str(&template[at..i.outer.start]);
            let expr = template[i.inner.clone()].trim();
            let shown = &template[i.outer.clone()];
            let source = sources.of_expr(expr);
            if source == Source::Free {
                return Err(free_text(index, shown));
            }
            match value(expr)? {
                Some(v) if source == Source::Fixed => {
                    let text = fixed_text(&v, sources.choice_options(expr))
                        .map_err(|what| not_fixed(index, shown, &what))?;
                    arg.push_str(&text);
                }
                Some(v) => arg.push_str(&cel::render(&v)),
                None => arg.push_str(shown),
            }
            at = i.outer.end;
        }
        // At launch, the same error as any other template's; a preview
        // leaves it as written.
        if strict && unterminated.is_some() {
            if let Err(e) = cel::interpolations(template) {
                return Err(e.to_string());
            }
        }
        arg.push_str(&template[at..]);
        out.push(arg);
    }
    Ok(out)
}

/// The text of a fixed-type value from outside, or what it is instead:
/// `true`/`false`, a non-negative decimal number, or one of the values
/// `options` lists (only for an expression that reads a `choice` input and
/// nothing else).
fn fixed_text(v: &Value, options: Option<&[String]>) -> Result<String, String> {
    match v {
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => {
            let text = n.to_string();
            if is_decimal(&text) {
                Ok(text)
            } else {
                Err(format!(
                    "the number `{text}`, which is not a non-negative decimal number"
                ))
            }
        }
        Value::String(s) if options.is_some_and(|o| o.iter().any(|x| x == s)) => Ok(s.clone()),
        Value::String(_) => Err("text that is not one of a `choice` input's listed values".into()),
        Value::Null => Err("null".into()),
        Value::Array(_) => Err("a list".into()),
        Value::Object(_) => Err("a map".into()),
    }
}

/// `^[0-9]+(\.[0-9]+)?$`: no sign, no exponent.
fn is_decimal(text: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match text.split_once('.') {
        Some((whole, frac)) => digits(whole) && digits(frac),
        None => digits(text),
    }
}

/// Why an argument is refused by [`arguments`]: a free-text value from
/// outside.
fn free_text(index: usize, shown: &str) -> String {
    format!(
        "`command[{index}]`: `{shown}` reads a value from outside the Pipeline (a Run \
         parameter, a Step's results, an event field, or a library input bound to one) that is \
         not a `choice`, boolean or number, and Scarab puts no such value into a command; pass \
         it in `env:` (`env: [[VALUE, \"{shown}\"]]`) and read it from the environment \
         (`\"$VALUE\"` in a shell script)"
    )
}

/// Why an argument is refused by [`arguments`]: a fixed-type value from
/// outside rendered to something else.
fn not_fixed(index: usize, shown: &str, what: &str) -> String {
    format!(
        "`command[{index}]`: `{shown}` reads a value from outside the Pipeline and renders to \
         {what}; a command takes only a listed `choice` value, `true`/`false` or a \
         non-negative decimal number from outside; pass it in `env:` (`env: [[VALUE, \
         \"{shown}\"]]`) and read it from the environment"
    )
}

/// The image a Step will launch with, as far as Run creation knows it
/// ([`Facts::Preview`]). What a `run.create` Policy and privilege admission
/// check; a `${{` left in it means part of it is not known yet.
pub fn preview_image(
    image: &str,
    scope: Option<&Scope>,
    matrix_values: &BTreeMap<String, String>,
    params: &BTreeMap<String, Value>,
    event: &Map<String, Value>,
) -> String {
    render(
        Templates {
            image,
            command: &[],
            env: &[],
        },
        scope,
        matrix_values,
        Facts::Preview {
            params,
            declared: &[],
            event,
        },
    )
    .map(|r| r.image)
    .unwrap_or_else(|_| image.to_string())
}

/// Evaluate a guard (`when:`) of a library Step or gate at Run creation, in
/// `scope` (ADR-0076): `inputs` are the scope's bound inputs, `matrix` its
/// coordinate. No Step has run, so `outputs` is not readable, and an input
/// bound to a caller's template that reads `outputs` is absent; the compiler
/// refuses a guard that reads one.
pub(crate) fn guard_holds(
    expr: &str,
    scope: &Scope,
    params: &BTreeMap<String, Value>,
) -> Result<bool, crate::PipelineError> {
    let bound = bind(
        Some(scope),
        &BTreeMap::new(),
        params,
        &Map::new(),
        None,
        Mode::Guard,
    )
        .map_err(crate::PipelineError::Cel)?;
    let mut ctx = bound.ctx;
    ctx.remove("outputs");
    cel::eval_bool(expr, &Value::Object(ctx))
}

/// How [`bind`] treats a value that is not known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Launch: every binding renders; an error fails the Step.
    Launch,
    /// Run creation, for a guard: a caller's template renders when every
    /// value it reads is known (it reads no `outputs`).
    Guard,
    /// Run creation, for a preview: as for a guard, and a caller's template
    /// that does not render (the launch will refuse it) is left unknown.
    Preview,
}

/// Which names of a scope have a value.
#[derive(Debug, Clone)]
struct Known {
    /// The input names with a value; `None` when every input has one.
    inputs: Option<BTreeSet<String>>,
    /// Whether `outputs` has its values (only at launch).
    outputs: bool,
}

impl Known {
    /// Whether every read of `expr` has a value. An expression that does not
    /// parse, or reads a name the scope does not bind, does not.
    fn covers(&self, expr: &str) -> bool {
        let Ok(reads) = cel::reads(expr) else {
            return false;
        };
        reads.iter().all(|r| match r.var.as_str() {
            "matrix" | "event" => true,
            "outputs" => self.outputs,
            "inputs" => match (&self.inputs, &r.first) {
                (None, _) => true,
                (Some(have), Some(name)) => have.contains(name),
                (Some(_), None) => false,
            },
            _ => false,
        })
    }

    /// Whether every `${{ … }}` of `template` is [`covers`](Self::covers)ed.
    fn covers_template(&self, template: &str) -> bool {
        let (found, unterminated) = cel::scan_interpolations(template);
        unterminated.is_none()
            && found
                .iter()
                .all(|i| self.covers(template[i.inner.clone()].trim()))
    }
}

/// A scope's values: the CEL context its templates render in.
struct Bound {
    /// `inputs`, `outputs` (at launch) and `matrix`.
    ctx: Map<String, Value>,
    known: Known,
    /// The scope's inputs that have a value, for `SCARAB_PARAM_*`.
    inputs: BTreeMap<String, Value>,
}

/// Build `scope`'s values. A caller's template renders in the caller's scope,
/// built once per scope, so the work grows with the depth of the invoke
/// chain, not with its fan-out.
fn bind(
    scope: Option<&Scope>,
    matrix_values: &BTreeMap<String, String>,
    params: &BTreeMap<String, Value>,
    event: &Map<String, Value>,
    results: Option<&Results>,
    mode: Mode,
) -> Result<Bound, String> {
    let coordinate = |m: &BTreeMap<String, String>| {
        Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        )
    };
    let mut ctx = Map::new();
    // The Run's checked `event` values (ADR-0076 §9) are trigger facts, not
    // a caller's values: every scope reads them, a library's included.
    ctx.insert("event".into(), Value::Object(event.clone()));
    let Some(scope) = scope else {
        // A Step written in the Pipeline file, rendered as it always was.
        let mut outputs = Map::new();
        for (step, r) in results.into_iter().flatten() {
            if !r.is_empty() {
                outputs.insert(step.clone(), Value::Object(r.clone().into_iter().collect()));
            }
        }
        ctx.insert("outputs".into(), Value::Object(outputs));
        ctx.insert(
            "inputs".into(),
            Value::Object(params.clone().into_iter().collect()),
        );
        ctx.insert("matrix".into(), coordinate(matrix_values));
        return Ok(Bound {
            ctx,
            known: Known {
                inputs: None,
                outputs: results.is_some(),
            },
            inputs: params.clone(),
        });
    };

    let (inputs, known_inputs) = match &scope.inputs {
        ScopeInputs::Run => (params.clone(), None),
        ScopeInputs::Bound(bindings) => {
            let any_template = bindings
                .values()
                .any(|b| matches!(b.value, BoundValue::Template(_)));
            let caller = if !any_template {
                None
            } else if mode == Mode::Preview {
                // A preview renders what it can and leaves the rest.
                scope
                    .caller
                    .as_deref()
                    .and_then(|c| {
                        bind(Some(c), &BTreeMap::new(), params, event, results, mode).ok()
                    })
            } else {
                let caller = scope.caller.as_deref().ok_or_else(|| {
                    "a caller's `with:` value has no caller scope to render in".to_string()
                })?;
                Some(bind(
                    Some(caller),
                    &BTreeMap::new(),
                    params,
                    event,
                    results,
                    mode,
                )?)
            };
            let mut values = BTreeMap::new();
            for (name, binding) in bindings {
                let template = match &binding.value {
                    BoundValue::Literal(v) => {
                        values.insert(name.clone(), v.clone());
                        continue;
                    }
                    BoundValue::Template(t) => t,
                };
                let Some(caller) = &caller else { continue };
                if mode != Mode::Launch && !caller.known.covers_template(template) {
                    continue;
                }
                let ctx = Value::Object(caller.ctx.clone());
                let value = (|| {
                    let raw = match exact(template) {
                        Some(expr) => cel::eval(expr, &ctx),
                        None => cel::interpolate(template, &ctx).map(Value::String),
                    }
                    .map_err(|e| format!("input `{name}` (from the caller's `with:`): {e}"))?;
                    let param = binding.param.as_deref().ok_or_else(|| {
                        format!("input `{name}` (from the caller's `with:`) has no declared type")
                    })?;
                    params::resolve_one(param, &raw)
                        .map_err(|e| format!("input `{name}` (from the caller's `with:`): {e}"))
                })();
                match value {
                    Ok(v) => {
                        values.insert(name.clone(), v);
                    }
                    // What the launch will refuse, a preview leaves unknown.
                    Err(_) if mode == Mode::Preview => {}
                    Err(e) => return Err(e),
                }
            }
            let known = values.keys().cloned().collect();
            (values, Some(known))
        }
    };

    if let Some(results) = results {
        let mut outputs = Map::new();
        for (name, source) in &scope.outputs {
            match source {
                OutputSource::Step(id) => {
                    if let Some(r) = results.get(id) {
                        outputs
                            .insert(name.clone(), Value::Object(r.clone().into_iter().collect()));
                    }
                }
                OutputSource::Invoke(exposed) => {
                    let values: Map<String, Value> = exposed
                        .iter()
                        .filter_map(|(out, id)| {
                            results
                                .get(id)
                                .and_then(|r| r.get(out))
                                .map(|v| (out.clone(), v.clone()))
                        })
                        .collect();
                    outputs.insert(name.clone(), Value::Object(values));
                }
            }
        }
        ctx.insert("outputs".into(), Value::Object(outputs));
    }
    ctx.insert(
        "inputs".into(),
        Value::Object(inputs.clone().into_iter().collect()),
    );
    ctx.insert("matrix".into(), coordinate(&scope.matrix));
    Ok(Bound {
        ctx,
        known: Known {
            inputs: known_inputs,
            outputs: results.is_some(),
        },
        inputs,
    })
}

/// The expression of a template that is exactly one `${{ e }}`, which
/// renders to `e`'s typed value rather than to text.
fn exact(template: &str) -> Option<&str> {
    let (found, unterminated) = cel::scan_interpolations(template);
    match (found.as_slice(), unterminated) {
        ([only], None) if only.outer == (0..template.len()) => {
            Some(template[only.inner.clone()].trim())
        }
        _ => None,
    }
}

/// [`Facts::Preview`] of one template: each `${{ … }}` whose reads all have
/// a value renders as it would at launch; any other is left as written.
fn preview(template: &str, ctx: &Map<String, Value>, known: &Known) -> String {
    let (found, _) = cel::scan_interpolations(template);
    let ctx = Value::Object(ctx.clone());
    let mut out = String::with_capacity(template.len());
    let mut at = 0;
    for i in found {
        out.push_str(&template[at..i.outer.start]);
        let expr = template[i.inner.clone()].trim();
        let value = known
            .covers(expr)
            .then(|| cel::eval(expr, &ctx).ok())
            .flatten();
        match value {
            Some(v) => out.push_str(&cel::render(&v)),
            None => out.push_str(&template[i.outer.clone()]),
        }
        at = i.outer.end;
    }
    out.push_str(&template[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::Binding;
    use crate::{ParamSpec, ParamType};
    use serde_json::json;

    fn param(name: &str, ty: ParamType) -> ParamSpec {
        ParamSpec {
            name: name.into(),
            r#type: ty,
            required: true,
            default: None,
            options: None,
            validate: None,
            description: None,
        }
    }

    fn map(v: Value) -> BTreeMap<String, Value> {
        serde_json::from_value(v).unwrap()
    }

    fn results(v: Value) -> Results {
        serde_json::from_value(v).unwrap()
    }

    /// Render `command` at launch, with no declared Run inputs: every
    /// launch parameter is free text.
    fn launch(
        command: &[&str],
        scope: Option<&Scope>,
        matrix: &BTreeMap<String, String>,
        params: &BTreeMap<String, Value>,
        results: &Results,
    ) -> Result<Rendered, String> {
        launch_declared(&[], command, scope, matrix, params, results)
    }

    /// Render `command` at launch, with the Run's declared inputs.
    fn launch_declared(
        declared: &[ParamSpec],
        command: &[&str],
        scope: Option<&Scope>,
        matrix: &BTreeMap<String, String>,
        params: &BTreeMap<String, Value>,
        results: &Results,
    ) -> Result<Rendered, String> {
        let command: Vec<String> = command.iter().map(|c| c.to_string()).collect();
        render(
            Templates {
                image: "busybox",
                command: &command,
                env: &[],
            },
            scope,
            matrix,
            Facts::Launch {
                params,
                declared,
                results,
                event: &serde_json::Map::new(),
            },
        )
    }

    /// Render `templates` at launch as the values of env variables, where
    /// a value from outside may be; the rendered values, in order.
    fn env_values(
        templates: &[&str],
        scope: Option<&Scope>,
        matrix: &BTreeMap<String, String>,
        params: &BTreeMap<String, Value>,
        results: &Results,
    ) -> Result<Vec<String>, String> {
        let env: Vec<(String, String)> = templates
            .iter()
            .enumerate()
            .map(|(i, t)| (format!("V{i}"), t.to_string()))
            .collect();
        let r = render(
            Templates {
                image: "busybox",
                command: &[],
                env: &env,
            },
            scope,
            matrix,
            Facts::Launch {
                params,
                declared: &[],
                results,
                event: &serde_json::Map::new(),
            },
        )?;
        Ok(r.env
            .into_iter()
            .filter(|(k, _)| !k.starts_with("SCARAB_PARAM_"))
            .map(|(_, v)| v)
            .collect())
    }

    /// A library scope binding `v` as given, called from the Pipeline file
    /// (whose `inputs` are the Run's parameters).
    fn library(bindings: Vec<(&str, ParamType, BoundValue)>) -> Scope {
        Scope {
            inputs: ScopeInputs::Bound(
                bindings
                    .into_iter()
                    .map(|(n, ty, value)| {
                        (
                            n.to_string(),
                            match value {
                                BoundValue::Literal(v) => Binding::literal(v),
                                BoundValue::Template(t) => Binding::template(t, &param(n, ty)),
                            },
                        )
                    })
                    .collect(),
            ),
            outputs: BTreeMap::new(),
            matrix: BTreeMap::new(),
            caller: Some(Box::new(Scope::run())),
        }
    }

    #[test]
    fn a_step_without_a_scope_renders_inputs_outputs_and_matrix_as_before() {
        let params = map(json!({ "p": "P", "n": 3 }));
        let res = results(json!({ "a": { "v": "V" } }));
        let matrix = BTreeMap::from([("os".into(), "linux".into())]);
        let v = env_values(
            &["${{ inputs.p }}", "${{ outputs.a.v }}", "${{ matrix.os }}"],
            None,
            &matrix,
            &params,
            &res,
        )
        .unwrap();
        assert_eq!(v, ["P", "V", "linux"]);
        // Every Run parameter reaches the Step, before its own env.
        let r = launch(&["${{ matrix.os }}"], None, &matrix, &params, &res).unwrap();
        assert_eq!(r.command, ["linux"]);
        assert_eq!(
            r.env,
            [
                ("SCARAB_PARAM_N".to_string(), "3".to_string()),
                ("SCARAB_PARAM_P".to_string(), "P".to_string())
            ]
        );
    }

    /// Render once: a value is data wherever it comes from. Each holds a
    /// `${{ … }}` that would read something else if it were rendered again.
    #[test]
    fn a_value_holding_a_template_is_never_rendered() {
        let evil = "${{ inputs.secret }}";
        let params = map(json!({ "p": evil, "secret": "LEAKED" }));
        let res = results(json!({ "a": { "v": evil }, "b": { "out": evil } }));
        // A Run parameter, a result and a matrix value, read by a Step
        // written in the Pipeline file.
        let v = env_values(
            &["${{ inputs.p }}", "${{ outputs.a.v }}", "${{ matrix.m }}"],
            None,
            &BTreeMap::from([("m".into(), evil.into())]),
            &params,
            &res,
        )
        .unwrap();
        assert_eq!(v, [evil, evil, evil]);

        // In a library scope: a literal binding, a caller's template whose
        // value holds a template, a view of a Step and of an invoke's
        // exposed output, and the scope's matrix.
        let mut scope = library(vec![
            ("lit", ParamType::String, BoundValue::Literal(json!(evil))),
            (
                "tpl",
                ParamType::String,
                BoundValue::Template("${{ inputs.p }}".into()),
            ),
            (
                "mixed",
                ParamType::String,
                BoundValue::Template("x-${{ outputs.a.v }}".into()),
            ),
        ]);
        scope.outputs = BTreeMap::from([
            ("s".into(), OutputSource::Step("a".into())),
            (
                "inv".into(),
                OutputSource::Invoke(BTreeMap::from([("out".into(), "b".into())])),
            ),
        ]);
        scope.matrix = BTreeMap::from([("m".into(), evil.into())]);
        let caller = scope.caller.as_mut().unwrap();
        caller.outputs = BTreeMap::from([("a".into(), OutputSource::Step("a".into()))]);
        let v = env_values(
            &[
                "${{ inputs.lit }}",
                "${{ inputs.tpl }}",
                "${{ inputs.mixed }}",
                "${{ outputs.s.v }}",
                "${{ outputs.inv.out }}",
                "${{ matrix.m }}",
            ],
            Some(&scope),
            &BTreeMap::new(),
            &params,
            &res,
        )
        .unwrap();
        assert_eq!(v, [evil, evil, &format!("x-{evil}"), evil, evil, evil]);
        assert!(!v.iter().any(|c| c.contains("LEAKED")));
    }

    #[test]
    fn a_library_step_sees_its_bindings_never_the_runs_parameters() {
        let scope = library(vec![(
            "tag",
            ParamType::String,
            BoundValue::Literal(json!("from-with")),
        )]);
        let params = map(json!({ "tag": "RUN", "other": "RUN" }));
        let r = launch(
            &["${{ inputs.tag }}"],
            Some(&scope),
            &BTreeMap::new(),
            &params,
            &Results::new(),
        )
        .unwrap();
        assert_eq!(r.command, ["from-with"]);
        // `SCARAB_PARAM_*` is the Step's own inputs only.
        assert_eq!(
            r.env,
            [("SCARAB_PARAM_TAG".to_string(), "from-with".to_string())]
        );
        let e = env_values(
            &["${{ inputs.other }}"],
            Some(&scope),
            &BTreeMap::new(),
            &params,
            &Results::new(),
        )
        .unwrap_err();
        assert!(e.contains("other"), "{e}");
    }

    #[test]
    fn an_exact_template_is_typed_and_checked_at_launch() {
        let res = results(json!({ "a": { "n": 5, "flag": true } }));
        let mut scope = library(vec![
            (
                "n",
                ParamType::Number,
                BoundValue::Template("${{ outputs.a.n }}".into()),
            ),
            (
                "b",
                ParamType::Boolean,
                BoundValue::Template("${{ outputs.a.flag }}".into()),
            ),
        ]);
        scope.caller.as_mut().unwrap().outputs =
            BTreeMap::from([("a".into(), OutputSource::Step("a".into()))]);
        let r = launch(
            &["${{ inputs.n > 4 }}", "${{ !inputs.b }}", "${{ inputs.n }}"],
            Some(&scope),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &res,
        )
        .unwrap();
        assert_eq!(r.command, ["true", "false", "5"]);

        // A value that does not convert, or fails `validate:`, fails the Step.
        let ScopeInputs::Bound(b) = &mut scope.inputs else {
            unreachable!()
        };
        b.get_mut("n").unwrap().param.as_mut().unwrap().validate = Some("value > 10".into());
        let e = launch(&[], Some(&scope), &BTreeMap::new(), &BTreeMap::new(), &res).unwrap_err();
        assert!(
            e.contains("input `n`") && e.contains("failed validation `value > 10`"),
            "{e}"
        );
        let res = results(json!({ "a": { "n": "five", "flag": true } }));
        let e = launch(&[], Some(&scope), &BTreeMap::new(), &BTreeMap::new(), &res).unwrap_err();
        assert!(e.contains("`five` is not a number"), "{e}");
    }

    #[test]
    fn a_template_binding_without_a_caller_scope_fails_and_previews_as_text() {
        let mut scope = library(vec![(
            "v",
            ParamType::String,
            BoundValue::Template("${{ inputs.p }}".into()),
        )]);
        scope.caller = None;
        let e = launch(
            &["${{ inputs.v }}"],
            Some(&scope),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &Results::new(),
        )
        .unwrap_err();
        assert!(e.contains("no caller scope"), "{e}");
        // A preview never fails: what it cannot render stays as written.
        let p =
            |image: &str| preview_image(image, Some(&scope), &BTreeMap::new(), &BTreeMap::new(), &serde_json::Map::new());
        assert_eq!(p("app:${{ inputs.v }}"), "app:${{ inputs.v }}");
        assert_eq!(
            p("app:${{ event.x }}-${{ 1 + }}"),
            "app:${{ event.x }}-${{ 1 + }}"
        );
    }

    #[test]
    fn a_preview_renders_only_what_is_known_at_run_creation() {
        let scope = library(vec![
            (
                "lit",
                ParamType::String,
                BoundValue::Literal(json!("sha256:abc")),
            ),
            (
                "tpl",
                ParamType::String,
                BoundValue::Template("${{ inputs.d }}".into()),
            ),
            (
                "out",
                ParamType::String,
                BoundValue::Template("${{ outputs.a.d }}".into()),
            ),
            (
                "num",
                ParamType::Number,
                BoundValue::Template("${{ inputs.d }}".into()),
            ),
        ]);
        let params = map(json!({ "d": "sha256:def" }));
        let p = |image: &str, scope: Option<&Scope>| {
            preview_image(image, scope, &BTreeMap::new(), &params, &serde_json::Map::new())
        };
        assert_eq!(p("img@${{ inputs.lit }}", Some(&scope)), "img@sha256:abc");
        // A caller's template that reads a launch parameter is known; one
        // that reads an output, or that the launch will refuse, is not.
        assert_eq!(p("img@${{ inputs.tpl }}", Some(&scope)), "img@sha256:def");
        assert_eq!(
            p("img@${{ inputs.out }}", Some(&scope)),
            "img@${{ inputs.out }}"
        );
        assert_eq!(
            p("img@${{ inputs.num }}", Some(&scope)),
            "img@${{ inputs.num }}"
        );
        assert_eq!(
            p("img@${{ size(inputs) > 0 ? 'x' : 'y' }}", Some(&scope)),
            "img@${{ size(inputs) > 0 ? 'x' : 'y' }}"
        );
        assert_eq!(p("img@${{ inputs.d }}", None), "img@sha256:def");
        assert_eq!(p("img@${{ outputs.a.d }}", None), "img@${{ outputs.a.d }}");
        assert_eq!(p("img:${{ unterminated", None), "img:${{ unterminated");
    }
    /// The command boundary (ADR-0076, 2026-10-30): free text from outside
    /// never reaches a command, whatever its value and wherever it sits in
    /// an argument; it is refused at launch and in a preview, known or not.
    #[test]
    fn free_text_from_outside_never_reaches_a_command() {
        let declared = [param("p", ParamType::String), param("n", ParamType::Number)];
        // Values that look like fixed shapes are refused by where they come
        // from, not by what they are.
        let params = map(json!({ "p": "5", "n": 3, "undeclared": "true" }));
        let res = results(json!({ "a": { "v": 7, "flag": true } }));
        let matrix = BTreeMap::from([("m".into(), "make".into())]);
        let mut scope = library(vec![
            (
                "s",
                ParamType::String,
                BoundValue::Template("${{ inputs.n }}".into()),
            ),
            ("lit", ParamType::String, BoundValue::Literal(json!("-x"))),
        ]);
        scope.caller.as_mut().unwrap().outputs =
            BTreeMap::from([("a".into(), OutputSource::Step("a".into()))]);
        scope.matrix = matrix.clone();
        for (command, scope, at, shown) in [
            (&["git", "${{ inputs.p }}"][..], None, 1, "${{ inputs.p }}"),
            (&["--ref=${{ inputs.p }}"], None, 0, "${{ inputs.p }}"),
            (&["sh", "-c", "x ${{ inputs.p }}"], None, 2, "${{ inputs.p }}"),
            (&["x", "${{ inputs.undeclared }}"], None, 1, "${{ inputs.undeclared }}"),
            // A result, even a number or a boolean: results have no type.
            (&["x", "${{ outputs.a.v }}"], None, 1, "${{ outputs.a.v }}"),
            (&["x", "${{ outputs.a.flag }}"], None, 1, "${{ outputs.a.flag }}"),
            // Computed from free text, or from everything.
            (&["x", "${{ size(inputs.p) }}"], None, 1, "${{ size(inputs.p) }}"),
            (&["x", "${{ size(inputs) }}"], None, 1, "${{ size(inputs) }}"),
            (&["x", "${{ event.sha }}"], None, 1, "${{ event.sha }}"),
            // A library's string input bound to a caller's number: the
            // library declared free text.
            (&["x", "${{ inputs.s }}"], Some(&scope), 1, "${{ inputs.s }}"),
        ] {
            let e = launch_declared(&declared, command, scope, &matrix, &params, &res)
                .unwrap_err();
            assert!(
                e.starts_with(&format!("`command[{at}]`: `{shown}` reads a value from outside"))
                    && e.contains("pass it in `env:`"),
                "{command:?}: {e}"
            );
            // The preview refuses it too, known or not.
            let command: Vec<String> = command.iter().map(|c| c.to_string()).collect();
            let preview = render(
                Templates {
                    image: "",
                    command: &command,
                    env: &[],
                },
                scope,
                &matrix,
                Facts::Preview {
                    params: &params,
                    declared: &declared,
                    event: &serde_json::Map::new(),
                },
            );
            assert_eq!(preview.unwrap_err(), e);
        }
        // The Pipeline's own values are not checked: a literal binding, the
        // matrix, as the program or as an option.
        let r = launch_declared(
            &declared,
            &["${{ matrix.m }}", "${{ inputs.lit }}"],
            Some(&scope),
            &matrix,
            &params,
            &res,
        )
        .unwrap();
        assert_eq!(r.command, ["make", "-x"]);
    }

    /// A listed `choice`, a boolean or a non-negative decimal number from
    /// outside reaches a command, checked against its shape when it renders.
    #[test]
    fn fixed_shapes_from_outside_reach_a_command_checked_at_launch() {
        let mut region = param("region", ParamType::Choice);
        region.options = Some(vec!["eu".into(), "us-east".into()]);
        let declared = [
            region.clone(),
            param("n", ParamType::Number),
            param("b", ParamType::Boolean),
        ];
        let run = |command: &[&str], params: Value| {
            launch_declared(
                &declared,
                command,
                None,
                &BTreeMap::new(),
                &map(params),
                &Results::new(),
            )
        };
        let r = run(
            &[
                "deploy",
                "${{ inputs.region }}",
                "n=${{ inputs.n }}",
                "${{ inputs.b }}",
                "${{ inputs.n > 80 }}",
                "${{ inputs['region'] }}",
            ],
            json!({ "region": "eu", "n": 3, "b": true }),
        )
        .unwrap();
        assert_eq!(r.command, ["deploy", "eu", "n=3", "true", "false", "eu"]);
        assert_eq!(
            run(&["x", "${{ inputs.n }}"], json!({ "n": 1.5 })).unwrap().command,
            ["x", "1.5"]
        );
        // What does not fit its shape is refused at launch, naming it.
        for (command, params, what) in [
            // A sign: `-1` is an option once a program reads it.
            (&["x", "${{ inputs.n }}"][..], json!({ "n": -1 }), "the number `-1`"),
            (&["x", "${{ inputs.n - 10 }}"], json!({ "n": 3 }), "the number `-7`"),
            (&["x", "${{ inputs.n }}"], json!({ "n": 1e300 }), "the number `1e+300`"),
            // A value the Pipeline did not list (a parameter that skipped
            // Run creation's check).
            (&["x", "${{ inputs.region }}"], json!({ "region": "--x" }), "text that is not"),
            // Text built from a choice is not a listed value.
            (
                &["x", "${{ inputs.region + '-x' }}"],
                json!({ "region": "eu" }),
                "text that is not",
            ),
            (&["x", "${{ inputs.b ? 'a' : 'b' }}"], json!({ "b": true }), "text that is not"),
            (&["x", "${{ [inputs.n] }}"], json!({ "n": 1 }), "a list"),
            (&["x", "${{ {'a': inputs.n} }}"], json!({ "n": 1 }), "a map"),
            (&["x", "${{ inputs.b ? null : 1 }}"], json!({ "b": true }), "null"),
        ] {
            let e = run(command, params).unwrap_err();
            assert!(
                e.starts_with("`command[1]`") && e.contains(&format!("renders to {what}")),
                "{command:?}: {e}"
            );
        }

        // A library input bound to a caller's template that reads a Step's
        // results: typed by the library's declaration, converted, checked.
        let mut scope = library(vec![
            (
                "n",
                ParamType::Number,
                BoundValue::Template("${{ outputs.a.n }}".into()),
            ),
            (
                "where",
                ParamType::Choice,
                BoundValue::Template("${{ outputs.a.w }}".into()),
            ),
        ]);
        let ScopeInputs::Bound(b) = &mut scope.inputs else {
            unreachable!()
        };
        b.get_mut("where").unwrap().param = Some(Box::new(region));
        scope.caller.as_mut().unwrap().outputs =
            BTreeMap::from([("a".into(), OutputSource::Step("a".into()))]);
        let lib = |res: Value| {
            launch(
                &["x", "${{ inputs.n }}", "${{ inputs.where }}"],
                Some(&scope),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &results(res),
            )
        };
        assert_eq!(
            lib(json!({ "a": { "n": "4", "w": "us-east" } })).unwrap().command,
            ["x", "4", "us-east"]
        );
        let e = lib(json!({ "a": { "n": "4", "w": "ap" } })).unwrap_err();
        assert!(e.contains("input `where`") && e.contains("allowed choices"), "{e}");
        let e = lib(json!({ "a": { "n": -2, "w": "eu" } })).unwrap_err();
        assert!(e.contains("the number `-2`"), "{e}");

        // A preview checks what it knows and leaves the rest as written.
        let preview = |params: Value| {
            let command = vec!["x".to_string(), "${{ inputs.n }}".to_string()];
            render(
                Templates {
                    image: "",
                    command: &command,
                    env: &[],
                },
                Some(&scope),
                &BTreeMap::new(),
                Facts::Preview {
                    params: &map(params),
                    declared: &declared,
                    event: &serde_json::Map::new(),
                },
            )
        };
        assert_eq!(preview(json!({})).unwrap().command, ["x", "${{ inputs.n }}"]);
        let e = run(&["x", "${{ inputs.n }}"], json!({ "n": -1 })).unwrap_err();
        let command = vec!["x".to_string(), "${{ inputs.n }}".to_string()];
        let p = render(
            Templates {
                image: "",
                command: &command,
                env: &[],
            },
            None,
            &BTreeMap::new(),
            Facts::Preview {
                params: &map(json!({ "n": -1 })),
                declared: &declared,
                event: &serde_json::Map::new(),
            },
        );
        assert_eq!(p.unwrap_err(), e);
    }

    #[test]
    fn a_decimal_has_no_sign_and_no_exponent() {
        for ok in ["0", "10", "1.5", "007.250"] {
            assert!(is_decimal(ok), "{ok}");
        }
        for bad in ["", "-1", "+1", "1.", ".5", "1e5", "1.2.3", "１", "NaN"] {
            assert!(!is_decimal(bad), "{bad}");
        }
    }

    /// A stored binding written before bindings were trimmed (with the
    /// input's whole declaration) still loads and checks its value; a
    /// template stored without a declaration does not render; a literal is
    /// stored without one.
    #[test]
    fn a_binding_stores_only_what_rendering_needs() {
        let old: Binding = serde_json::from_value(json!({
            "param": {
                "name": "v", "type": "number", "required": true,
                "description": "a long description", "validate": "value < 10"
            },
            "value": { "template": "${{ inputs.p }}" }
        }))
        .unwrap();
        let mut scope = library(vec![]);
        scope.inputs = ScopeInputs::Bound(BTreeMap::from([("v".to_string(), old)]));
        let run = |scope: &Scope, p: Value| {
            launch(
                &["${{ inputs.v }}"],
                Some(scope),
                &BTreeMap::new(),
                &map(json!({ "p": p })),
                &Results::new(),
            )
        };
        assert_eq!(run(&scope, json!(5)).unwrap().command, ["5"]);
        let e = run(&scope, json!(50)).unwrap_err();
        assert!(e.contains("failed validation `value < 10`"), "{e}");

        let ScopeInputs::Bound(b) = &mut scope.inputs else {
            unreachable!()
        };
        b.get_mut("v").unwrap().param = None;
        let e = run(&scope, json!(5)).unwrap_err();
        assert!(
            e.contains("input `v`") && e.contains("no declared type"),
            "{e}"
        );

        let mut declared = param("v", ParamType::Number);
        declared.description = Some("not stored".into());
        declared.validate = Some("value < 10".into());
        assert_eq!(
            serde_json::to_value(Binding::literal(json!(1))).unwrap(),
            json!({ "value": { "literal": 1 } })
        );
        assert_eq!(
            serde_json::to_value(Binding::template("${{ inputs.p }}".into(), &declared)).unwrap(),
            json!({
                "param": { "name": "v", "type": "number", "required": true, "validate": "value < 10" },
                "value": { "template": "${{ inputs.p }}" }
            })
        );
    }
}
