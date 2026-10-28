//! The type checker for Policy rules: the CEL subset of ADR-0075 §6, checked
//! against one checkpoint's [`Schema`].
//!
//! It walks the AST `cel` 0.15 parses, macros already expanded, so the tree it
//! checks is the tree the evaluator runs. The standard macros expand to
//! comprehensions; [`MacroKind::of`] recognises `exists`, `all` and
//! `exists_one` by the accumulator their expansion starts from, and checks only
//! the predicate the author wrote, with the iteration variable bound to the
//! list's element type.
//!
//! The subset: field access, `has()`, literals (scalars, lists, maps),
//! `==` `!=` `<` `<=` `>` `>=`, `&&` `||` `!` `?:`, `in` on lists and maps,
//! `startsWith` `endsWith` `contains` `matches` (literal pattern, compiled
//! here), `size`, `timestamp('<literal>')` and `duration('<literal>')`, and
//! `exists` `all` `exists_one` over lists. Anything else, `+` on strings
//! included, is "not supported in policies".
//!
//! **Cost bound.** Each node costs 1. A macro costs its range plus
//! `cap × (predicate + 1)`, where `cap` is the list's declared size cap (a
//! literal list's length); `x in list` adds the list's cap. Nested macros
//! multiply. The bound is the worst case: every macro iterating every element,
//! short-circuits ignored. Costs saturate at `u64::MAX` rather than wrap. String
//! functions cost 1 regardless of string length; that is sound only while
//! checkpoint inputs bound their strings, which this spike does not enforce.
//! `matches()` costs 2 because its pattern is compiled once per evaluation,
//! not once per call (`eval::evaluate_when`); `cel`'s own `matches` compiles
//! on every call and is never the one evaluated.
//!
//! **Stack.** Parsing, checking and evaluating run on a thread with a large
//! stack ([`on_cel_stack`]), after a length and bracket-depth pre-check: a
//! stack overflow aborts the process and `catch_unwind` cannot stop it, and a
//! Policy save runs this checker on user input inside the server. Nothing that
//! holds a parsed AST leaves that thread, since dropping the tree recurses
//! too: [`Checked`] keeps the source, and evaluation parses it again.

use std::collections::BTreeSet;
use std::fmt;

use cel::common::ast::{
    operators, CallExpr, ComprehensionExpr, EntryExpr, Expr, IdedExpr, LiteralValue, SelectExpr,
    SourceInfo,
};

use crate::schema::{Schema, Type};

/// The largest cost bound a rule may have: about the number of AST nodes the
/// evaluator visits in the worst case. 100,000 admits one macro over the
/// Steps of a Run nested with one over each Step's `origin` (ADR-0075 §4's
/// example costs 75,265 under `run.create`'s caps), and rejects two macros
/// nested over the Steps, which is quadratic in the size of the Run.
pub const MAX_COST: u64 = 100_000;

/// The longest rule source, in bytes: the 4 KiB ADR-0075 §6 sets for rules,
/// and `scarab-pipeline` for `when:` and `${{ }}`. See [`parse`].
pub const MAX_SOURCE_LEN: usize = 4096;

/// The deepest the checker recurses into a rule's AST: ADR-0075 §6's 64
/// levels. The parser has already accepted the rule by then; this bounds the
/// checker's own recursion and rejects a rule nested deeper.
pub const MAX_DEPTH: u32 = 64;

/// Identifiers CEL resolves to types. Type values are outside the subset.
const TYPE_NAMES: &[&str] = &[
    "bool",
    "int",
    "uint",
    "double",
    "string",
    "bytes",
    "list",
    "map",
    "null_type",
    "type",
    "google",
];

/// A rule's checked expression, or a finding about it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    /// Where in the Policy the finding is, e.g. `rules[0].when` or `match`.
    /// Empty for a finding of [`check`] alone, which sees one rule.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub at: String,
    /// 1-based line and column of the node, when the parser recorded one; 0
    /// for a finding that is not about CEL.
    pub line: u32,
    pub column: u32,
    /// The sub-expression the finding is about, as the checker renders it.
    pub expr: String,
    pub message: String,
}

impl Diagnostic {
    /// A finding about the Policy's structure, not about a CEL expression.
    pub fn at(at: impl Into<String>, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            at: at.into(),
            line: 0,
            column: 0,
            expr: String::new(),
            message: message.into(),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.at.is_empty() {
            write!(f, "{}: ", self.at)?;
        }
        if self.line == 0 {
            return f.write_str(&self.message);
        }
        write!(
            f,
            "{}:{}: `{}`: {}",
            self.line, self.column, self.expr, self.message
        )
    }
}

/// A rule that type-checked. It keeps the source, not the parsed program:
/// evaluation parses it again on the large-stack thread (see the module
/// documentation).
#[derive(Clone, Debug, PartialEq)]
pub struct Checked {
    pub source: String,
    pub result: Type,
    pub cost: u64,
    pub warnings: Vec<Diagnostic>,
    /// The checkpoint fields the rule reads: each top-level variable, and
    /// `variable.field` for a field selected from it directly (`trigger`,
    /// `trigger.ref`). Fields read through an iteration variable count
    /// towards the list variable (`steps`).
    pub reads: BTreeSet<String>,
}

