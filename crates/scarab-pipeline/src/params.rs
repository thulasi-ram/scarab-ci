//! Launch-parameter coercion + resolution (ADR-0043).
//!
//! A pipeline's [`Interface::inputs`](crate::Interface) declares typed launch
//! parameters ([`ParamSpec`]). A caller supplies raw values (a launch `params`
//! map, or an invoke `with:`); this module turns those into a validated,
//! fully-typed `name → value` map, applying defaults for absent optionals and
//! failing **closed** on anything wrong — an unknown param, a value that will
//! not coerce, a choice outside its options, a `validate:` predicate that does
//! not hold.
//!
//! Pure (ADR-0031): no I/O; the engine builds the supplied map at the edge and
//! calls in. Shared by the compile-time invoke path and the launch path so the
//! two agree byte-for-byte.

use std::collections::{BTreeMap, BTreeSet};

use crate::{cel, Interface, ParamSpec, ParamType, PipelineError};

fn param_err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Param(msg.into())
}

/// Coerce a raw JSON value to the shape of a declared [`ParamType`], fail-closed.
///
/// Already-typed values pass through; string forms are parsed
/// (`"3"` → number `3`, `"true"`/`"yes"` → `true`). `choice` only requires a
/// string here — membership in `options` is enforced by [`resolve_one`] /
/// [`resolve_params`]. Anything that does not fit is an error.
pub fn coerce(raw: &serde_json::Value, ty: ParamType) -> Result<serde_json::Value, PipelineError> {
    use serde_json::Value;
    match ty {
        ParamType::String => match raw {
            Value::String(s) => Ok(Value::String(s.clone())),
            Value::Number(n) => Ok(Value::String(n.to_string())),
            other => Err(param_err(format!("cannot coerce {other} to a string"))),
        },
        ParamType::Number => match raw {
            Value::Number(_) => Ok(raw.clone()),
            Value::String(s) => {
                let t = s.trim();
                if let Ok(i) = t.parse::<i64>() {
                    Ok(Value::Number(i.into()))
                } else if let Ok(f) = t.parse::<f64>() {
                    serde_json::Number::from_f64(f)
                        .map(Value::Number)
                        .ok_or_else(|| param_err(format!("`{s}` is not a finite number")))
                } else {
                    Err(param_err(format!("`{s}` is not a number")))
                }
            }
            other => Err(param_err(format!("cannot coerce {other} to a number"))),
        },
        ParamType::Boolean => match raw {
            Value::Bool(_) => Ok(raw.clone()),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" => Ok(Value::Bool(true)),
                "false" | "no" => Ok(Value::Bool(false)),
                _ => Err(param_err(format!("`{s}` is not a boolean"))),
            },
            other => Err(param_err(format!("cannot coerce {other} to a boolean"))),
        },
        // A choice is carried as its string label; the option set is enforced at
        // the resolve layer (it needs the spec, not just the type).
        ParamType::Choice => match raw {
            Value::String(s) => Ok(Value::String(s.clone())),
            other => Err(param_err(format!(
                "a choice value must be a string, got {other}"
            ))),
        },
    }
}

/// Resolve one supplied value against its spec: coerce to the declared type,
/// enforce `choice ∈ options`, then evaluate the `validate:` predicate (bound as
/// `value`). Fail-closed on any step.
pub(crate) fn resolve_one(
    spec: &ParamSpec,
    raw: &serde_json::Value,
) -> Result<serde_json::Value, PipelineError> {
    let coerced = coerce(raw, spec.r#type)?;

    if spec.r#type == ParamType::Choice {
        let opts = spec.options.as_deref().unwrap_or(&[]);
        if let serde_json::Value::String(s) = &coerced {
            if !opts.iter().any(|o| o == s) {
                return Err(param_err(format!(
                    "`{s}` is not one of the allowed choices [{}]",
                    opts.join(", ")
                )));
            }
        }
    }

    if let Some(expr) = &spec.validate {
        let ctx = serde_json::json!({ "value": coerced });
        // A non-bool / erroring predicate propagates as a hard failure.
        if !cel::eval_bool(expr, &ctx)? {
            return Err(param_err(format!("failed validation `{expr}`")));
        }
    }
    Ok(coerced)
}

