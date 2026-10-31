//! The trigger's `event` fields a `${{ … }}` template may read (ADR-0076 §9).
//!
//! A `when:` sees the whole trigger event. A template does not: what it
//! renders becomes part of a command or an environment variable, and some
//! event fields are written by anyone who can push a branch or comment. So a
//! template reads only the fields in [`READABLE`], each of whose values
//! Scarab can check, and only when the value passes its check.
//!
//! The values are fixed when the Run is created. The server builds them from
//! the trigger event, keeps the fields that pass their check ([`facts`]),
//! and stores them with the Run; the renderer reads the stored values when a
//! Step launches. A Pipeline that reads a field the trigger does not have, or
//! one whose value fails its check, is refused at Run creation with the
//! reason. A Pipeline that does not read the field is not affected.
//!
//! Event values come from outside the Pipeline: every rule about where an
//! outside value may sit in a `command` applies to them.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::cel;
use crate::scope::{BoundValue, ScopeInputs};
use crate::PipelineIr;

/// The fields a template may read, as `event.<field>`.
pub const READABLE: [&str; 12] = [
    "kind",
    "sha",
    "number",
    "issue",
    "fork",
    "run",
    "repo.clone_url",
    "repo.owner",
    "repo.name",
    "ref",
    "branch",
    "tag",
];

/// The fields whose values are of a fixed shape, and so may be written into
/// a Step's `command` (ADR-0076, 2026-10-30): the commit SHA, the pull
/// request or issue number, `fork` and the trigger kind. Each is checked
/// against its shape again when the Step launches. Every other field is
/// free text: it reaches a Step only as an environment variable's value.
pub const FIXED: [&str; 5] = ["sha", "number", "issue", "fork", "kind"];

/// The trigger kinds `event.kind` may hold: each `on:` key a trigger
/// matches.
pub const TRIGGER_KINDS: [&str; 9] = [
    "push",
    "pull_request",
    "tag",
    "release",
    "comment",
    "cron",
    "manual",
    "api",
    "upstream",
];

/// Whether `s` is a commit SHA as a command takes it:
/// `^[0-9a-f]{40}([0-9a-f]{24})?$` (SHA-1 or SHA-256, lower case).
pub(crate) fn is_command_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Why a field a template may not read is left out.
fn unreadable(field: &str) -> Option<&'static str> {
    Some(match field {
        "actor" => "a forge login, which a template could not use safely (a GitHub bot's ends in `[bot]`, a shell glob)",
        "actor_id" => "a forge account id, which is not trigger data a Step needs",
        "body" => "free text that anyone who can comment writes",
        "schedule" => "it comes from the Pipeline itself",
        "message" | "title" | "base" | "reason" => {
            "it is provenance shown on the Run page, not trigger data (ADR-0057)"
        }
        _ => return None,
    })
}

