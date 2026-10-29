//! CEL (Common Expression Language) binding for the pipeline DSL.
//!
//! CEL is the single expression language for `when:` guards, `${{ … }}` string
//! interpolation, and matrix predicates (ADR-0009). It is **total** (always
//! terminates), typed, and sandboxed — the deliberate antidote to GHA's
//! `${{ }}` Turing-tarpit. The evaluator (`cel`) is pure
//! computation — no I/O, no clock, no RNG — so it lives directly in this pure
//! crate, no port/adapter ceremony (ADR-0031).
//!
//! Expressions evaluate against a **JSON object context**: each top-level key
//! becomes a CEL variable (e.g. `{ "event": {…}, "matrix": {…} }` → `event`,
//! `matrix`). Bad expressions are caught at **submit time** by [`check`], never
//! mid-run.

use cel::{Context, Program};

use crate::PipelineError;

/// The longest CEL expression accepted, in bytes. The same cap ADR-0075 sets
/// for policy rules. Applies to each `when:`, each matrix `exclude`, each
/// parameter `validate`, and each `${{ … }}` on its own.
pub const MAX_EXPRESSION_BYTES: usize = 4096;

/// The deepest bracket nesting (`(`, `[`, `{` combined) accepted in a CEL
/// expression. Brackets inside string literals and `//` comments do not count.
pub const MAX_EXPRESSION_NESTING: usize = 64;

/// Stack size of the thread that parses and evaluates CEL.
///
/// The parser (antlr4rust) and the evaluator recurse once per
/// nesting level *and* once per link of a left-recursive chain (`1+1+1…`,
/// `a.b.b…`, `l[0][0]…`). A stack overflow aborts the whole process, and
/// `catch_unwind` cannot stop it, so an expression a repository author wrote
/// could take down the control plane. The caps above bound the depth; this
/// stack holds the deepest expression they allow. Measured worst case for a
/// 4 KiB expression (a 2,048-link `1+1+…` chain, parse + evaluate + drop):
/// about 1.3 MiB in release and about 70 MiB in an unoptimised (debug) build,
/// where the antlr parser's frames are much larger (a 64-deep `((…))` alone
/// takes about 9 MiB there). A tokio worker thread has 2 MiB: release would
/// leave little room for the caller's frames, and debug does not fit. The
/// stack is reserved address space; only the pages an expression touches are
/// committed. Measured cost of the extra thread: about 57 µs per call in
/// release on an M-series Mac (69 µs against 12 µs inline for a two-clause
/// guard), which is small next to the Postgres writes around each caller.
/// These figures were measured on `cel-interpreter` 0.10; the limit tests
/// below run every worst-case shape on `cel` 0.15 under this stack.
/// `scarab-policy` parses and evaluates Policy rules under the same stack.
pub const CEL_STACK_BYTES: usize = if cfg!(debug_assertions) {
    256 << 20
} else {
    64 << 20
};

/// The expression as quoted in an error: whole when short, else its start.
fn shown(expr: &str) -> String {
    const MAX: usize = 80;
    if expr.len() <= MAX {
        return expr.to_string();
    }
    let mut end = MAX;
    while !expr.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &expr[..end])
}

/// Reject an expression the parser could not handle safely, before parsing
/// it: longer than [`MAX_EXPRESSION_BYTES`], or with brackets nested deeper
/// than [`MAX_EXPRESSION_NESTING`].
fn check_limits(expr: &str) -> Result<(), PipelineError> {
    if expr.len() > MAX_EXPRESSION_BYTES {
        return Err(PipelineError::Cel(format!(
            "`{}`: expression is {} bytes; the limit is {MAX_EXPRESSION_BYTES} bytes",
            shown(expr),
            expr.len()
        )));
    }
    let depth = max_nesting(expr);
    if depth > MAX_EXPRESSION_NESTING {
        return Err(PipelineError::Cel(format!(
            "`{}`: brackets nest {depth} deep; the limit is {MAX_EXPRESSION_NESTING}",
            shown(expr)
        )));
    }
    Ok(())
}