/// Runs `f` on a thread whose stack holds the deepest rule the limits allow
/// (`scarab_pipeline::cel::CEL_STACK_BYTES`: 256 MiB reserved in a debug
/// build, 64 MiB in release; only touched pages are committed). A panic on
/// that thread, or failing to start it, is an `Err` with a short reason.
pub fn on_cel_stack<T: Send>(f: impl FnOnce() -> T + Send) -> Result<T, String> {
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("scarab-policy-cel".into())
            .stack_size(scarab_pipeline::cel::CEL_STACK_BYTES)
            .spawn_scoped(scope, f)
            .map_err(|e| format!("cannot start the policy thread: {e}"))?;
        handle
            .join()
            .map_err(|_| "the policy thread panicked".to_string())
    })
}

/// Parses `source` and checks it against `schema`, expecting `expected` (bool
/// for a `when`; [`Type::Dyn`] accepts any result type).
pub fn check(schema: &Schema, source: &str, expected: &Type) -> Result<Checked, Vec<Diagnostic>> {
    check_limits(source)?;
    on_cel_stack(|| check_on_this_thread(schema, source, expected))
        .unwrap_or_else(|e| Err(vec![at_start(source, format!("does not parse: {e}"))]))
}

fn check_on_this_thread(
    schema: &Schema,
    source: &str,
    expected: &Type,
) -> Result<Checked, Vec<Diagnostic>> {
    let program = parse(source)?;
    let mut checker = Checker {
        schema,
        info: program.source_info(),
        scopes: Vec::new(),
        guards: Vec::new(),
        warnings: Vec::new(),
        reads: BTreeSet::new(),
        depth: 0,
    };
    let root = program.expression();
    let (result, cost) = checker.expr(root).map_err(|d| vec![d])?;
    if !assignable(expected, &result) {
        return Err(vec![checker.diag(
            root,
            format!("the rule must return {expected}, but this returns {result}"),
        )]);
    }
    if cost > MAX_COST {
        return Err(vec![checker.diag(
            root,
            format!(
                "worst-case cost is {cost}, above the limit of {MAX_COST}; \
                 nested macros multiply by each list's size cap"
            ),
        )]);
    }
    let Checker {
        warnings, reads, ..
    } = checker;
    Ok(Checked {
        source: source.to_string(),
        result,
        cost,
        warnings,
        reads,
    })
}

fn at_start(source: &str, message: String) -> Diagnostic {
    Diagnostic {
        at: String::new(),
        line: 1,
        column: 1,
        expr: source.chars().take(60).collect(),
        message,
    }
}

/// Refuses, before parsing, a rule the parser could not handle safely: longer
/// than [`MAX_SOURCE_LEN`], or with brackets nested deeper than
/// [`MAX_DEPTH`] (counted as `scarab-pipeline` counts them, outside string
/// literals and comments).
pub(crate) fn check_limits(source: &str) -> Result<(), Vec<Diagnostic>> {
    if source.len() > MAX_SOURCE_LEN {
        return Err(vec![at_start(
            source,
            format!(
                "a rule is at most {MAX_SOURCE_LEN} bytes; this one is {}",
                source.len()
            ),
        )]);
    }
    let depth = scarab_pipeline::cel::max_nesting(source);
    if depth > MAX_DEPTH as usize {
        return Err(vec![at_start(
            source,
            format!("brackets nest {depth} deep; the limit is {MAX_DEPTH}"),
        )]);
    }
    Ok(())
}

/// Parses with the standard macros, containing a parser panic as a parse
/// error. Call it only on the [`on_cel_stack`] thread, after
/// [`check_limits`].
///
/// A stack overflow is not a panic: it aborts the process. In a debug build
/// `cel` 0.15's parser overflows a 2 MiB thread at about 12 nested
/// parentheses; in release a left-recursive chain (`a.b.b.b…`, `x + x + x…`)
/// of about 5,000 links does, in the parser or in dropping the AST.
pub(crate) fn parse(source: &str) -> Result<cel::Program, Vec<Diagnostic>> {
    let parsed = std::panic::catch_unwind(|| cel::Program::compile(source));
    match parsed {
        Ok(Ok(program)) => Ok(program),
        Ok(Err(errors)) => Err(vec![at_start(source, format!("does not parse: {errors}"))]),
        Err(_) => Err(vec![at_start(source, "does not parse".to_string())]),
    }
}

/// The most memory a `matches()` pattern may compile to.
pub const MAX_PATTERN_BYTES: usize = 1 << 20;

/// Compiles a `matches()` pattern, under [`MAX_PATTERN_BYTES`]: at check
/// time to validate it, and at evaluation once per pattern
/// (`eval::evaluate_when`).
pub(crate) fn pattern(p: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(p)
        .size_limit(MAX_PATTERN_BYTES)
        .build()
}

/// Which standard macro a comprehension was expanded from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MacroKind {
    Exists,
    All,
    ExistsOne,
    /// `map` or `filter`: both start from an empty list.
    MapOrFilter,
}