/// The `event` field one CEL read names: `Ok(Some(field))` for a read of
/// `event`, `Ok(None)` for a read of another variable, `Err` with the reason
/// for a read of `event` a template may not make.
fn field_of(read: &cel::Read) -> Result<Option<String>, String> {
    if read.var != "event" {
        return Ok(None);
    }
    let Some(first) = &read.first else {
        return Err("reads `event` whole; read one field, such as `event.sha`".to_string());
    };
    let field = if first == "repo" {
        match &read.second {
            Some(second) => format!("repo.{second}"),
            None => {
                return Err(
                    "reads `event.repo` whole; read `event.repo.owner`, `event.repo.name` or \
                     `event.repo.clone_url`"
                        .to_string(),
                )
            }
        }
    } else {
        first.clone()
    };
    if READABLE.contains(&field.as_str()) {
        return Ok(Some(field));
    }
    Err(match unreadable(&field) {
        Some(why) => format!("reads `event.{field}`, which a template may not read: {why}"),
        None => format!(
            "reads `event.{field}`, which is not a field a template may read (it may read: {})",
            READABLE
                .iter()
                .map(|f| format!("`event.{f}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

/// The `event` fields the `${{ … }}` templates of `text` read, and a reason
/// for each read a template may not make. A template that does not parse
/// reads nothing here; the compiler reports it elsewhere.
pub(crate) fn template_reads(text: &str) -> (BTreeSet<String>, Vec<String>) {
    let mut fields = BTreeSet::new();
    let mut errors = Vec::new();
    for i in cel::scan_interpolations(text).0 {
        let Ok(reads) = cel::reads(text[i.inner].trim()) else {
            continue;
        };
        for r in &reads {
            match field_of(r) {
                Ok(Some(f)) => {
                    fields.insert(f);
                }
                Ok(None) => {}
                Err(e) => errors.push(e),
            }
        }
    }
    (fields, errors)
}

/// Every template of a compiled Step: its image, command and env values, and
/// the `with:` templates of each scope it renders in. Each with the name the
/// compiler reports it by.
fn step_templates(step: &crate::StepSpec) -> Vec<(String, &str)> {
    let mut out = vec![("image".to_string(), step.image.as_str())];
    out.extend(
        step.command
            .iter()
            .enumerate()
            .map(|(i, c)| (format!("command[{i}]"), c.as_str())),
    );
    out.extend(
        step.env
            .iter()
            .enumerate()
            .map(|(i, (_, v))| (format!("env[{i}][1]"), v.as_str())),
    );
    if let Some(scope) = &step.scope {
        for s in scope.chain() {
            if let ScopeInputs::Bound(bindings) = &s.inputs {
                for (name, b) in bindings {
                    if let BoundValue::Template(t) = &b.value {
                        out.push((format!("with.{name}"), t.as_str()));
                    }
                }
            }
        }
    }
    out
}

/// The `event` fields a compiled Pipeline's templates read, which Run
/// creation checks ([`facts`]): its Steps' templates and its
/// `concurrency.group`.
pub fn reads(ir: &PipelineIr) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for step in &ir.steps {
        for (_, text) in step_templates(step) {
            out.extend(template_reads(text).0);
        }
    }
    if let Some(c) = &ir.concurrency {
        out.extend(template_reads(&c.group).0);
    }
    out
}

/// Each template of `ir` that reads `event` in a way a template may not. A
/// `concurrency.group` is a template too: Run creation renders it against
/// the same checked values.
pub(crate) fn check(ir: &PipelineIr, diagnostics: &mut Vec<String>) {
    if let Some(c) = &ir.concurrency {
        let mut seen = BTreeSet::new();
        for e in template_reads(&c.group).1 {
            let d = format!("concurrency: group {e}");
            if seen.insert(d.clone()) {
                diagnostics.push(d);
            }
        }
    }
    for step in &ir.steps {
        let mut seen = BTreeSet::new();
        for (field, text) in step_templates(step) {
            for e in template_reads(text).1 {
                let d = format!("step `{}`: `{field}` {e}", step.id);
                if seen.insert(d.clone()) {
                    diagnostics.push(d);
                }
            }
        }
    }
}

/// Whether `s` is a name made of `allowed` characters that does not start
/// with `-`.
fn plain(s: &str, allowed: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && !s.starts_with('-') && s.chars().all(allowed)
}

/// The longest value, in bytes, each text field accepts. A value is an
/// environment variable or an argument in every Step that reads it.
fn max_len(field: &str) -> usize {
    match field {
        "kind" => 32,
        "run" => 64,
        "repo.owner" | "repo.name" => 100,
        "ref" | "branch" | "tag" => 255,
        "repo.clone_url" => 2048,
        _ => usize::MAX,
    }
}

/// Check one field's value against its rule (ADR-0076 §9). `Err` is the
/// reason, written to follow "`${{ event.<field> }}` does not accept".
pub fn check_value(field: &str, value: &Value) -> Result<(), String> {
    let text = |v: &Value| v.as_str().map(str::to_string);
    let shown = |v: &Value| match v {
        Value::String(s) if s.len() > 80 => {
            let head: String = s.chars().take(40).collect();
            format!("`{}…`", head.escape_debug())
        }
        Value::String(s) => format!("`{}`", s.escape_debug()),
        other => format!("`{other}`"),
    };
    if let Some(s) = value.as_str() {
        let max = max_len(field);
        if s.len() > max {
            return Err(format!(
                "{}, which is {} bytes long (at most {max})",
                shown(value),
                s.len()
            ));
        }
    }
    match field {
        "kind" => match text(value) {
            Some(s) if plain(&s, |c| c.is_ascii_lowercase() || c == '_') => Ok(()),
            _ => Err(format!("the trigger kind {}", shown(value))),
        },
        "sha" => match text(value) {
            Some(s) if (s.len() == 40 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit()) => {
                Ok(())
            }
            _ => Err(format!(
                "the commit {}, which is not 40 or 64 hex characters",
                shown(value)
            )),
        },
        "number" | "issue" => match value.as_u64() {
            Some(_) => Ok(()),
            None => Err(format!("{}, which is not a whole number", shown(value))),
        },
        "fork" => match value {
            Value::Bool(_) => Ok(()),
            _ => Err(format!("{}, which is not true or false", shown(value))),
        },
        "run" => match text(value) {
            Some(s) if plain(&s, |c| c.is_ascii_alphanumeric() || c == '-') => Ok(()),
            _ => Err(format!("{}, which is not a Run id", shown(value))),
        },
        "repo.owner" | "repo.name" => match text(value) {
            Some(s)
                if plain(&s, |c| c.is_ascii_alphanumeric() || "._-".contains(c))
                    && s != "."
                    && s != ".." =>
            {
                Ok(())
            }
            _ => Err(format!(
                "{}, which may hold only letters, digits, `.`, `_` and `-`, may not start \
                 with `-`, and may not be `.` or `..`",
                shown(value)
            )),
        },
        "repo.clone_url" => match text(value) {
            Some(s)
                if (s.starts_with("https://") || s.starts_with("http://"))
                    && !s.contains('@')
                    && s.chars().all(|c| c.is_ascii_graphic()) =>
            {
                Ok(())
            }
            _ => Err(format!(
                "{}, which is not an http(s) URL without credentials",
                shown(value)
            )),
        },
        "ref" | "branch" | "tag" => match text(value) {
            Some(s)
                if plain(&s, |c| c.is_ascii_alphanumeric() || "._/@-".contains(c))
                    && !s.contains("..")
                    && !s.contains("//") =>
            {
                Ok(())
            }
            Some(s) => {
                let why = if s.is_empty() {
                    "is empty".to_string()
                } else if s.starts_with('-') {
                    "starts with `-`".to_string()
                } else if s.contains("..") {
                    "contains `..`".to_string()
                } else if s.contains("//") {
                    "contains `//`".to_string()
                } else {
                    let bad: String = s
                        .chars()
                        .find(|c| !(c.is_ascii_alphanumeric() || "._/@-".contains(*c)))
                        .map(|c| c.escape_debug().to_string())
                        .unwrap_or_default();
                    format!("contains `{bad}`")
                };
                Err(format!("{}, which {why}", shown(value)))
            }
            None => Err(format!("{}, which is not text", shown(value))),
        },
        other => Err(format!("`event.{other}`, which a template may not read")),
    }
}

/// The value of `field` in `fields` (`repo.owner` is `fields.repo.owner`).
fn get<'a>(fields: &'a Map<String, Value>, field: &str) -> Option<&'a Value> {
    match field.split_once('.') {
        Some((outer, inner)) => fields.get(outer)?.as_object()?.get(inner),
        None => fields.get(field),
    }
}

fn put(out: &mut Map<String, Value>, field: &str, value: Value) {
    match field.split_once('.') {
        Some((outer, inner)) => {
            let entry = out
                .entry(outer.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(m) = entry {
                m.insert(inner.to_string(), value);
            }
        }
        None => {
            out.insert(field.to_string(), value);
        }
    }
}

/// The `event` values to store with a Run: each field of [`READABLE`] that
/// `fields` (the trigger event's values, built by the server) has and whose
/// value passes [`check_value`]. A field that fails its check is left out,
/// so no template can ever render it.
///
/// `Err` holds a refusal for each field in `reads` (what the Pipeline's
/// templates read) that the trigger does not have, or whose value fails its
/// check, written as the reason the Run is refused.
pub fn facts(
    fields: &Map<String, Value>,
    kind: &str,
    reads: &BTreeSet<String>,
) -> Result<Map<String, Value>, Vec<String>> {
    let mut out = Map::new();
    let mut refusals = Vec::new();
    for field in READABLE {
        match get(fields, field) {
            Some(v) => match check_value(field, v) {
                Ok(()) => put(&mut out, field, v.clone()),
                Err(why) if reads.contains(field) => refusals.push(format!(
                    "`${{{{ event.{field} }}}}` does not accept {why}"
                )),
                Err(_) => {}
            },
            None if reads.contains(field) => refusals.push(format!(
                "the Pipeline reads `${{{{ event.{field} }}}}`, which a `{kind}` trigger does \
                 not have"
            )),
            None => {}
        }
    }
    if refusals.is_empty() {
        Ok(out)
    } else {
        Err(refusals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn each_field_has_a_check() {
        let sha = "a".repeat(40);
        let ok = [
            ("kind", json!("push")),
            ("sha", json!(sha)),
            ("sha", json!("b".repeat(64))),
            ("number", json!(7)),
            ("issue", json!(12)),
            ("fork", json!(false)),
            ("run", json!("0192f0aa-7b1c-7c3e-9f00-1234567890ab")),
            ("repo.clone_url", json!("https://github.com/acme/web.git")),
            ("repo.owner", json!("acme")),
            ("repo.name", json!("web.site_2-x")),
            ("ref", json!("refs/heads/feat/a@b")),
            ("branch", json!("release/1.2")),
            ("tag", json!("v1.2.3")),
        ];
        for (f, v) in ok {
            assert_eq!(check_value(f, &v), Ok(()), "{f} = {v}");
        }
        let bad = [
            ("kind", json!("Push;")),
            ("sha", json!("abc123")),
            ("sha", json!("g".repeat(40))),
            ("number", json!("7")),
            ("number", json!(-1)),
            ("issue", json!(1.5)),
            ("fork", json!("false")),
            ("run", json!("r;rm")),
            ("run", json!("-r")),
            ("repo.clone_url", json!("https://u:p@github.com/a/b.git")),
            ("repo.clone_url", json!("file:///etc/passwd")),
            ("repo.clone_url", json!("https://x y")),
            ("repo.owner", json!("-acme")),
            ("repo.owner", json!("ac/me")),
            ("repo.name", json!("")),
            ("ref", json!("refs/heads/a;b")),
            ("branch", json!("--upload-pack=x")),
            ("branch", json!("a..b")),
            ("branch", json!("a//b")),
            ("branch", json!("a b")),
            ("tag", json!("v1$(id)")),
            ("tag", json!(1)),
        ];
        for (f, v) in bad {
            assert!(check_value(f, &v).is_err(), "{f} = {v} must fail its check");
        }
    }

    #[test]
    fn a_failing_field_refuses_only_a_pipeline_that_reads_it() {
        let fields = obj(json!({
            "kind": "push",
            "sha": "a".repeat(40),
            "branch": "feat;rm -rf",
            "ref": "refs/heads/feat;rm -rf",
            "repo": { "owner": "acme", "name": "web" },
        }));
        // Reads only `sha`: admitted, and the bad branch is not stored.
        let stored = facts(&fields, "push", &BTreeSet::from(["sha".to_string()])).unwrap();
        assert_eq!(stored["sha"], json!("a".repeat(40)));
        assert!(stored.get("branch").is_none() && stored.get("ref").is_none());
        assert_eq!(stored["repo"]["owner"], "acme");
        // Reads `branch`: refused with the reason.
        let refused = facts(&fields, "push", &BTreeSet::from(["branch".to_string()])).unwrap_err();
        assert_eq!(refused.len(), 1);
        assert!(
            refused[0].contains("`${{ event.branch }}` does not accept")
                && refused[0].contains("contains `;`"),
            "{refused:?}"
        );
    }

    #[test]
    fn a_field_the_trigger_lacks_refuses_a_pipeline_that_reads_it() {
        let fields = obj(json!({ "kind": "tag", "tag": "v1", "repo": { "owner": "a", "name": "b" } }));
        let refused = facts(&fields, "tag", &BTreeSet::from(["sha".to_string()])).unwrap_err();
        assert_eq!(
            refused,
            vec!["the Pipeline reads `${{ event.sha }}`, which a `tag` trigger does not have"]
        );
        assert!(facts(&fields, "tag", &BTreeSet::new()).is_ok());
    }

    #[test]
    fn templates_read_only_allowlisted_fields() {
        let (fields, errors) =
            template_reads("${{ event.sha }}-${{ event.repo.clone_url }}-${{ event['branch'] }}");
        assert_eq!(
            fields,
            BTreeSet::from(["sha".into(), "repo.clone_url".into(), "branch".into()])
        );
        assert!(errors.is_empty(), "{errors:?}");
        for (t, want) in [
            ("${{ event }}", "reads `event` whole"),
            ("${{ event.repo }}", "reads `event.repo` whole"),
            ("${{ event.actor }}", "`event.actor`, which a template may not read"),
            ("${{ event.body }}", "`event.body`, which a template may not read"),
            ("${{ event.schedule }}", "`event.schedule`, which a template may not read"),
            ("${{ event.message }}", "`event.message`, which a template may not read"),
            ("${{ event.nope }}", "not a field a template may read"),
            ("${{ size(event) > 0 }}", "reads `event` whole"),
        ] {
            let (_, errors) = template_reads(t);
            assert!(
                errors.iter().any(|e| e.contains(want)),
                "{t}: {errors:?} should contain {want}"
            );
        }
    }
}