/// The deepest `(` / `[` / `{` nesting in `expr`, skipping string and bytes
/// literals (CEL's `'…'`, `"…"`, triple-quoted, and `r`/`b`-prefixed forms)
/// and `//` comments, which is where the parser does not see brackets either.
/// `scarab-policy` applies the same pre-check to Policy rules.
pub fn max_nesting(expr: &str) -> usize {
    let b = expr.as_bytes();
    let (mut i, mut depth, mut max) = (0, 0usize, 0usize);
    while i < b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => {
                depth += 1;
                max = max.max(depth);
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            q @ (b'\'' | b'"') => i = skip_string(b, i, q),
            _ => i += 1,
        }
    }
    max
}

/// Given `b[open]` is a quote `q`, return the index just past the literal it
/// opens (or `b.len()` if it is unterminated). Follows the CEL lexer: a raw
/// literal (`r`/`R` prefix, possibly with `b`/`B`) has no escapes; a
/// single-quoted literal ends at a newline; a triple-quoted one spans lines.
fn skip_string(b: &[u8], open: usize, q: u8) -> usize {
    // The prefix letters directly before the quote, if they are not the tail
    // of a longer identifier (`bar'x'` is the identifier `bar` then `'x'`).
    let mut p = open;
    while p > 0 && open - p < 2 && matches!(b[p - 1], b'r' | b'R' | b'b' | b'B') {
        p -= 1;
    }
    let prefix_ok = p == 0 || !(b[p - 1].is_ascii_alphanumeric() || b[p - 1] == b'_');
    let raw = prefix_ok && b[p..open].iter().any(|c| matches!(c, b'r' | b'R'));
    let triple = b.get(open + 1) == Some(&q) && b.get(open + 2) == Some(&q);
    let mut i = open + if triple { 3 } else { 1 };
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && !raw {
            i += 2;
        } else if c == q && (!triple || (b.get(i + 1) == Some(&q) && b.get(i + 2) == Some(&q))) {
            return i + if triple { 3 } else { 1 };
        } else if !triple && (c == b'\n' || c == b'\r') {
            return i;
        } else {
            i += 1;
        }
    }
    b.len()
}

/// Compile `expr` and run `f` on the result, on a thread whose stack holds
/// the deepest expression the limits allow (see [`CEL_STACK_BYTES`]). The
/// program is also dropped there: dropping its tree recurses too.
///
/// It also contains any panic in the parser. `cel-parser` 0.10 (antlr4rust
/// 0.3.0-rc2) reached an `unreachable!()` on some malformed inputs, e.g. a
/// trailing binary operator (`1 +`), instead of returning a `ParseError`.
/// `cel` 0.15 (antlr4rust 0.6) returns an error for every malformed input we
/// have tried, `1 +` included, but the parser is still generated code over a
/// third-party runtime. A malformed *user* pipeline must be a submit-time
/// validation error, never a control-plane crash, so a panic on the CEL thread
/// becomes [`PipelineError::Cel`].
fn with_program<T: Send>(
    expr: &str,
    f: impl FnOnce(&Program) -> Result<T, PipelineError> + Send,
) -> Result<T, PipelineError> {
    check_limits(expr)?;
    let run = || {
        let program = Program::compile(expr)
            .map_err(|e| PipelineError::Cel(format!("`{}`: {e}", shown(expr))))?;
        f(&program)
    };
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("scarab-cel".into())
            .stack_size(CEL_STACK_BYTES)
            .spawn_scoped(scope, run)
            .map_err(|e| {
                PipelineError::Cel(format!(
                    "`{}`: cannot start the CEL thread: {e}",
                    shown(expr)
                ))
            })?;
        handle.join().unwrap_or_else(|_| {
            Err(PipelineError::Cel(format!(
                "`{}`: malformed expression",
                shown(expr)
            )))
        })
    })
}

/// Parse-check a CEL expression without evaluating it. Used at submit time so a
/// malformed `when:` / interpolation is rejected before a Run ever starts.
pub fn check(expr: &str) -> Result<(), PipelineError> {
    with_program(expr, |_| Ok(()))
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
    with_program(expr, |program| {
        let mut context = Context::default();
        bind(&mut context, ctx, expr)?;
        let value = program
            .execute(&context)
            .map_err(|e| PipelineError::Cel(format!("`{}`: {e}", shown(expr))))?;
        value
            .json()
            .map_err(|e| PipelineError::Cel(format!("`{}`: {e:?}", shown(expr))))
    })
}

