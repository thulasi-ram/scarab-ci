//! Evaluating a checked rule with `cel` 0.15, failing closed.
//!
//! The input is the checkpoint's Rust value, serialised and converted to CEL
//! values under the same schema the rule was checked against: structs become
//! maps, an absent optional field is a missing key (so `has()` sees it), an
//! absent optional variable is `null`, and a [`Timestamp`](crate::schema::Timestamp)
//! becomes a CEL `timestamp`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cel::common::ast::{EntryExpr, Expr, IdedEntryExpr, IdedExpr};
use cel::extractors::This;
use cel::objects::{Key, Map};
use cel::{Context, ExecutionError, Value};

use crate::check::{check, check_limits, on_cel_stack, parse, Checked, Diagnostic};
use crate::schema::{Schema, Type};

/// One rule of a Policy, checked. Its message is static (ADR-0075 §4): the
/// decision stores the input, which shows what the rule matched.
#[derive(Debug)]
pub struct Rule {
    pub when: Checked,
    pub message: String,
}

impl Rule {
    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.when.warnings.iter()
    }
}

/// Checks a rule's `when`, which must return a bool.
pub fn compile_rule(schema: &Schema, when: &str, message: &str) -> Result<Rule, Vec<Diagnostic>> {
    let when = check(schema, when, &Type::Bool)?;
    Ok(Rule {
        when,
        message: message.to_string(),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuleOutcome {
    /// `when` was false: the rule's effect does not apply.
    DoesNotApply,
    /// `when` was true: the rule's effect applies, with this message.
    Applies { message: String },
    /// Evaluation failed. Whatever the rule's effect, the decision is deny.
    DeniedOnError { message: String, error: String },
}

/// Evaluates `rule` against `input`. Any error, in binding the input or in
/// evaluating `when`, denies (ADR-0075 §6). Runs on the large-stack thread
/// ([`on_cel_stack`]).
pub fn evaluate_rule<I: serde::Serialize + Sync>(
    schema: &Schema,
    rule: &Rule,
    input: &I,
) -> RuleOutcome {
    let denied = |error: String| RuleOutcome::DeniedOnError {
        message: rule.message.clone(),
        error,
    };
    let context = || bind(schema, input);
    match on_cel_stack(|| {
        let context = context().map_err(|e| format!("input does not match the schema: {e}"))?;
        evaluate_when(&rule.when.source, &context)
    }) {
        Ok(Ok(true)) => RuleOutcome::Applies {
            message: rule.message.clone(),
        },
        Ok(Ok(false)) => RuleOutcome::DoesNotApply,
        Ok(Err(e)) | Err(e) => denied(e),
    }
}

/// Parses a checked `when` again and evaluates it against `context`. Call it
/// only on the [`on_cel_stack`] thread: the parsed program is dropped here.
///
/// Each `x.matches(pattern)` is evaluated through the context's
/// [`MATCHES_FN`] (see [`bind`]), not `cel`'s own `matches`, which compiles
/// its pattern on every call.
pub(crate) fn evaluate_when(source: &str, context: &Context<'_, '_>) -> Result<bool, String> {
    check_limits(source).map_err(|d| d[0].message.clone())?;
    let program = parse(source).map_err(|d| d[0].message.clone())?;
    let mut expr = program.expression().clone();
    drop(program);
    route_matches(&mut expr);
    match Value::resolve(&expr, context) {
        Ok(Value::Bool(b)) => Ok(b),
        Ok(other) => Err(format!("`when` returned {other:?}, not a bool")),
        Err(e) => Err(e.to_string()),
    }
}

/// The name a rule's `matches` is evaluated under. It is not an identifier,
/// so no rule can call it by name; the checker refuses unknown functions
/// besides.
const MATCHES_FN: &str = "@scarab.matches";

/// How many regexes evaluation has compiled in this process: what the
/// regression test for one compile per pattern per evaluation counts.
static REGEX_COMPILES: AtomicU64 = AtomicU64::new(0);

#[doc(hidden)]
pub fn regex_compiles() -> u64 {
    REGEX_COMPILES.load(Ordering::Relaxed)
}

/// Renames every `x.matches(pattern)` call to [`MATCHES_FN`]. `cel` resolves
/// a member overload ahead of a context function, so the call has to be
/// renamed for the context's `matches` to be the one called.
fn route_matches(e: &mut IdedExpr) {
    match &mut e.expr {
        Expr::Call(call) => {
            if call.func_name == "matches" && call.target.is_some() && call.args.len() == 1 {
                call.func_name = MATCHES_FN.to_string();
            }
            if let Some(target) = call.target.as_deref_mut() {
                route_matches(target);
            }
            call.args.iter_mut().for_each(route_matches);
        }
        Expr::Select(sel) => route_matches(&mut sel.operand),
        Expr::Comprehension(c) => {
            for part in [
                &mut c.iter_range,
                &mut c.accu_init,
                &mut c.loop_cond,
                &mut c.loop_step,
                &mut c.result,
            ] {
                route_matches(part);
            }
        }
        Expr::List(list) => list.elements.iter_mut().for_each(route_matches),
        Expr::Map(map) => map.entries.iter_mut().for_each(route_entry),
        Expr::Struct(st) => st.entries.iter_mut().for_each(route_entry),
        Expr::Literal(_) | Expr::Ident(_) | Expr::Unspecified => {}
    }
}

fn route_entry(entry: &mut IdedEntryExpr) {
    match &mut entry.expr {
        EntryExpr::MapEntry(m) => {
            route_matches(&mut m.key);
            route_matches(&mut m.value);
        }
        EntryExpr::StructField(f) => route_matches(&mut f.value),
    }
}

/// A rule's `matches`: each distinct pattern is compiled once per bound
/// context, under the size limit the checker compiled it with, and reused
/// for every later call. A rule iterating 512 Steps × 8 origins compiles its
/// pattern once, not 4,096 times.
fn matches_fn() -> impl Fn(This<Arc<String>>, Arc<String>) -> Result<Value, ExecutionError> {
    let compiled: Mutex<HashMap<String, Result<regex::Regex, String>>> = Mutex::default();
    move |This(text), pattern| {
        let mut compiled = compiled.lock().unwrap_or_else(|e| e.into_inner());
        let re = compiled.entry(pattern.to_string()).or_insert_with(|| {
            REGEX_COMPILES.fetch_add(1, Ordering::Relaxed);
            crate::check::pattern(&pattern).map_err(|e| e.to_string())
        });
        match re {
            Ok(re) => Ok(Value::Bool(re.is_match(&text))),
            Err(e) => Err(ExecutionError::function_error(
                "matches",
                format!("'{pattern}' is not a valid regex: {e}"),
            )),
        }
    }
}

/// Binds each top-level field of `input` as a CEL variable.
pub fn bind<I: serde::Serialize>(
    schema: &Schema,
    input: &I,
) -> Result<Context<'static, 'static>, String> {
    let json = serde_json::to_value(input).map_err(|e| e.to_string())?;
    let serde_json::Value::Object(obj) = json else {
        return Err("the input is not a struct".to_string());
    };
    let mut context = Context::default();
    context
        .add_function(MATCHES_FN, matches_fn())
        .map_err(|e| format!("cannot declare `matches`: {e}"))?;
    for var in &schema.variables {
        let value = match obj.get(&var.name) {
            None | Some(serde_json::Value::Null) if var.optional => Value::Null,
            None | Some(serde_json::Value::Null) => {
                return Err(format!("`{}` is required", var.name));
            }
            Some(v) => to_value(schema, &var.ty, v).map_err(|e| format!("{}{e}", var.name))?,
        };
        context.add_variable_from_value(var.name.clone(), value);
    }
    Ok(context)
}

/// Converts `json` to a CEL value of type `ty`. Errors carry the path below
/// the value, e.g. `.target.kind: ...`.
fn to_value(schema: &Schema, ty: &Type, json: &serde_json::Value) -> Result<Value, String> {
    use serde_json::Value as J;
    Ok(match (ty, json) {
        (Type::Bool, J::Bool(b)) => Value::Bool(*b),
        (Type::String, J::String(s)) => Value::String(Arc::new(s.clone())),
        (Type::Int, J::Number(n)) if n.is_i64() => Value::Int(n.as_i64().unwrap_or_default()),
        (Type::Uint, J::Number(n)) if n.is_u64() => Value::UInt(n.as_u64().unwrap_or_default()),
        (Type::Double, J::Number(n)) => Value::Float(n.as_f64().unwrap_or_default()),
        (Type::Timestamp, J::String(s)) => {
            Value::Timestamp(chrono::DateTime::parse_from_rfc3339(s).map_err(|e| format!(": {e}"))?)
        }
        (Type::Enum(name), J::String(s)) => {
            if !schema.enum_decl(name).variants.contains(s) {
                return Err(format!(": \"{s}\" is not a {name}"));
            }
            Value::String(Arc::new(s.clone()))
        }
        (Type::List { elem, cap }, J::Array(items)) => {
            if items.len() as u64 > *cap {
                return Err(format!(": {} items, above the cap of {cap}", items.len()));
            }
            let items = items
                .iter()
                .enumerate()
                .map(|(i, v)| to_value(schema, elem, v).map_err(|e| format!("[{i}]{e}")))
                .collect::<Result<Vec<_>, _>>()?;
            Value::List(Arc::new(items))
        }
        (Type::Struct(name), J::Object(obj)) => {
            let decl = schema.struct_decl(name);
            let mut map = HashMap::new();
            for field in &decl.fields {
                match obj.get(&field.name) {
                    None | Some(J::Null) if field.optional => {}
                    None | Some(J::Null) => return Err(format!(".{}: required", field.name)),
                    Some(v) => {
                        let v = to_value(schema, &field.ty, v)
                            .map_err(|e| format!(".{}{e}", field.name))?;
                        map.insert(Key::String(Arc::new(field.name.clone())), v);
                    }
                }
            }
            Value::Map(Map { map: Arc::new(map) })
        }
        (ty, v) => return Err(format!(": expected {ty}, got {v}")),
    })
}
