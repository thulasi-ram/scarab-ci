//! The parts of a compile error a reader acts on, read back from its text.
//!
//! The compiler reports each problem as one line of text (see
//! [`PipelineError::Validation`](crate::PipelineError::Validation)). The
//! checks a caller can fix by editing a binding, the command-boundary check
//! and the script-template check, write a fixed format; this module reads that
//! format back into fields (the Step, the command index, the template it
//! reads, and for a library Step the library and the caller's binding), so a
//! client can show "what" and "fix" lines without parsing prose. The text
//! stays the source: a line in no known format comes back as
//! [`DiagnosticKind::Other`] with only its message, and possibly its Step.

use serde::Serialize;

/// Which check wrote a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    /// A `command` argument reads free text from outside the Pipeline.
    CommandValue,
    /// A `${{ … }}` sits in a script that a shell or interpreter runs.
    ScriptTemplate,
    /// Any other compile error.
    Other,
}

/// One compile error, with the fields its format carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    /// The Step in the Pipeline file: for a library Step, the invoke Step
    /// that inlined it.
    pub step: Option<String>,
    /// For a library Step: the library, `<publisher>/<name>` or a file path.
    pub library: Option<String>,
    /// For a library Step: its id inside the library.
    pub library_step: Option<String>,
    /// The index into the Step's `command`.
    pub command_index: Option<usize>,
    /// The `${{ … }}` the argument reads, as written.
    pub read: Option<String>,
    /// For a library Step: the library input the caller binds.
    pub input: Option<String>,
    /// For a library Step: the caller's `with:` value for [`input`](Self::input).
    pub bound: Option<String>,
    /// The compiler's text, unchanged.
    pub message: String,
}

impl Diagnostic {
    /// Read the fields of one compile error line.
    pub fn parse(message: &str) -> Diagnostic {
        let mut d = Diagnostic {
            kind: DiagnosticKind::Other,
            step: None,
            library: None,
            library_step: None,
            command_index: None,
            read: None,
            input: None,
            bound: None,
            message: message.to_string(),
        };
        // `library `<lib>`, step `<own>`: …` or `step `<id>`: …`.
        let rest = if let Some(r) = message.strip_prefix("library `") {
            let Some((lib, r)) = r.split_once("`, step `") else {
                return d;
            };
            let Some((own, r)) = r.split_once("`: ") else {
                return d;
            };
            d.library = Some(lib.to_string());
            d.library_step = Some(own.to_string());
            r
        } else if let Some(r) = message.strip_prefix("step `") {
            let Some((id, r)) = r.split_once("`: ") else {
                return d;
            };
            d.step = Some(id.to_string());
            r
        } else {
            return d;
        };
        let Some(r) = rest.strip_prefix("`command[") else {
            return d;
        };
        let Some((index, r)) = r.split_once("]` ") else {
            return d;
        };
        let Ok(index) = index.parse::<usize>() else {
            return d;
        };
        if let Some(r) = r.strip_prefix("reads `") {
            // `<shown>`, which step `<invoke>` binds with `with.<name>: <t>`, a value …
            // `<shown>`, a value …
            if let Some((shown, r)) = r.split_once("`, which step `") {
                let Some((invoke, r)) = r.split_once("` binds with `with.") else {
                    return d;
                };
                let Some((binding, _)) = r.split_once("`, a value from outside") else {
                    return d;
                };
                let Some((name, template)) = binding.split_once(": ") else {
                    return d;
                };
                d.read = Some(shown.to_string());
                d.step = Some(invoke.to_string());
                d.input = Some(name.to_string());
                d.bound = Some(template.to_string());
            } else {
                let Some((shown, _)) = r.split_once("`, a value from outside") else {
                    return d;
                };
                d.read = Some(shown.to_string());
            }
            d.kind = DiagnosticKind::CommandValue;
            d.command_index = Some(index);
        } else if let Some(r) = r.strip_prefix("is a script that `") {
            // `<program>` runs, with `<shown>` in it, …
            let Some((_, r)) = r.split_once("` runs, with `") else {
                return d;
            };
            let Some((shown, _)) = r.split_once("` in it,") else {
                return d;
            };
            d.kind = DiagnosticKind::ScriptTemplate;
            d.command_index = Some(index);
            d.read = Some(shown.to_string());
        }
        d
    }
}