/// Evaluate `expr` as a boolean guard (for `when:` and matrix predicates).
/// Anything that does not resolve to a bool is an error.
pub fn eval_bool(expr: &str, ctx: &serde_json::Value) -> Result<bool, PipelineError> {
    match eval(expr, ctx)? {
        serde_json::Value::Bool(b) => Ok(b),
        other => Err(PipelineError::Cel(format!(
            "`{}`: expected a boolean, got `{other}`",
            shown(expr)
        ))),
    }
}

/// Evaluate the guard `expr` against each of `ctxs`, parsing it once: the
/// result of [`eval_bool`] for each context, in order, or its first error.
/// Every context must bind the same keys.
pub(crate) fn eval_bool_each(
    expr: &str,
    ctxs: &[&serde_json::Value],
) -> Result<Vec<bool>, PipelineError> {
    with_program(expr, |program| {
        // One context: building one registers the standard functions, which
        // costs more than the evaluation. Each binding replaces the last; the
        // contexts given share their keys (a matrix coordinate's dimensions).
        let mut context = Context::default();
        ctxs.iter()
            .map(|ctx| {
                bind(&mut context, ctx, expr)?;
                let value = program
                    .execute(&context)
                    .map_err(|e| PipelineError::Cel(format!("`{}`: {e}", shown(expr))))?;
                match value
                    .json()
                    .map_err(|e| PipelineError::Cel(format!("`{}`: {e:?}", shown(expr))))?
                {
                    serde_json::Value::Bool(b) => Ok(b),
                    other => Err(PipelineError::Cel(format!(
                        "`{}`: expected a boolean, got `{other}`",
                        shown(expr)
                    ))),
                }
            })
            .collect()
    })
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
        let end = after
            .find("}}")
            .ok_or_else(|| PipelineError::Cel(format!("unterminated `${{{{` in `{template}`")))?;
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
        let end = after
            .find("}}")
            .ok_or_else(|| PipelineError::Cel(format!("unterminated `${{{{` in `{template}`")))?;
        exprs.push(after[..end].trim());
        rest = &after[end + 2..];
    }
    Ok(exprs)
}

/// One read of the `inputs` variable in a CEL expression (ADR-0076 §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputRead {
    /// Byte range of the read: `inputs.<name>`, or `inputs` alone, with the
    /// leading `.` of a root-qualified read.
    pub start: usize,
    pub end: usize,
    /// `<name>` when read as `inputs.<name>`; `None` for any other use
    /// (`inputs["x"]`, `inputs` passed whole).
    pub field: Option<String>,
}

/// Each read of the top-level `inputs` variable in `expr`, in order: `inputs`
/// and CEL's root-qualified `.inputs`. Skips string literals and `//`
/// comments, and a field that happens to be named `inputs` (`x.inputs`).
/// Whether a `.` selects a field or qualifies a root name depends on the
/// token before it, which is found by skipping whitespace and comments, never
/// by reading the text before the `.`.
pub(crate) fn input_reads(expr: &str) -> Vec<InputRead> {
    /// The last token outside whitespace, comments and string contents.
    #[derive(Clone, Copy, PartialEq)]
    enum Last {
        /// Start of the expression, an operator or an opening bracket.
        Other,
        /// The end of an operand: a name, a number, a literal or a closing
        /// bracket. A `.` after it selects a field.
        Operand,
        /// A `.` at byte `at`; `select` when it follows an operand.
        Dot { at: usize, select: bool },
    }
    let b = expr.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut out = Vec::new();
    let mut last = Last::Other;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            c if c.is_ascii_whitespace() => i += 1,
            q @ (b'\'' | b'"') => {
                i = skip_string(b, i, q);
                last = Last::Operand;
            }
            b'.' => {
                last = Last::Dot {
                    at: i,
                    select: last == Last::Operand,
                };
                i += 1;
            }
            b')' | b']' | b'}' => {
                last = Last::Operand;
                i += 1;
            }
            c if ident(c) => {
                let mut start = i;
                while i < b.len() && ident(b[i]) {
                    i += 1;
                }
                let before = last;
                last = Last::Operand;
                if &expr[start..i] != "inputs" || c.is_ascii_digit() {
                    continue;
                }
                match before {
                    // `x.inputs`: a field, not the variable.
                    Last::Dot { select: true, .. } => continue,
                    // `.inputs`: the variable, qualified from the root.
                    Last::Dot { at, select: false } => start = at,
                    _ => {}
                }
                let mut field = None;
                if b.get(i) == Some(&b'.') {
                    let f = i + 1;
                    let mut e = f;
                    while e < b.len() && ident(b[e]) {
                        e += 1;
                    }
                    if e > f && !b[f].is_ascii_digit() {
                        field = Some(expr[f..e].to_string());
                        i = e;
                    }
                }
                out.push(InputRead {
                    start,
                    end: i,
                    field,
                });
            }
            _ => {
                last = Last::Other;
                i += 1;
            }
        }
    }
    out
}

