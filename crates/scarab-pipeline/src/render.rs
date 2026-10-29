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

use crate::scope::{BoundValue, InputSources, OutputSource, Scope, ScopeInputs};
use crate::{cel, params, ValueSource};

/// The results of a launching Step's `needs`, by Step id, then result name.
pub type Results = BTreeMap<String, BTreeMap<String, Value>>;

/// What is known when a Step's templates render.
#[derive(Debug, Clone, Copy)]
pub enum Facts<'a> {
    /// The Step is launching: the Run's launch parameters and the results of
    /// the Step's `needs` (a Step that is not a need is absent).
    Launch {
        params: &'a BTreeMap<String, Value>,
        results: &'a Results,
    },
    /// The Run is being created and no Step has run. Only values known then
    /// render: the Run's launch parameters, literal `with:` values and
    /// defaults, and `matrix`. A `${{ … }}` that reads anything else (an
    /// output, an input bound to the caller's template) is left as its
    /// template text, so a leftover `${{` means "not known yet".
    Preview { params: &'a BTreeMap<String, Value> },
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
/// fails its `validate:`, or a value from outside the Pipeline that would
/// start a `command` argument with `-` (see [`arguments`]). A preview fails
/// only for that last reason, and only for a value it knows.
pub fn render(
    templates: Templates<'_>,
    scope: Option<&Scope>,
    matrix_values: &BTreeMap<String, String>,
    facts: Facts<'_>,
) -> Result<Rendered, String> {
    match facts {
        Facts::Launch { params, results } => {
            let bound = bind(scope, matrix_values, params, Some(results), Mode::Launch)?;
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
            let sources = InputSources::of(scope);
            let command = arguments(templates.command, &sources, true, |expr| {
                cel::eval(expr, &ctx)
                    .map(|v| Some(cel::render(&v)))
                    .map_err(|e| e.to_string())
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
        Facts::Preview { params } => {
            let bound = bind(scope, matrix_values, params, None, Mode::Preview);
            let one = |t: &str| match &bound {
                Ok(b) => preview(t, &b.ctx, &b.known),
                Err(_) => t.to_string(),
            };
            // A value known now is refused as an argument the way the
            // launch refuses it; what is not known is left as written.
            let command = match &bound {
                Ok(b) => {
                    let ctx = Value::Object(b.ctx.clone());
                    arguments(templates.command, &InputSources::of(scope), false, |expr| {
                        Ok(b.known
                            .covers(expr)
                            .then(|| cel::eval(expr, &ctx).ok())
                            .flatten()
                            .map(|v| cel::render(&v)))
                    })?
                }
                Err(_) => templates.command.to_vec(),
            };
            Ok(Rendered {
                image: one(templates.image),
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
/// A value from outside the Pipeline ([`ValueSource::Outside`]: a Run
/// parameter, a Step's results, a caller's `with:` that reads one) is data,
/// but a program reads an argument that starts with `-` as an option: `git
/// fetch origin ${{ inputs.ref }}` with `--upload-pack=…` runs a command. So
/// such a value may not be the start of an argument. Text written before it
/// (`--ref=${{ inputs.ref }}`) is the Pipeline's own, and an argument right
/// after a `--` argument is an operand, so both are allowed. Which part of
/// an argument is written and which is a value is known here, while the
/// argument is built, so the check never reads the rendered text again.
fn arguments(
    command: &[String],
    sources: &InputSources,
    strict: bool,
    mut value: impl FnMut(&str) -> Result<Option<String>, String>,
) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(command.len());
    for (index, template) in command.iter().enumerate() {
        let operand = index > 0 && command[index - 1] == "--";
        let (found, unterminated) = cel::scan_interpolations(template);
        let mut arg = String::with_capacity(template.len());
        let mut at = 0;
        for i in found {
            arg.push_str(&template[at..i.outer.start]);
            let expr = template[i.inner.clone()].trim();
            match value(expr)? {
                Some(text) => {
                    if arg.is_empty()
                        && !operand
                        && text.starts_with('-')
                        && sources.of_expr(expr) == ValueSource::Outside
                    {
                        return Err(leading_dash(index, &template[i.outer.clone()]));
                    }
                    arg.push_str(&text);
                }
                None => arg.push_str(&template[i.outer.clone()]),
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

/// Why an argument is refused by [`arguments`].
fn leading_dash(index: usize, shown: &str) -> String {
    format!(
        "`command[{index}]`: `{shown}` starts the argument with `-`, and its value comes from \
         outside the Pipeline (a Run parameter, a Step's results or an event field), so the \
         program would read it as an option; write the option in front of it \
         (`--name={shown}`), or put a `--` argument just before it"
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
) -> String {
    render(
        Templates {
            image,
            command: &[],
            env: &[],
        },
        scope,
        matrix_values,
        Facts::Preview { params },
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
    let bound = bind(Some(scope), &BTreeMap::new(), params, None, Mode::Guard)
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
    /// Run creation, for a preview: a caller's template is not rendered.
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
            "matrix" => true,
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
            let caller = if any_template && mode != Mode::Preview {
                let caller = scope.caller.as_deref().ok_or_else(|| {
                    "a caller's `with:` value has no caller scope to render in".to_string()
                })?;
                Some(bind(Some(caller), &BTreeMap::new(), params, results, mode)?)
            } else {
                None
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
                if mode == Mode::Guard && !caller.known.covers_template(template) {
                    continue;
                }
                let ctx = Value::Object(caller.ctx.clone());
                let raw = match exact(template) {
                    Some(expr) => cel::eval(expr, &ctx),
                    None => cel::interpolate(template, &ctx).map(Value::String),
                }
                .map_err(|e| format!("input `{name}` (from the caller's `with:`): {e}"))?;
                let value = params::resolve_one(&binding.param, &raw)
                    .map_err(|e| format!("input `{name}` (from the caller's `with:`): {e}"))?;
                values.insert(name.clone(), value);
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

    fn launch(
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
            Facts::Launch { params, results },
        )
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
                            Binding {
                                param: param(n, ty),
                                value,
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
        let r = launch(
            &["${{ inputs.p }}", "${{ outputs.a.v }}", "${{ matrix.os }}"],
            None,
            &BTreeMap::from([("os".into(), "linux".into())]),
            &map(json!({ "p": "P", "n": 3 })),
            &results(json!({ "a": { "v": "V" } })),
        )
        .unwrap();
        assert_eq!(r.command, ["P", "V", "linux"]);
        // Every Run parameter reaches the Step, before its own env.
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
        let r = launch(
            &["${{ inputs.p }}", "${{ outputs.a.v }}", "${{ matrix.m }}"],
            None,
            &BTreeMap::from([("m".into(), evil.into())]),
            &params,
            &res,
        )
        .unwrap();
        assert_eq!(r.command, [evil, evil, evil]);
        assert!(r.env.contains(&("SCARAB_PARAM_P".into(), evil.into())));

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
        let r = launch(
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
        assert_eq!(
            r.command,
            [evil, evil, &format!("x-{evil}"), evil, evil, evil]
        );
        assert!(!r
            .command
            .iter()
            .chain(r.env.iter().map(|(_, v)| v))
            .any(|c| c.contains("LEAKED")));
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
        let e = launch(
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
        b.get_mut("n").unwrap().param.validate = Some("value > 10".into());
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
            |image: &str| preview_image(image, Some(&scope), &BTreeMap::new(), &BTreeMap::new());
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
        ]);
        let params = map(json!({ "d": "sha256:def" }));
        let p = |image: &str, scope: Option<&Scope>| {
            preview_image(image, scope, &BTreeMap::new(), &params)
        };
        assert_eq!(p("img@${{ inputs.lit }}", Some(&scope)), "img@sha256:abc");
        assert_eq!(
            p("img@${{ inputs.tpl }}", Some(&scope)),
            "img@${{ inputs.tpl }}"
        );
        assert_eq!(
            p("img@${{ size(inputs) > 0 ? 'x' : 'y' }}", Some(&scope)),
            "img@${{ size(inputs) > 0 ? 'x' : 'y' }}"
        );
        assert_eq!(p("img@${{ inputs.d }}", None), "img@sha256:def");
        assert_eq!(p("img@${{ outputs.a.d }}", None), "img@${{ outputs.a.d }}");
        assert_eq!(p("img:${{ unterminated", None), "img:${{ unterminated");
    }
    /// Decision B9: a value from outside the Pipeline may not start an
    /// argument with `-`; the Pipeline's own values and authored text may.
    #[test]
    fn an_outside_value_may_not_start_an_argument_with_a_dash() {
        let evil = "--upload-pack=touch /tmp/pwned";
        let params = map(json!({ "p": evil, "empty": "" }));
        let res = results(json!({ "a": { "v": "-x" } }));
        let matrix = BTreeMap::from([("m".into(), "-m".into()), ("e".into(), String::new())]);
        let run = |command: &[&str], scope: Option<&Scope>| {
            launch(command, scope, &matrix, &params, &res)
        };
        // A Run parameter, a result, or a Pipeline value that renders empty
        // in front of one: refused, naming the template.
        for command in [
            &["git", "fetch", "${{ inputs.p }}"][..],
            &["${{ outputs.a.v }}"],
            &["x", "${{ matrix.e }}${{ inputs.p }}"],
            &["x", "${{ inputs.empty }}${{ inputs.p }}"],
            &["x", "${{ inputs.p + 'x' }}"],
        ] {
            let e = run(command, None).unwrap_err();
            assert!(
                e.contains("starts the argument with `-`") && e.contains("from outside"),
                "{command:?}: {e}"
            );
        }
        let e = run(&["git", "${{ inputs.p }}"], None).unwrap_err();
        assert!(e.contains("`command[1]`: `${{ inputs.p }}`"), "{e}");
        // Authored text in front, an operand after `--`, the Pipeline's own
        // value, an env value: allowed.
        let r = run(
            &[
                "--ref=${{ inputs.p }}",
                "-${{ inputs.p }}",
                "--",
                "${{ inputs.p }}",
                "${{ matrix.m }}",
                "x${{ outputs.a.v }}",
            ],
            None,
        )
        .unwrap();
        assert_eq!(r.command[0], format!("--ref={evil}"));
        assert_eq!(r.command[3], evil);
        assert_eq!(r.command[4], "-m");

        // In a library: a literal binding is the Pipeline's own; a caller's
        // template is outside when it reads a Run parameter.
        let scope = library(vec![
            ("lit", ParamType::String, BoundValue::Literal(json!("-lit"))),
            ("tpl", ParamType::String, BoundValue::Template("${{ inputs.p }}".into())),
            ("wrapped", ParamType::String, BoundValue::Template("-${{ inputs.p }}".into())),
        ]);
        assert_eq!(run(&["${{ inputs.lit }}"], Some(&scope)).unwrap().command, ["-lit"]);
        for arg in ["${{ inputs.tpl }}", "${{ inputs.wrapped }}"] {
            let e = run(&[arg], Some(&scope)).unwrap_err();
            assert!(e.contains("starts the argument with `-`"), "{arg}: {e}");
        }
        // An unterminated template is still an error at launch.
        let e = run(&["${{ inputs.p"], None).unwrap_err();
        assert!(e.contains("unterminated"), "{e}");

        // A preview refuses what it knows and leaves the rest as written.
        let preview = |command: &[&str], scope: Option<&Scope>| {
            let command: Vec<String> = command.iter().map(|c| c.to_string()).collect();
            render(
                Templates {
                    image: "busybox",
                    command: &command,
                    env: &[],
                },
                scope,
                &matrix,
                Facts::Preview { params: &params },
            )
        };
        let e = preview(&["git", "${{ inputs.p }}"], None).unwrap_err();
        assert!(e.contains("starts the argument with `-`"), "{e}");
        let r = preview(&["${{ inputs.tpl }}", "${{ inputs.p"], Some(&scope)).unwrap();
        assert_eq!(r.command, ["${{ inputs.tpl }}", "${{ inputs.p"]);
    }
}