/// Resolve a caller's `supplied` values against an interface's declared params
/// (ADR-0043): apply defaults for absent optionals, error if a required param is
/// missing, coerce each value to its declared type, enforce `choice ∈ options`
/// and each `validate:` predicate, and reject unknown/extra params. Returns the
/// fully-typed `name → value` map.
///
/// All problems are aggregated into a single [`PipelineError::Validation`], one
/// line per offending parameter.
pub fn resolve_params(
    iface: &Interface,
    supplied: &BTreeMap<String, serde_json::Value>,
) -> Result<BTreeMap<String, serde_json::Value>, PipelineError> {
    let mut errors: Vec<String> = Vec::new();
    let mut resolved: BTreeMap<String, serde_json::Value> = BTreeMap::new();

    // Reject unknown/extra params up front (fail-closed — a typo must not be
    // silently dropped).
    for k in supplied.keys() {
        if !iface.inputs.iter().any(|p| &p.name == k) {
            errors.push(format!("`{k}`: unknown parameter"));
        }
    }

    for p in &iface.inputs {
        match supplied.get(&p.name) {
            Some(raw) => match resolve_one(p, raw) {
                Ok(v) => {
                    resolved.insert(p.name.clone(), v);
                }
                Err(e) => errors.push(format!("`{}`: {e}", p.name)),
            },
            None => {
                if let Some(def) = &p.default {
                    // A default is authored in the declared type but is coerced +
                    // checked all the same, so an ill-typed default fails loudly.
                    match resolve_one(p, def) {
                        Ok(v) => {
                            resolved.insert(p.name.clone(), v);
                        }
                        Err(e) => errors.push(format!("`{}` (default): {e}", p.name)),
                    }
                } else if p.required {
                    errors.push(format!("`{}`: required parameter not supplied", p.name));
                }
                // `!required` with no default is a spec error, caught by
                // `validate_param_specs` at compile — nothing to do here.
            }
        }
    }

    if errors.is_empty() {
        Ok(resolved)
    } else {
        Err(PipelineError::Validation(errors))
    }
}