/// Does `expr` read the variable `name`, as the CEL parser sees it? A
/// root-qualified read (`.name`) counts. This is the parser's own answer, not
/// a scan of the text: [`input_reads`] is checked against it, so a form the
/// scan misses is refused instead of left unbound. An expression that does
/// not parse is an error.
pub(crate) fn reads_variable(expr: &str, name: &str) -> Result<bool, PipelineError> {
    with_program(expr, |program| {
        Ok(program
            .references()
            .variables()
            .iter()
            .any(|v| v.strip_prefix('.').unwrap_or(v) == name))
    })
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
                context.add_variable(k.clone(), v.clone()).map_err(|e| {
                    PipelineError::Cel(format!("`{}`: binding `{k}`: {e}", shown(expr)))
                })?;
            }
            Ok(())
        }
        serde_json::Value::Null => Ok(()),
        _ => Err(PipelineError::Cel(format!(
            "`{}`: context must be a JSON object",
            shown(expr)
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

    // --- size and nesting limits (git-bug af7ef4d) --------------------------
    //
    // A stack overflow in the CEL parser or evaluator aborts the process, so
    // every case below runs on a 2 MiB thread — the stack of a tokio worker,
    // which is where the server calls into this module. Before the fix, each
    // of these aborted the test binary (in debug; the 1,000-paren case also in
    // release) instead of failing.

    /// Run `f` on a thread with a tokio worker's default stack.
    fn on_worker_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }

    /// `head` followed by as many `unit`s as fit in `len` bytes.
    fn chain(head: &str, unit: &str, len: usize) -> String {
        let mut s = head.to_string();
        while s.len() + unit.len() <= len {
            s.push_str(unit);
        }
        s
    }

    fn nested(open: &str, close: &str, depth: usize) -> String {
        format!("{}1{}", open.repeat(depth), close.repeat(depth))
    }

    fn limits_ctx() -> serde_json::Value {
        json!({ "t": true, "f": false, "x": 1, "l": [[1]], "a": {} })
    }

    #[test]
    fn nesting_at_the_limit_evaluates_and_one_more_is_rejected() {
        on_worker_stack(|| {
            let ctx = limits_ctx();
            let at = nested("(", ")", MAX_EXPRESSION_NESTING);
            assert_eq!(eval(&at, &ctx).unwrap(), json!(1));
            let lists = nested("[", "]", MAX_EXPRESSION_NESTING);
            assert!(eval(&lists, &ctx).is_ok());

            for over in [
                nested("(", ")", MAX_EXPRESSION_NESTING + 1),
                nested("[", "]", MAX_EXPRESSION_NESTING + 1),
                // The shape that aborted a release build of cel 0.10.
                nested("(", ")", 1000),
            ] {
                let err = check(&over).unwrap_err().to_string();
                assert!(err.contains("the limit is 64"), "{err}");
            }
            // Mixed bracket kinds count together.
            let mixed = format!("{}1{}", "([".repeat(33), "])".repeat(33));
            assert!(check(&mixed)
                .unwrap_err()
                .to_string()
                .contains("nest 66 deep"));
        });
    }

    #[test]
    fn longest_left_recursive_chains_evaluate_without_overflowing() {
        on_worker_stack(|| {
            let ctx = limits_ctx();
            let max = MAX_EXPRESSION_BYTES;
            let links = (max as u64 - 1) / 2 + 1;
            // `1+1+…` is the deepest per byte: one parser level per 2 bytes.
            assert_eq!(eval(&chain("1", "+1", max), &ctx).unwrap(), json!(links));
            assert_eq!(eval(&chain("x", "+x", max), &ctx).unwrap(), json!(links));
            assert_eq!(eval(&chain("1", "*1", max), &ctx).unwrap(), json!(1));
            assert_eq!(eval(&chain("t", "&&t", max), &ctx).unwrap(), json!(true));
            assert_eq!(eval(&chain("f", "||f", max), &ctx).unwrap(), json!(false));
            // Member, index and comparison chains parse; evaluating them fails
            // at the first missing field or type mismatch — an error, not an abort.
            for deep in [
                chain("a", ".b", max),
                chain("l", "[0]", max),
                chain("1", "<1", max),
            ] {
                check(&deep).unwrap();
                assert!(eval(&deep, &ctx).is_err());
            }
            // A ternary chain nests to the right. `cel` 0.15's parser stops at
            // a recursion depth of 96 and returns an error, so the longest
            // chain is a submit-time error, not an abort; a chain within that
            // depth evaluates.
            let ternary = format!("{}1", "t?1:".repeat((max - 1) / 4));
            let err = check(&ternary).unwrap_err().to_string();
            assert!(err.contains("Recursion limit"), "{}", shown(&err));
            let ternary = format!("{}1", "t?1:".repeat(64));
            assert_eq!(eval(&ternary, &ctx).unwrap(), json!(1));
            // A long flat list literal.
            let list = format!("size({}])", chain("[1", ",1", max - 7));
            assert!(list.len() <= max);
            assert!(eval(&list, &ctx).is_ok());
        });
    }

    #[test]
    fn expression_over_the_size_limit_is_rejected_before_parsing() {
        on_worker_stack(|| {
            check(&chain("1", "+1", MAX_EXPRESSION_BYTES)).unwrap();
            // 10 KB: the chain the ADR-0075 spike found aborts cel 0.15 in release.
            for over in [
                chain("1", "+1", MAX_EXPRESSION_BYTES + 2),
                chain("a", ".b", 10_000),
            ] {
                let err = check(&over).unwrap_err().to_string();
                assert!(err.contains("the limit is 4096 bytes"), "{err}");
                // The error quotes the start of the expression, not all of it.
                assert!(err.len() < 200, "{err}");
            }
            let template = format!("img:${{{{ {} }}}}", chain("1", "+1", 5000));
            let err = check_interpolation(&template).unwrap_err().to_string();
            assert!(err.contains("the limit is 4096 bytes"), "{err}");
        });
    }

    #[test]
    fn brackets_in_strings_and_comments_do_not_count_towards_nesting() {
        let deep = "(".repeat(100);
        assert_eq!(max_nesting(&format!("'{deep}' == x")), 0);
        assert_eq!(max_nesting(&format!("\"{deep}\" == x")), 0);
        assert_eq!(max_nesting(&format!("'''a'b\n{deep}''' == x")), 0);
        assert_eq!(max_nesting(&format!("b'{deep}' == x")), 0);
        assert_eq!(max_nesting(&format!("// it's {deep}\nx")), 0);
        // An escaped quote does not end the string...
        assert_eq!(max_nesting(&format!(r"'\'{deep}' == x")), 0);
        // ...except in a raw string, where `\` is literal, so the brackets
        // after its closing quote count.
        assert_eq!(max_nesting(&format!(r"r'\' + ({deep}")), 101);
        // `bar` is an identifier, not a raw/bytes prefix.
        assert_eq!(max_nesting(&format!(r"bar'\'' + {deep}")), 100);
        // A single-quoted string cannot span lines; what follows is code.
        assert_eq!(max_nesting(&format!("'unterminated\n{deep}")), 100);
        assert_eq!(max_nesting("(a[b]{c[d]})"), 3);
        on_worker_stack(move || check(&format!("'{deep}' == 'x'")).unwrap());
    }

    #[test]
    fn eval_bool_each_checks_each_context() {
        let a = json!({ "a": "x" });
        let b = json!({ "a": "y" });
        assert_eq!(
            eval_bool_each("a == 'x'", &[&a, &b]).unwrap(),
            vec![true, false]
        );
        let err = eval_bool_each("a", &[&a]).unwrap_err().to_string();
        assert!(err.contains("expected a boolean"), "{err}");
        assert!(eval_bool_each("missing", &[&a]).is_err());
        assert!(eval_bool_each("a", &[&json!(1)]).is_err());
    }
}