impl MacroKind {
    fn of(c: &ComprehensionExpr) -> Option<MacroKind> {
        match &c.accu_init.expr {
            Expr::Literal(LiteralValue::Boolean(b)) if !*b.inner() => Some(MacroKind::Exists),
            Expr::Literal(LiteralValue::Boolean(_)) => Some(MacroKind::All),
            Expr::Literal(LiteralValue::Int(_)) => Some(MacroKind::ExistsOne),
            Expr::List(_) => Some(MacroKind::MapOrFilter),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            MacroKind::Exists => "exists",
            MacroKind::All => "all",
            MacroKind::ExistsOne => "exists_one",
            MacroKind::MapOrFilter => "map/filter",
        }
    }

    /// The predicate the author wrote, out of the expansion's loop step.
    fn predicate(self, c: &ComprehensionExpr) -> Option<&IdedExpr> {
        let Expr::Call(step) = &c.loop_step.expr else {
            return None;
        };
        match self {
            MacroKind::Exists if step.func_name == operators::LOGICAL_OR => step.args.get(1),
            MacroKind::All if step.func_name == operators::LOGICAL_AND => step.args.get(1),
            MacroKind::ExistsOne if step.func_name == operators::CONDITIONAL => step.args.first(),
            _ => None,
        }
    }
}

struct Checker<'a> {
    schema: &'a Schema,
    info: &'a SourceInfo,
    /// Iteration variables in scope, innermost last.
    scopes: Vec<(String, Type)>,
    /// Paths known present here: `has(a.b)` or `a != null` held on the way in.
    guards: Vec<String>,
    warnings: Vec<Diagnostic>,
    /// See [`Checked::reads`].
    reads: BTreeSet<String>,
    depth: u32,
}

type Checked1 = Result<(Type, u64), Diagnostic>;