/// The stringified form of a resolved param value, for a `SCARAB_PARAM_<NAME>`
/// env var (ADR-0008): a string verbatim, everything else its compact JSON.
pub fn stringify(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Compile-time structural validation of a list of declared param specs
/// (ADR-0043 §2). Pushes a diagnostic per problem onto `diagnostics`, prefixed
/// with `label` (e.g. `"interface"` or `` "library `x`" ``):
///
/// - names must be env-safe and unique;
/// - `required: true` with a `default` is nonsensical; `required: false` with no
///   `default` is rejected (optional ⇒ default mandatory, keeping params total);
/// - a `choice` needs a non-empty `options`, and any `default` must be within it;
/// - a non-choice `default` must coerce to the declared type;
/// - a `validate:` expression must parse.
pub fn validate_param_specs(specs: &[ParamSpec], label: &str, diagnostics: &mut Vec<String>) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for p in specs {
        if !crate::is_identifier(&p.name) {
            diagnostics.push(format!(
                "{label}: parameter `{}` is not a valid identifier (must be env-safe)",
                p.name
            ));
        }
        if !seen.insert(p.name.as_str()) {
            diagnostics.push(format!("{label}: duplicate parameter `{}`", p.name));
        }
        if p.required && p.default.is_some() {
            diagnostics.push(format!(
                "{label}: parameter `{}` is `required` but also declares a `default` (remove one)",
                p.name
            ));
        }
        if !p.required && p.default.is_none() {
            diagnostics.push(format!(
                "{label}: optional parameter `{}` must declare a `default`",
                p.name
            ));
        }
        match p.r#type {
            ParamType::Choice => match p.options.as_deref() {
                None | Some([]) => diagnostics.push(format!(
                    "{label}: choice parameter `{}` must declare a non-empty `options` list",
                    p.name
                )),
                Some(opts) => {
                    if let Some(def) = &p.default {
                        match def.as_str() {
                                Some(s) if opts.iter().any(|o| o == s) => {}
                                Some(s) => diagnostics.push(format!(
                                    "{label}: parameter `{}` default `{s}` is not one of its options [{}]",
                                    p.name,
                                    opts.join(", ")
                                )),
                                None => diagnostics.push(format!(
                                    "{label}: choice parameter `{}` default must be a string",
                                    p.name
                                )),
                            }
                    }
                }
            },
            _ => {
                if let Some(def) = &p.default {
                    if let Err(e) = coerce(def, p.r#type) {
                        diagnostics.push(format!("{label}: parameter `{}` default {e}", p.name));
                    }
                }
            }
        }
        if let Some(expr) = &p.validate {
            if let Err(e) = cel::check(expr) {
                diagnostics.push(format!("{label}: parameter `{}` validate {e}", p.name));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str, ty: ParamType) -> ParamSpec {
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

    // --- coerce ---------------------------------------------------------------

    #[test]
    fn coerce_string_from_json_and_number() {
        assert_eq!(
            coerce(&json!("hi"), ParamType::String).unwrap(),
            json!("hi")
        );
        assert_eq!(coerce(&json!(3), ParamType::String).unwrap(), json!("3"));
        // A bool is not a string (fail-closed).
        assert!(coerce(&json!(true), ParamType::String).is_err());
    }

    #[test]
    fn coerce_number_from_typed_and_string() {
        assert_eq!(coerce(&json!(3), ParamType::Number).unwrap(), json!(3));
        assert_eq!(coerce(&json!("3"), ParamType::Number).unwrap(), json!(3));
        assert_eq!(
            coerce(&json!(" 42 "), ParamType::Number).unwrap(),
            json!(42)
        );
        assert!(coerce(&json!("3.5"), ParamType::Number)
            .unwrap()
            .is_number());
        assert!(coerce(&json!("three"), ParamType::Number).is_err());
    }

    #[test]
    fn coerce_boolean_from_typed_and_string_forms() {
        assert_eq!(
            coerce(&json!(true), ParamType::Boolean).unwrap(),
            json!(true)
        );
        assert_eq!(
            coerce(&json!("true"), ParamType::Boolean).unwrap(),
            json!(true)
        );
        assert_eq!(
            coerce(&json!("YES"), ParamType::Boolean).unwrap(),
            json!(true)
        );
        assert_eq!(
            coerce(&json!("no"), ParamType::Boolean).unwrap(),
            json!(false)
        );
        assert!(coerce(&json!("maybe"), ParamType::Boolean).is_err());
        assert!(coerce(&json!(1), ParamType::Boolean).is_err());
    }

    #[test]
    fn coerce_choice_requires_a_string() {
        assert_eq!(coerce(&json!("a"), ParamType::Choice).unwrap(), json!("a"));
        assert!(coerce(&json!(1), ParamType::Choice).is_err());
    }

    // --- resolve_params -------------------------------------------------------

    fn iface(inputs: Vec<ParamSpec>) -> Interface {
        Interface {
            inputs,
            outputs: vec![],
        }
    }

    #[test]
    fn resolve_applies_defaults_for_absent_optionals() {
        let mut replicas = spec("replicas", ParamType::Number);
        replicas.required = false;
        replicas.default = Some(json!(2));
        let i = iface(vec![spec("region", ParamType::String), replicas]);

        let supplied = BTreeMap::from([("region".to_string(), json!("us-east-1"))]);
        let out = resolve_params(&i, &supplied).unwrap();
        assert_eq!(out["region"], json!("us-east-1"));
        assert_eq!(out["replicas"], json!(2)); // default applied, typed as number
    }

    #[test]
    fn resolve_rejects_missing_required() {
        let i = iface(vec![spec("region", ParamType::String)]);
        let err = resolve_params(&i, &BTreeMap::new()).unwrap_err();
        assert!(
            err.to_string().contains("required parameter not supplied"),
            "{err}"
        );
    }

    #[test]
    fn resolve_coerces_supplied_string_to_number() {
        let i = iface(vec![spec("n", ParamType::Number)]);
        let supplied = BTreeMap::from([("n".to_string(), json!("81"))]);
        let out = resolve_params(&i, &supplied).unwrap();
        assert_eq!(out["n"], json!(81));
    }

    #[test]
    fn resolve_rejects_choice_outside_options() {
        let mut env = spec("env", ParamType::Choice);
        env.options = Some(vec!["staging".into(), "prod".into()]);
        let i = iface(vec![env]);
        let supplied = BTreeMap::from([("env".to_string(), json!("dev"))]);
        let err = resolve_params(&i, &supplied).unwrap_err();
        assert!(
            err.to_string().contains("not one of the allowed choices"),
            "{err}"
        );
    }

    #[test]
    fn resolve_enforces_validate_predicate_fail_closed() {
        let mut n = spec("n", ParamType::Number);
        n.validate = Some("value > 0".into());
        let i = iface(vec![n]);
        assert!(resolve_params(&i, &BTreeMap::from([("n".to_string(), json!("5"))])).is_ok());
        let err =
            resolve_params(&i, &BTreeMap::from([("n".to_string(), json!("-1"))])).unwrap_err();
        assert!(err.to_string().contains("failed validation"), "{err}");
    }

    #[test]
    fn resolve_rejects_unknown_param() {
        let i = iface(vec![spec("region", ParamType::String)]);
        let supplied = BTreeMap::from([
            ("region".to_string(), json!("x")),
            ("bogus".to_string(), json!("y")),
        ]);
        let err = resolve_params(&i, &supplied).unwrap_err();
        assert!(
            err.to_string().contains("`bogus`: unknown parameter"),
            "{err}"
        );
    }

    // --- validate_param_specs (§2) -------------------------------------------

    fn diags(specs: &[ParamSpec]) -> Vec<String> {
        let mut d = Vec::new();
        validate_param_specs(specs, "interface", &mut d);
        d
    }

    #[test]
    fn required_with_default_is_rejected() {
        let mut p = spec("x", ParamType::String);
        p.default = Some(json!("d"));
        assert!(diags(&[p])
            .iter()
            .any(|m| m.contains("also declares a `default`")));
    }

    #[test]
    fn optional_without_default_is_rejected() {
        let mut p = spec("x", ParamType::String);
        p.required = false;
        assert!(diags(&[p])
            .iter()
            .any(|m| m.contains("must declare a `default`")));
    }

    #[test]
    fn choice_without_options_is_rejected() {
        let d = diags(&[spec("x", ParamType::Choice)]);
        assert!(d.iter().any(|m| m.contains("non-empty `options`")), "{d:?}");
    }

    #[test]
    fn choice_default_outside_options_is_rejected() {
        let mut p = spec("x", ParamType::Choice);
        p.required = false;
        p.options = Some(vec!["a".into(), "b".into()]);
        p.default = Some(json!("c"));
        assert!(diags(&[p])
            .iter()
            .any(|m| m.contains("is not one of its options")));
    }

    #[test]
    fn duplicate_and_non_env_safe_names_are_rejected() {
        let d = diags(&[spec("ok", ParamType::String), spec("ok", ParamType::String)]);
        assert!(d.iter().any(|m| m.contains("duplicate parameter")), "{d:?}");
        let d = diags(&[spec("1bad", ParamType::String)]);
        assert!(
            d.iter().any(|m| m.contains("not a valid identifier")),
            "{d:?}"
        );
    }

    #[test]
    fn unparsable_validate_expression_is_rejected() {
        let mut p = spec("x", ParamType::Number);
        p.validate = Some("value >".into());
        assert!(diags(&[p]).iter().any(|m| m.contains("validate")));
    }

    #[test]
    fn well_formed_specs_produce_no_diagnostics() {
        let mut choice = spec("env", ParamType::Choice);
        choice.required = false;
        choice.options = Some(vec!["staging".into(), "prod".into()]);
        choice.default = Some(json!("staging"));
        let mut n = spec("n", ParamType::Number);
        n.validate = Some("value >= 0".into());
        assert!(diags(&[spec("region", ParamType::String), choice, n]).is_empty());
    }
}