/// The compile errors in a [`PipelineError`](crate::PipelineError)'s text:
/// one per line of a validation failure, or the whole text as one.
pub fn diagnostics(text: &str) -> Vec<Diagnostic> {
    const HEAD: &str = "pipeline validation failed:\n  - ";
    match text.find(HEAD) {
        Some(at) => text[at + HEAD.len()..]
            .split("\n  - ")
            .map(Diagnostic::parse)
            .collect(),
        None => vec![Diagnostic::parse(text)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compile_with_libraries, InstalledLibrary, Libraries};
    use std::collections::BTreeMap;

    fn errors(yaml: &str, libs: &Libraries) -> Vec<Diagnostic> {
        diagnostics(&compile_with_libraries(yaml, libs).unwrap_err().to_string())
    }

    #[test]
    fn a_pipeline_steps_command_value_is_read_back() {
        let yaml = r#"
interface:
  inputs:
    - { name: version, type: string }
on:
  manual: {}
steps:
  - id: tag
    image: alpine/git@sha256:aaaa
    command: [git, tag, -a, "${{ inputs.version }}"]
"#;
        let e = errors(yaml, &Libraries::default());
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!(e[0].kind, DiagnosticKind::CommandValue);
        assert_eq!(e[0].step.as_deref(), Some("tag"));
        assert_eq!(e[0].command_index, Some(3));
        assert_eq!(e[0].read.as_deref(), Some("${{ inputs.version }}"));
        assert_eq!(e[0].library, None);
        assert!(e[0]
            .message
            .contains("Scarab puts no such value into a command"));
    }

    #[test]
    fn a_library_steps_command_value_names_the_callers_binding() {
        let lib = r#"
kind: library
interface:
  inputs:
    - { name: release, type: string }
steps:
  - id: helm
    image: alpine/helm@sha256:aaaa
    command: [helm, upgrade, --install, --wait, "${{ inputs.release }}"]
"#;
        let yaml = r#"
interface:
  inputs:
    - { name: service, type: string }
on:
  manual: {}
steps:
  - id: ship
    invoke: lib:acme/deploy-helm
    with: { release: "${{ inputs.service }}" }
"#;
        let libs = Libraries {
            installed: BTreeMap::from([(
                "acme/deploy-helm".to_string(),
                InstalledLibrary {
                    content: lib.to_string(),
                    source: format!("github.com/acme/libs//deploy-helm.yaml@{}", "1".repeat(40)),
                },
            )]),
            ..Default::default()
        };
        let e = errors(yaml, &libs);
        assert_eq!(e.len(), 1, "{e:?}");
        let d = &e[0];
        assert_eq!(d.kind, DiagnosticKind::CommandValue);
        assert_eq!(d.step.as_deref(), Some("ship"));
        assert_eq!(d.library.as_deref(), Some("acme/deploy-helm"));
        assert_eq!(d.library_step.as_deref(), Some("helm"));
        assert_eq!(d.command_index, Some(4));
        assert_eq!(d.read.as_deref(), Some("${{ inputs.release }}"));
        assert_eq!(d.input.as_deref(), Some("release"));
        assert_eq!(d.bound.as_deref(), Some("${{ inputs.service }}"));
    }

    #[test]
    fn a_line_in_no_known_format_keeps_its_text() {
        let d = Diagnostic::parse("step `a`: `needs` names `b`, which is not a step");
        assert_eq!(d.kind, DiagnosticKind::Other);
        assert_eq!(d.step.as_deref(), Some("a"));
        assert_eq!(d.command_index, None);
        let d = Diagnostic::parse("yaml parse error: oops");
        assert_eq!(d.kind, DiagnosticKind::Other);
        assert_eq!(d.step, None);
        assert_eq!(d.message, "yaml parse error: oops");
    }
}