impl Checker<'_> {
    fn diag(&self, e: &IdedExpr, message: String) -> Diagnostic {
        let (line, column) = self
            .info
            .offset_for(e.id)
            .map(|(start, _)| line_col(&self.info.source, start as usize))
            .unwrap_or((1, 1));
        Diagnostic {
            at: String::new(),
            line,
            column,
            expr: render(e),
            message,
        }
    }

    fn unsupported(&self, e: &IdedExpr, what: &str) -> Diagnostic {
        self.diag(e, format!("{what} is not supported in policies"))
    }

    fn expr(&mut self, e: &IdedExpr) -> Checked1 {
        self.depth += 1;
        let r = if self.depth > MAX_DEPTH {
            Err(self.diag(e, format!("nested more than {MAX_DEPTH} deep")))
        } else {
            self.expr_inner(e)
        };
        self.depth -= 1;
        r
    }

    fn expr_inner(&mut self, e: &IdedExpr) -> Checked1 {
        match &e.expr {
            Expr::Literal(lit) => Ok((literal_type(lit), 1)),
            Expr::Ident(name) => self.ident(e, name),
            Expr::Select(sel) if sel.test => self.has(e, sel),
            Expr::Select(sel) => self.select(e, sel),
            Expr::Call(call) => self.call(e, call),
            Expr::Comprehension(c) => self.comprehension(e, c),
            Expr::List(list) => {
                if !list.optional_indices.is_empty() {
                    return Err(self.unsupported(e, "an optional list element"));
                }
                let mut elem = Type::Dyn;
                let mut cost: u64 = 1;
                for item in &list.elements {
                    let (t, c) = self.expr(item)?;
                    elem = unify(&elem, &t).ok_or_else(|| {
                        self.unsupported(item, &format!("a list mixing {elem} and {t}"))
                    })?;
                    cost = cost.saturating_add(c);
                }
                let cap = list.elements.len() as u64;
                Ok((
                    Type::List {
                        elem: Box::new(elem),
                        cap,
                    },
                    cost,
                ))
            }
            Expr::Map(map) => {
                let (mut key, mut value, mut cost) = (Type::Dyn, Type::Dyn, 1);
                for entry in &map.entries {
                    let EntryExpr::MapEntry(entry) = &entry.expr else {
                        return Err(self.unsupported(e, "a struct entry"));
                    };
                    if entry.optional {
                        return Err(self.unsupported(e, "an optional map entry"));
                    }
                    let (k, kc) = self.expr(&entry.key)?;
                    if !matches!(k, Type::String | Type::Int | Type::Uint | Type::Bool) {
                        return Err(self.diag(&entry.key, format!("a map key cannot be {k}")));
                    }
                    let (v, vc) = self.expr(&entry.value)?;
                    key = unify(&key, &k).ok_or_else(|| {
                        self.unsupported(&entry.key, &format!("a map mixing {key} and {k} keys"))
                    })?;
                    value = unify(&value, &v).ok_or_else(|| {
                        self.unsupported(
                            &entry.value,
                            &format!("a map mixing {value} and {v} values"),
                        )
                    })?;
                    cost = add(&[cost, kc, vc]);
                }
                let cap = map.entries.len() as u64;
                Ok((
                    Type::Map {
                        key: Box::new(key),
                        value: Box::new(value),
                        cap,
                    },
                    cost,
                ))
            }
            Expr::Struct(_) => Err(self.unsupported(e, "a message literal")),
            Expr::Unspecified => Err(self.unsupported(e, "this expression")),
        }
    }

    /// A read of a variable: warns when it is an optional top-level variable
    /// that nothing guarded with `!= null`. A side of `==` / `!=` reads it
    /// through [`Checker::ident_type`] instead (see [`Checker::equality_side`]).
    fn ident(&mut self, e: &IdedExpr, name: &str) -> Checked1 {
        let shadowed = self.scopes.iter().any(|(n, _)| n == name);
        if !shadowed
            && self.schema.variable(name).is_some_and(|v| v.optional)
            && !self.guarded(name)
        {
            let w = self.diag(
                e,
                format!(
                    "`{name}` may be null; guard the read with `{name} != null && ...`, \
                     or a missing value fails the rule closed"
                ),
            );
            self.warnings.push(w);
        }
        self.ident_type(e, name)
    }

    fn ident_type(&mut self, e: &IdedExpr, name: &str) -> Checked1 {
        if let Some((_, t)) = self.scopes.iter().rev().find(|(n, _)| n == name) {
            return Ok((t.clone(), 1));
        }
        match self.schema.variable(name) {
            Some(var) => {
                self.reads.insert(name.to_string());
                Ok((var.ty.clone(), 1))
            }
            None if TYPE_NAMES.contains(&name) => {
                Err(self.unsupported(e, &format!("the type name `{name}`")))
            }
            None => {
                let names: Vec<&str> = self
                    .schema
                    .variables
                    .iter()
                    .map(|v| v.name.as_str())
                    .collect();
                Err(self.diag(
                    e,
                    format!(
                        "no variable `{name}` at {}{}; variables are {}",
                        self.schema.checkpoint,
                        did_you_mean(name, &names),
                        names.join(", ")
                    ),
                ))
            }
        }
    }

    /// Records `variable.field` when `sel` selects from a top-level variable.
    fn read_field(&mut self, sel: &SelectExpr) {
        if let Expr::Ident(name) = &sel.operand.expr {
            let shadowed = self.scopes.iter().any(|(n, _)| n == name);
            if !shadowed && self.schema.variable(name).is_some() {
                self.reads.insert(format!("{name}.{}", sel.field));
            }
        }
    }

    fn select(&mut self, e: &IdedExpr, sel: &SelectExpr) -> Checked1 {
        let (t, cost) = self.expr(&sel.operand)?;
        self.read_field(sel);
        match &t {
            Type::Struct(name) => {
                let decl = self.schema.struct_decl(name);
                let Some(field) = decl.field(&sel.field) else {
                    let names: Vec<&str> = decl.fields.iter().map(|f| f.name.as_str()).collect();
                    return Err(self.diag(
                        e,
                        format!(
                            "no field `{}` on {name}{}; fields are {}",
                            sel.field,
                            did_you_mean(&sel.field, &names),
                            names.join(", ")
                        ),
                    ));
                };
                let field = field.clone();
                if field.optional {
                    let path = path_of(e);
                    if !path.as_deref().is_some_and(|p| self.guarded(p)) {
                        let w = self.diag(
                            e,
                            format!(
                                "`{}` is optional; guard the read with `has({}) && ...`, \
                                 or a missing value fails the rule closed",
                                sel.field,
                                render(e)
                            ),
                        );
                        self.warnings.push(w);
                    }
                }
                Ok((field.ty, add(&[cost, 1])))
            }
            Type::Map { key, value, .. } if matches!(**key, Type::String | Type::Dyn) => {
                Ok(((**value).clone(), add(&[cost, 1])))
            }
            other => Err(self.diag(e, format!("cannot select a field from {other}"))),
        }
    }

    fn has(&mut self, e: &IdedExpr, sel: &SelectExpr) -> Checked1 {
        let (t, cost) = self.expr(&sel.operand)?;
        self.read_field(sel);
        match &t {
            Type::Struct(name) => {
                let decl = self.schema.struct_decl(name);
                if decl.field(&sel.field).is_none() {
                    let names: Vec<&str> = decl.fields.iter().map(|f| f.name.as_str()).collect();
                    return Err(self.diag(
                        e,
                        format!(
                            "no field `{}` on {name}{}; fields are {}",
                            sel.field,
                            did_you_mean(&sel.field, &names),
                            names.join(", ")
                        ),
                    ));
                }
                Ok((Type::Bool, add(&[cost, 1])))
            }
            Type::Map { key, .. } if matches!(**key, Type::String | Type::Dyn) => {
                Ok((Type::Bool, add(&[cost, 1])))
            }
            other => Err(self.diag(e, format!("has() needs a struct or map field, not {other}"))),
        }
    }

    /// One side of `==` / `!=`. Comparing a null variable with anything is
    /// false or true, never an error, so a variable read here draws no
    /// warning: `principal != null` is the guard itself, and
    /// `principal == "user:x"` is false when there is no Principal.
    fn equality_side(&mut self, side: &IdedExpr) -> Checked1 {
        match &side.expr {
            Expr::Ident(name) => self.ident_type(side, name),
            _ => self.expr(side),
        }
    }

    fn guarded(&self, path: &str) -> bool {
        self.guards.iter().any(|g| g == path)
    }

    /// Checks `e` with `facts` known to hold.
    fn with_guards(&mut self, facts: Vec<String>, e: &IdedExpr) -> Checked1 {
        let depth = self.guards.len();
        self.guards.extend(facts);
        let r = self.expr(e);
        self.guards.truncate(depth);
        r
    }

    fn call(&mut self, e: &IdedExpr, call: &CallExpr) -> Checked1 {
        let f = call.func_name.as_str();
        let args = &call.args;
        match (f, call.target.as_deref(), args.as_slice()) {
            (operators::LOGICAL_AND, None, [a, b]) | (operators::LOGICAL_OR, None, [a, b]) => {
                let (ta, ca) = self.expr(a)?;
                self.expect(a, &ta, &Type::Bool)?;
                let facts = if f == operators::LOGICAL_AND {
                    facts_if_true(a)
                } else {
                    facts_if_false(a)
                };
                let (tb, cb) = self.with_guards(facts, b)?;
                self.expect(b, &tb, &Type::Bool)?;
                Ok((Type::Bool, add(&[ca, cb, 1])))
            }
            (operators::LOGICAL_NOT, None, [a]) => {
                let (t, c) = self.expr(a)?;
                self.expect(a, &t, &Type::Bool)?;
                Ok((Type::Bool, add(&[c, 1])))
            }
            (operators::CONDITIONAL, None, [cond, then, otherwise]) => {
                let (tc, cc) = self.expr(cond)?;
                self.expect(cond, &tc, &Type::Bool)?;
                let (tt, ct) = self.with_guards(facts_if_true(cond), then)?;
                let (te, ce) = self.with_guards(facts_if_false(cond), otherwise)?;
                let t = unify(&tt, &te).ok_or_else(|| {
                    self.diag(
                        e,
                        format!("both branches must have one type: {tt} and {te}"),
                    )
                })?;
                Ok((t, add(&[cc, ct.max(ce), 1])))
            }
            (operators::EQUALS, None, [a, b]) | (operators::NOT_EQUALS, None, [a, b]) => {
                let (ta, ca) = self.equality_side(a)?;
                let (tb, cb) = self.equality_side(b)?;
                self.equatable(e, a, &ta, b, &tb)?;
                Ok((Type::Bool, add(&[ca, cb, 1])))
            }
            (
                operators::LESS
                | operators::LESS_EQUALS
                | operators::GREATER
                | operators::GREATER_EQUALS,
                None,
                [a, b],
            ) => {
                let (ta, ca) = self.expr(a)?;
                let (tb, cb) = self.expr(b)?;
                let ordered = |t: &Type| {
                    matches!(
                        t,
                        Type::Bool
                            | Type::Int
                            | Type::Uint
                            | Type::Double
                            | Type::String
                            | Type::Bytes
                            | Type::Timestamp
                            | Type::Duration
                            | Type::Dyn
                    )
                };
                let same = ta == tb || ta == Type::Dyn || tb == Type::Dyn;
                if !ordered(&ta) || !ordered(&tb) || !same {
                    return Err(self.diag(e, format!("cannot order {ta} against {tb}")));
                }
                Ok((Type::Bool, add(&[ca, cb, 1])))
            }
            (operators::IN, None, [needle, haystack]) => {
                let (tn, cn) = self.expr(needle)?;
                let (th, ch) = self.expr(haystack)?;
                let (elem, cap) = match &th {
                    Type::List { elem, cap } => ((**elem).clone(), *cap),
                    Type::Map { key, .. } => ((**key).clone(), 1),
                    other => {
                        return Err(
                            self.diag(haystack, format!("`in` needs a list or map, not {other}"))
                        )
                    }
                };
                if !equatable(&tn, &elem) {
                    return Err(self.diag(e, format!("cannot look for {tn} in {th}")));
                }
                if let (Type::Enum(name), Expr::List(list)) = (&tn, &haystack.expr) {
                    for item in &list.elements {
                        self.enum_literal(item, name)?;
                    }
                }
                Ok((Type::Bool, add(&[cn, ch, cap, 1])))
            }
            ("size", None, [x]) | ("size", Some(x), []) => {
                let (t, c) = self.expr(x)?;
                match t {
                    Type::String
                    | Type::Enum(_)
                    | Type::Bytes
                    | Type::List { .. }
                    | Type::Map { .. } => Ok((Type::Int, add(&[c, 1]))),
                    other => Err(self.diag(
                        e,
                        format!("size() needs a string, bytes, list or map, not {other}"),
                    )),
                }
            }
            ("startsWith" | "endsWith" | "contains", Some(recv), [arg]) => {
                let (tr, cr) = self.expr(recv)?;
                let (ta, ca) = self.expr(arg)?;
                if !is_stringy(&tr) || !is_stringy(&ta) {
                    return Err(self.diag(
                        e,
                        format!("{f}() is a string function: called on {tr} with {ta}"),
                    ));
                }
                Ok((Type::Bool, add(&[cr, ca, 1])))
            }
            ("matches", Some(recv), [pattern]) => {
                let (tr, cr) = self.expr(recv)?;
                if !is_stringy(&tr) {
                    return Err(self.diag(e, format!("matches() is a string function, not {tr}'s")));
                }
                let Expr::Literal(LiteralValue::String(p)) = &pattern.expr else {
                    return Err(self.diag(
                        pattern,
                        "matches() takes a string literal pattern in policies".to_string(),
                    ));
                };
                self::pattern(p.inner())
                    .map_err(|err| self.diag(pattern, format!("invalid pattern: {err}")))?;
                Ok((Type::Bool, add(&[cr, 2])))
            }
            (conv @ ("timestamp" | "duration"), None, [arg]) => {
                let Expr::Literal(LiteralValue::String(s)) = &arg.expr else {
                    return Err(
                        self.unsupported(e, &format!("{conv}() of anything but a string literal"))
                    );
                };
                // Validate with the evaluator's own parser, so a literal that
                // checks is one that evaluates.
                let probe = format!("{conv}({})", render(arg));
                let ok = cel::Program::compile(&probe)
                    .ok()
                    .and_then(|p| p.execute(&cel::Context::default()).ok())
                    .is_some();
                if !ok {
                    return Err(self.diag(arg, format!("not a valid {conv}: {}", s.inner())));
                }
                let t = if conv == "timestamp" {
                    Type::Timestamp
                } else {
                    Type::Duration
                };
                Ok((t, 1))
            }
            _ => {
                let what = match operator_symbol(f) {
                    Some(op) => format!("the `{op}` operator"),
                    None => format!("the function `{f}`"),
                };
                Err(self.unsupported(e, &what))
            }
        }
    }

    fn comprehension(&mut self, e: &IdedExpr, c: &ComprehensionExpr) -> Checked1 {
        let kind = MacroKind::of(c);
        let (Some(kind), None) = (kind, &c.iter_var2) else {
            return Err(self.unsupported(e, "this comprehension"));
        };
        let Some(predicate) = kind.predicate(c) else {
            return Err(self.unsupported(e, &format!("the `{}` macro", kind.name())));
        };
        let (range, rc) = self.expr(&c.iter_range)?;
        let (elem, cap) = match &range {
            Type::List { elem, cap } => ((**elem).clone(), *cap),
            Type::Map { .. } => {
                return Err(
                    self.unsupported(&c.iter_range, &format!("`{}` over a map", kind.name()))
                )
            }
            other => {
                return Err(self.diag(
                    &c.iter_range,
                    format!("`{}` needs a list, not {other}", kind.name()),
                ))
            }
        };
        // A guard on a name the iteration variable shadows does not hold inside.
        let saved = self.guards.clone();
        let var = c.iter_var.as_str();
        self.guards
            .retain(|g| g != var && !g.starts_with(&format!("{var}.")));
        self.scopes.push((c.iter_var.clone(), elem));
        let r = self.expr(predicate);
        self.scopes.pop();
        self.guards = saved;
        let (tp, cp) = r?;
        self.expect(predicate, &tp, &Type::Bool)?;
        Ok((Type::Bool, add(&[rc, cap.saturating_mul(add(&[cp, 1]))])))
    }

    fn expect(&self, e: &IdedExpr, actual: &Type, expected: &Type) -> Result<(), Diagnostic> {
        if assignable(expected, actual) {
            Ok(())
        } else {
            Err(self.diag(e, format!("expected {expected}, got {actual}")))
        }
    }

    fn equatable(
        &self,
        e: &IdedExpr,
        a: &IdedExpr,
        ta: &Type,
        b: &IdedExpr,
        tb: &Type,
    ) -> Result<(), Diagnostic> {
        // `x == null` is meaningful for an optional top-level variable only:
        // an absent field is missing, not null.
        for (side, t, other) in [(a, ta, tb), (b, tb, ta)] {
            if *other == Type::Null && *t != Type::Null {
                let optional_var = matches!(&side.expr, Expr::Ident(n)
                    if self.schema.variable(n).is_some_and(|v| v.optional));
                if !optional_var {
                    let hint = match &side.expr {
                        Expr::Select(sel) if !sel.test => {
                            format!("; test presence with has({})", render(side))
                        }
                        _ => String::new(),
                    };
                    return Err(self.diag(
                        e,
                        format!("{} is {t} and is never null{hint}", render(side)),
                    ));
                }
                return Ok(());
            }
        }
        if !equatable(ta, tb) {
            return Err(self.diag(e, format!("cannot compare {ta} with {tb}")));
        }
        if let Type::Enum(name) = ta {
            self.enum_literal(b, name)?;
        }
        if let Type::Enum(name) = tb {
            self.enum_literal(a, name)?;
        }
        Ok(())
    }

    /// A string literal compared with an enum must be one of its values.
    fn enum_literal(&self, e: &IdedExpr, enum_name: &str) -> Result<(), Diagnostic> {
        if let Expr::Literal(LiteralValue::String(s)) = &e.expr {
            let decl = self.schema.enum_decl(enum_name);
            let s: &str = s.inner();
            if !decl.variants.iter().any(|v| v == s) {
                let names: Vec<&str> = decl.variants.iter().map(String::as_str).collect();
                return Err(self.diag(
                    e,
                    format!(
                        "\"{s}\" is not a {enum_name}{}; values are {}",
                        did_you_mean(s, &names),
                        names.join(", ")
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// The sum of cost parts, saturating at `u64::MAX`: a release build does not
/// check overflow, and a wrapped sum would let an unbounded rule under
/// [`MAX_COST`].
fn add(parts: &[u64]) -> u64 {
    parts.iter().fold(0, |sum, c| sum.saturating_add(*c))
}

fn literal_type(lit: &LiteralValue) -> Type {
    match lit {
        LiteralValue::Boolean(_) => Type::Bool,
        LiteralValue::Bytes(_) => Type::Bytes,
        LiteralValue::Double(_) => Type::Double,
        LiteralValue::Int(_) => Type::Int,
        LiteralValue::Null => Type::Null,
        LiteralValue::String(_) => Type::String,
        LiteralValue::UInt(_) => Type::Uint,
    }
}

fn is_stringy(t: &Type) -> bool {
    matches!(t, Type::String | Type::Enum(_))
}

/// Strips what does not change a value's kind: list and map caps, and an
/// enum's closed set (it is a string).
fn shape(t: &Type) -> Type {
    match t {
        Type::Enum(_) => Type::String,
        Type::List { elem, .. } => Type::List {
            elem: Box::new(shape(elem)),
            cap: 0,
        },
        Type::Map { key, value, .. } => Type::Map {
            key: Box::new(shape(key)),
            value: Box::new(shape(value)),
            cap: 0,
        },
        other => other.clone(),
    }
}

fn equatable(a: &Type, b: &Type) -> bool {
    *a == Type::Dyn || *b == Type::Dyn || unify(a, b).is_some()
}

fn assignable(expected: &Type, actual: &Type) -> bool {
    *expected == Type::Dyn || *actual == Type::Dyn || shape(expected) == shape(actual)
}

/// The common type of two values that must share one, keeping the larger cap.
fn unify(a: &Type, b: &Type) -> Option<Type> {
    match (a, b) {
        (Type::Dyn, t) | (t, Type::Dyn) => Some(t.clone()),
        (Type::Enum(x), Type::Enum(y)) if x == y => Some(a.clone()),
        (Type::Enum(_), Type::String) | (Type::String, Type::Enum(_)) => Some(Type::String),
        (Type::List { elem: x, cap: cx }, Type::List { elem: y, cap: cy }) => Some(Type::List {
            elem: Box::new(unify(x, y)?),
            cap: (*cx).max(*cy),
        }),
        (
            Type::Map {
                key: kx,
                value: vx,
                cap: cx,
            },
            Type::Map {
                key: ky,
                value: vy,
                cap: cy,
            },
        ) => Some(Type::Map {
            key: Box::new(unify(kx, ky)?),
            value: Box::new(unify(vx, vy)?),
            cap: (*cx).max(*cy),
        }),
        _ if a == b => Some(a.clone()),
        _ => None,
    }
}

/// `a.b.c` for a chain of selects on an identifier.
fn path_of(e: &IdedExpr) -> Option<String> {
    match &e.expr {
        Expr::Ident(name) => Some(name.clone()),
        Expr::Select(sel) => Some(format!("{}.{}", path_of(&sel.operand)?, sel.field)),
        _ => None,
    }
}

fn is_null(e: &IdedExpr) -> bool {
    matches!(e.expr, Expr::Literal(LiteralValue::Null))
}

/// Paths known present when `e` is true: `has(p)`, `p != null`, and either
/// side of an `&&`.
fn facts_if_true(e: &IdedExpr) -> Vec<String> {
    match &e.expr {
        Expr::Select(sel) if sel.test => path_of(e).into_iter().collect(),
        Expr::Call(c) => match (c.func_name.as_str(), c.args.as_slice()) {
            (operators::LOGICAL_AND, [a, b]) => {
                let mut f = facts_if_true(a);
                f.extend(facts_if_true(b));
                f
            }
            (operators::LOGICAL_NOT, [a]) => facts_if_false(a),
            (operators::NOT_EQUALS, [a, b]) if is_null(b) => path_of(a).into_iter().collect(),
            (operators::NOT_EQUALS, [a, b]) if is_null(a) => path_of(b).into_iter().collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Paths known present when `e` is false: `!has(p)`, `p == null`, and either
/// side of an `||`.
fn facts_if_false(e: &IdedExpr) -> Vec<String> {
    match &e.expr {
        Expr::Call(c) => match (c.func_name.as_str(), c.args.as_slice()) {
            (operators::LOGICAL_OR, [a, b]) => {
                let mut f = facts_if_false(a);
                f.extend(facts_if_false(b));
                f
            }
            (operators::LOGICAL_NOT, [a]) => facts_if_true(a),
            (operators::EQUALS, [a, b]) if is_null(b) => path_of(a).into_iter().collect(),
            (operators::EQUALS, [a, b]) if is_null(a) => path_of(b).into_iter().collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn line_col(source: &str, offset: usize) -> (u32, u32) {
    let before = &source[..offset.min(source.len())];
    let line = before.matches('\n').count() as u32 + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) as u32 + 1;
    (line, column)
}

fn did_you_mean(name: &str, candidates: &[&str]) -> String {
    candidates
        .iter()
        .map(|c| (edit_distance(name, c), *c))
        .filter(|(d, c)| *d <= 2.max(c.len() / 4))
        .min()
        .map(|(_, c)| format!(" (did you mean `{c}`?)"))
        .unwrap_or_default()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

fn operator_symbol(f: &str) -> Option<&'static str> {
    Some(match f {
        operators::ADD => "+",
        operators::SUBSTRACT | operators::NEGATE => "-",
        operators::MULTIPLY => "*",
        operators::DIVIDE => "/",
        operators::MODULO => "%",
        operators::INDEX => "[]",
        operators::OPT_INDEX => "[?]",
        operators::OPT_SELECT => ".?",
        operators::EQUALS => "==",
        operators::NOT_EQUALS => "!=",
        operators::LESS => "<",
        operators::LESS_EQUALS => "<=",
        operators::GREATER => ">",
        operators::GREATER_EQUALS => ">=",
        operators::LOGICAL_AND => "&&",
        operators::LOGICAL_OR => "||",
        operators::LOGICAL_NOT => "!",
        operators::CONDITIONAL => "?:",
        operators::IN => "in",
        _ => return None,
    })
}

/// Renders an expression back to CEL source, macros folded back to the call
/// the author wrote. Used to name the sub-expression a diagnostic is about.
pub fn render(e: &IdedExpr) -> String {
    match &e.expr {
        Expr::Literal(lit) => match lit {
            LiteralValue::Boolean(b) => b.inner().to_string(),
            LiteralValue::Int(i) => i.inner().to_string(),
            LiteralValue::UInt(u) => format!("{}u", u.inner()),
            LiteralValue::Double(d) => format!("{:?}", d.inner()),
            LiteralValue::Null => "null".to_string(),
            LiteralValue::String(s) => format!("{:?}", s.inner()),
            LiteralValue::Bytes(b) => format!("b{:?}", String::from_utf8_lossy(b.inner())),
        },
        Expr::Ident(name) => name.clone(),
        Expr::Select(sel) if sel.test => format!("has({}.{})", render(&sel.operand), sel.field),
        Expr::Select(sel) => format!("{}.{}", render_operand(&sel.operand), sel.field),
        Expr::Call(call) => render_call(call),
        Expr::Comprehension(c) => {
            let range = render_operand(&c.iter_range);
            match MacroKind::of(c).and_then(|k| Some((k, k.predicate(c)?))) {
                Some((k, p)) => format!("{range}.{}({}, {})", k.name(), c.iter_var, render(p)),
                None => {
                    let filter = matches!(&c.loop_step.expr,
                        Expr::Call(step) if step.func_name == operators::CONDITIONAL);
                    let name = if filter { "filter" } else { "map" };
                    format!("{range}.{name}({}, ...)", c.iter_var)
                }
            }
        }
        Expr::List(list) => format!(
            "[{}]",
            list.elements
                .iter()
                .map(render)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Map(map) => format!(
            "{{{}}}",
            map.entries
                .iter()
                .filter_map(|en| match &en.expr {
                    EntryExpr::MapEntry(m) =>
                        Some(format!("{}: {}", render(&m.key), render(&m.value))),
                    EntryExpr::StructField(_) => None,
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Struct(s) => format!("{}{{...}}", s.type_name),
        Expr::Unspecified => "?".to_string(),
    }
}

/// Parenthesises an operand that is itself an operator call.
fn render_operand(e: &IdedExpr) -> String {
    let s = render(e);
    match &e.expr {
        Expr::Call(c) if c.target.is_none() && operator_symbol(&c.func_name).is_some() => {
            format!("({s})")
        }
        _ => s,
    }
}

fn render_call(call: &CallExpr) -> String {
    let args: Vec<String> = call.args.iter().map(render_operand).collect();
    if let Some(target) = &call.target {
        let all: Vec<String> = call.args.iter().map(render).collect();
        return format!(
            "{}.{}({})",
            render_operand(target),
            call.func_name,
            all.join(", ")
        );
    }
    match (call.func_name.as_str(), args.as_slice()) {
        (operators::CONDITIONAL, [c, t, e]) => format!("{c} ? {t} : {e}"),
        (operators::INDEX, [a, _]) => format!("{a}[{}]", render(&call.args[1])),
        (operators::LOGICAL_NOT, [a]) => format!("!{a}"),
        (operators::NEGATE, [a]) => format!("-{a}"),
        (f, [a, b]) if operator_symbol(f).is_some() => {
            format!("{a} {} {b}", operator_symbol(f).unwrap_or(f))
        }
        (f, _) => format!(
            "{f}({})",
            call.args.iter().map(render).collect::<Vec<_>>().join(", ")
        ),
    }
}
