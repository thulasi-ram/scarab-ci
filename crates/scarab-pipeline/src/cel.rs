//! CEL (Common Expression Language) binding for the pipeline DSL.
//!
//! CEL is the single expression language for `when:` guards, `${{ … }}` string
//! interpolation, and matrix predicates (ADR-0009). It is **total** (always
//! terminates), typed, and sandboxed — the deliberate antidote to GHA's
//! `${{ }}` Turing-tarpit. The evaluator (`cel-interpreter`) is pure
//! computation — no I/O, no clock, no RNG — so it lives directly in this pure
//! crate, no port/adapter ceremony (ADR-0031).
//!
//! Expressions evaluate against a **JSON object context**: each top-level key
//! becomes a CEL variable (e.g. `{ "event": {…}, "matrix": {…} }` → `event`,
//! `matrix`). Bad expressions are caught at **submit time** by [`check`], never
//! mid-run.

use cel_interpreter::{Context, Program};

use crate::PipelineError;

/// Compile a CEL expression, containing the panic hazard in the parser.
///
/// `cel-parser` (antlr4rust 0.3.0-rc2) reaches an `unreachable!()` on some
/// malformed inputs — e.g. a trailing binary operator (`1 +`) — instead of
/// returning a `ParseError`. A malformed *user* pipeline must be a submit-time
/// validation error, never a control-plane crash (the durability ethos), so we
/// catch the unwind and turn it into [`PipelineError::Cel`]. (The parser's own
/// stderr note on the caught panic is harmless.)
fn compile(expr: &str) -> Result<Program, PipelineError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Program::compile(expr))) {
        Ok(Ok(program)) => Ok(program),
        Ok(Err(e)) => Err(PipelineError::Cel(format!("`{expr}`: {e}"))),
        Err(_) => Err(PipelineError::Cel(format!(
            "`{expr}`: malformed expression"
        ))),
    }
}

/// Parse-check a CEL expression without evaluating it. Used at submit time so a
/// malformed `when:` / interpolation is rejected before a Run ever starts.
pub fn check(expr: &str) -> Result<(), PipelineError> {
    compile(expr).map(|_| ())
}

/// Check every `${{ … }}` expression embedded in `template` (and that each is
/// terminated), without evaluating them.
pub fn check_interpolation(template: &str) -> Result<(), PipelineError> {
    for expr in interpolations(template)? {
        check(expr)?;
    }
    Ok(())
}

/// Evaluate `expr` against a JSON object `ctx` (each top-level key becomes a CEL
/// variable), returning the result as JSON.
pub fn eval(expr: &str, ctx: &serde_json::Value) -> Result<serde_json::Value, PipelineError> {
    let program = compile(expr)?;
    let mut context = Context::default();
    bind(&mut context, ctx, expr)?;
    let value = program
        .execute(&context)
        .map_err(|e| PipelineError::Cel(format!("`{expr}`: {e}")))?;
    value
        .json()
        .map_err(|e| PipelineError::Cel(format!("`{expr}`: {e:?}")))
}

/// Evaluate `expr` as a boolean guard (for `when:` and matrix predicates).
/// Anything that does not resolve to a bool is an error.
pub fn eval_bool(expr: &str, ctx: &serde_json::Value) -> Result<bool, PipelineError> {
    match eval(expr, ctx)? {
        serde_json::Value::Bool(b) => Ok(b),
        other => Err(PipelineError::Cel(format!(
            "`{expr}`: expected a boolean, got `{other}`"
        ))),
    }
}

/// Resolve every `${{ <cel> }}` interpolation in `template` against `ctx`.
/// String results are inserted raw (no quotes); other JSON values use their
/// compact JSON form.
pub fn interpolate(template: &str, ctx: &serde_json::Value) -> Result<String, PipelineError> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("${{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        let end = after.find("}}").ok_or_else(|| {
            PipelineError::Cel(format!("unterminated `${{{{` in `{template}`"))
        })?;
        let expr = after[..end].trim();
        out.push_str(&render(&eval(expr, ctx)?));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Extract the (trimmed) expressions inside each `${{ … }}` in `template`,
/// erroring if any is unterminated.
pub(crate) fn interpolations(template: &str) -> Result<Vec<&str>, PipelineError> {
    let mut exprs = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("${{") {
        let after = &rest[start + 3..];
        let end = after.find("}}").ok_or_else(|| {
            PipelineError::Cel(format!("unterminated `${{{{` in `{template}`"))
        })?;
        exprs.push(after[..end].trim());
        rest = &after[end + 2..];
    }
    Ok(exprs)
}

fn render(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Bind each key of a JSON object `ctx` as a CEL variable. A null context binds
/// nothing; any non-object context is an error.
fn bind(context: &mut Context, ctx: &serde_json::Value, expr: &str) -> Result<(), PipelineError> {
    match ctx {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                context
                    .add_variable(k.clone(), v.clone())
                    .map_err(|e| PipelineError::Cel(format!("`{expr}`: binding `{k}`: {e}")))?;
            }
            Ok(())
        }
        serde_json::Value::Null => Ok(()),
        _ => Err(PipelineError::Cel(format!(
            "`{expr}`: context must be a JSON object"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn eval_bool_reads_context_variables() {
        let ctx = json!({ "event": { "branch": "main" } });
        assert!(eval_bool("event.branch == 'main'", &ctx).unwrap());
        assert!(!eval_bool("event.branch == 'dev'", &ctx).unwrap());
    }

    #[test]
    fn non_boolean_guard_is_an_error() {
        let ctx = json!({});
        assert!(eval_bool("1 + 1", &ctx).is_err());
    }

    #[test]
    fn interpolation_resolves_strings_and_numbers() {
        let ctx = json!({ "matrix": { "os": "linux" }, "n": 3 });
        assert_eq!(
            interpolate("build-${{ matrix.os }}-${{ n }}", &ctx).unwrap(),
            "build-linux-3"
        );
        // No interpolation → passthrough.
        assert_eq!(interpolate("plain", &ctx).unwrap(), "plain");
    }

    #[test]
    fn malformed_expression_fails_the_check() {
        assert!(check("1 +").is_err());
        assert!(check("event.branch == 'main'").is_ok());
    }

    #[test]
    fn unterminated_interpolation_is_rejected() {
        assert!(check_interpolation("oops ${{ x ").is_err());
        assert!(check_interpolation("ok ${{ x }} done").is_ok());
    }
}
