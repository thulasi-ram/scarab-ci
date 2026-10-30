//! `scarab validate <file> [--server URL --org O --project P]`.
//!
//! Offline, it compiles the Pipeline file with the same compiler the server
//! runs, after reading every `file:` library it invokes (transitively) from
//! the repository on disk, so a Pipeline with `invoke:` compiles without a
//! server. A `lib:` library is installed on the server: offline it is
//! reported as unresolved and the rest is checked.
//!
//! With `--server`, it also asks the server's policy test API
//! (`POST /v1/orgs/{org}/policy-test`, ADR-0075 §2 and §9) what the
//! `run.create` Policies would decide for a Run of this Pipeline in
//! `--project`, for the given trigger (by default a push to the current
//! branch). Nothing is enforced or recorded. A rule that reads the trigger,
//! the Principal or the time is labelled with the trigger it was decided
//! for.
//!
//! `--server` has no default and no environment variable: a `SCARAB_SERVER`
//! in the environment never turns an offline check into a network call.
//!
//! Exit codes: 0 compiles (and, with `--server`, the Policies allow); 1 a
//! compile error offline, a deny, or a failed request; 2 a usage error, a
//! file that cannot be read, or (with `--server`) a compile error.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::{json, Value};

/// The trigger kinds `--event` takes.
const EVENTS: [&str; 5] = ["push", "pull_request", "tag", "manual", "api"];

#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// Path to the pipeline YAML (e.g. `.scarab/ci.yaml`). Libraries it
    /// invokes are read from the repository it is in.
    pub file: String,
    /// Also evaluate the org's and Project's `run.create` Policies on this
    /// server, without enforcing or recording (a dry run). Needs `--org`
    /// and `--project`. Not read from `SCARAB_SERVER`.
    #[arg(long)]
    pub server: Option<String>,
    /// An API token (`scarab_pat_…`) or a session id, sent as `Bearer`.
    #[arg(long, env = "SCARAB_TOKEN")]
    pub token: Option<String>,
    /// The Org whose Policies are evaluated (with `--server`).
    #[arg(long, requires = "server")]
    pub org: Option<String>,
    /// The Project the Run would be created in (with `--server`).
    #[arg(long, requires = "server")]
    pub project: Option<String>,
    /// The trigger to evaluate for (with `--server`).
    #[arg(long, default_value = "push", value_parser = EVENTS)]
    pub event: String,
    /// The trigger's symbolic ref as admission sees it (with `--server`).
    /// Default: the current branch, as `refs/heads/<branch>`, for `push`,
    /// `manual` and `api`. `tag` needs it (`refs/tags/<tag>`);
    /// `pull_request` needs it (`refs/pull/<n>/head`) or `--pr`.
    #[arg(long, requires = "server", conflicts_with = "pr")]
    pub r#ref: Option<String>,
    /// The pull request number, for `--event pull_request`: the ref is
    /// `refs/pull/<n>/head`.
    #[arg(long, requires = "server")]
    pub pr: Option<u64>,
    /// The trigger's commit (with `--server`). Default: `git rev-parse HEAD`.
    #[arg(long, requires = "server")]
    pub sha: Option<String>,
    /// The forge account that caused the event (with `--server`). Default:
    /// you, for `manual` and `api`; none for the others.
    #[arg(long, requires = "server")]
    pub actor: Option<String>,
    /// A pull request from a fork (with `--event pull_request`).
    #[arg(long, requires = "server")]
    pub fork: bool,
    /// The dispatch reason (with `--event manual` or `api`).
    #[arg(long, requires = "server")]
    pub reason: Option<String>,
}

/// A Pipeline file read from disk with the libraries it invokes.
pub struct Sources {
    pub yaml: String,
    /// Repository path → source, every library invoked, transitively.
    pub libs: BTreeMap<String, String>,
    /// The repository the file is in (git toplevel, else the current
    /// directory).
    pub root: PathBuf,
}

/// `git -C dir <args>`, trimmed; `None` when git fails or prints nothing.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// The directory the Pipeline file is in (`.` for a bare file name).
fn file_dir(file: &str) -> PathBuf {
    match Path::new(file).parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The repository root: the git toplevel of the file's directory, else the
/// current directory.
fn repo_root(file: &str) -> PathBuf {
    git(&file_dir(file), &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Read `file` and every library it invokes, transitively, from the
/// repository root (the keys the compiler looks libraries up by are
/// repository paths). A library that is not on disk is an error naming it;
/// so is one that is not UTF-8. Exit code 2 on any failure, printed.
pub fn read_sources(file: &str) -> Result<Sources, i32> {
    let yaml = std::fs::read_to_string(file).map_err(|e| {
        eprintln!("error: cannot read {file}: {e}");
        2
    })?;
    let root = repo_root(file);
    let mut libs = BTreeMap::new();
    let mut seen = BTreeSet::new();
    // (library, the file that invokes it)
    let mut work: Vec<(String, String)> = scarab_pipeline::invoke_refs(&yaml)
        .into_iter()
        .map(|l| (l, file.to_string()))
        .collect();
    while let Some((lib, by)) = work.pop() {
        if !seen.insert(lib.clone()) {
            continue;
        }
        let on_disk = root.join(&lib);
        let src = std::fs::read_to_string(&on_disk).map_err(|e| {
            eprintln!(
                "error: {by} invokes `{lib}`, which cannot be read at {}: {e}",
                on_disk.display()
            );
            2
        })?;
        work.extend(
            scarab_pipeline::invoke_refs(&src)
                .into_iter()
                .map(|l| (l, lib.clone())),
        );
        libs.insert(lib, src);
    }
    Ok(Sources { yaml, libs, root })
}

/// Compile `sources`. Errors print one diagnostic per line.
///
/// A `lib:` target is a library installed in an org on the server, which an
/// offline check cannot see (ADR-0076 §4). Each one is reported as
/// unresolved and stood in for, so the rest of the Pipeline is still
/// checked; with `--server`, the server compiles it against the org's
/// installed libraries.
pub fn compile(sources: &Sources) -> Result<scarab_pipeline::PipelineIr, ()> {
    let all =
        std::iter::once(sources.yaml.as_str()).chain(sources.libs.values().map(String::as_str));
    let installed = scarab_pipeline::unresolved_library_stubs(all, &BTreeMap::new());
    for name in installed.keys() {
        println!(
            "unresolved: `lib:{name}` is installed on the server, not on disk; its Steps were              not checked (use --server to check them)"
        );
    }
    let all =
        std::iter::once(sources.yaml.as_str()).chain(sources.libs.values().map(String::as_str));
    let plugin_gates = scarab_pipeline::unresolved_plugin_gate_stubs(all);
    for name in plugin_gates.keys() {
        println!(
            "unresolved: `plugin:{name}` is a gate of a plugin installed on the server; its \
             inputs and output types were not checked (use --server to check them)"
        );
    }
    let libraries = scarab_pipeline::Libraries {
        files: sources.libs.clone(),
        installed,
        plugin_gates,
    };
    match scarab_pipeline::compile_with_libraries(&sources.yaml, &libraries) {
        Ok(ir) => Ok(ir),
        Err(scarab_pipeline::PipelineError::Validation(diags)) => {
            for d in diags {
                eprintln!("error: {d}");
            }
            Err(())
        }
        Err(e) => {
            eprintln!("error: {e}");
            Err(())
        }
    }
}

/// The file's path relative to the repository root, `/`-separated (what
/// the server names the Pipeline by when it declares no name). A file
/// outside the root is sent as given.
fn repo_path(file: &str, root: &Path) -> String {
    let rel = std::fs::canonicalize(file).ok().and_then(|f| {
        let root = std::fs::canonicalize(root).ok()?;
        f.strip_prefix(&root).ok().map(Path::to_path_buf)
    });
    match rel {
        Some(r) => r
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        None => file.strip_prefix("./").unwrap_or(file).to_string(),
    }
}

pub async fn run(args: ValidateArgs) -> i32 {
    let Some(server) = args.server.clone() else {
        // Offline: compile only.
        let sources = match read_sources(&args.file) {
            Ok(s) => s,
            Err(code) => return code,
        };
        return match compile(&sources) {
            Ok(ir) => {
                println!("ok: {} step(s), ir v{}", ir.steps.len(), ir.ir_version);
                0
            }
            Err(()) => 1,
        };
    };
    let (Some(org), Some(project)) = (args.org.clone(), args.project.clone()) else {
        eprintln!(
            "error: --server needs --org and --project: the Org whose Policies are evaluated \
             and the Project the Run would be created in"
        );
        return 2;
    };
    let sources = match read_sources(&args.file) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let ir = match compile(&sources) {
        Ok(ir) => ir,
        Err(()) => return 2,
    };
    println!("ok: {} step(s), ir v{}", ir.steps.len(), ir.ir_version);

    let dir = file_dir(&args.file);
    if args.pr.is_some() && args.event != "pull_request" {
        eprintln!("error: --pr is for --event pull_request");
        return 2;
    }
    if args.fork && args.event != "pull_request" {
        eprintln!("error: --fork is for --event pull_request");
        return 2;
    }
    if args.reason.is_some() && !matches!(args.event.as_str(), "manual" | "api") {
        eprintln!("error: --reason is for --event manual or api");
        return 2;
    }
    let git_ref = match args.r#ref.clone() {
        Some(r) => r,
        None if args.event == "tag" => {
            eprintln!("error: --event tag needs --ref (e.g. refs/tags/v1.0.0)");
            return 2;
        }
        // A pull request's Run is admitted on refs/pull/<n>/head, never on
        // its branch: deciding for refs/heads/<branch> would answer for a
        // ref no pull request Run has.
        None if args.event == "pull_request" => match args.pr {
            Some(n) => format!("refs/pull/{n}/head"),
            None => {
                eprintln!(
                    "error: --event pull_request needs --pr <number> (or --ref \
                     refs/pull/<n>/head)"
                );
                return 2;
            }
        },
        None => match git(&dir, &["symbolic-ref", "-q", "HEAD"]) {
            Some(r) => r,
            None => {
                eprintln!("error: --ref is required: the current directory is not on a git branch");
                return 2;
            }
        },
    };
    let sha = args
        .sha
        .clone()
        .or_else(|| git(&dir, &["rev-parse", "HEAD"]));

    let mut trigger = json!({ "kind": args.event, "ref": git_ref });
    if let Some(sha) = sha {
        trigger["sha"] = json!(sha);
    }
    if let Some(actor) = &args.actor {
        trigger["actor"] = json!(actor);
    }
    if args.fork {
        trigger["fork"] = json!(true);
    }
    if let Some(reason) = &args.reason {
        trigger["reason"] = json!(reason);
    }
    let body = json!({
        "pipeline": {
            "yaml": sources.yaml,
            "path": repo_path(&args.file, &sources.root),
            "libs": sources.libs,
            "project": project,
            "trigger": trigger,
        }
    });
    let api = crate::security::Api::new(&server, &args.token);
    let path = format!("/v1/orgs/{org}/policy-test");
    let answer = match api.post_json(&path, &body).await {
        Ok(v) => v,
        Err(code) => return code,
    };
    print_answer(&answer, &args.event, &git_ref)
}

/// Whether a rule reading input field `f` gets an answer that depends on
/// more than the Pipeline file: the trigger, the Principal creating the Run
/// (absent for a webhook), or the time.
fn is_situational(f: &str) -> bool {
    ["trigger", "principal", "time"]
        .iter()
        .any(|v| f == *v || f.strip_prefix(v).is_some_and(|rest| rest.starts_with('.')))
}

/// Print each Policy's result, label the rules whose answer depends on the
/// trigger, the Principal or the time, and the overall outcome. 0 on allow,
/// 1 on deny.
fn print_answer(answer: &Value, event: &str, git_ref: &str) -> i32 {
    let s = crate::security::sanitize;
    let decision = &answer["decision"];
    let results = decision["results"].as_array().cloned().unwrap_or_default();
    let policies = answer["policies"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        println!("no run.create Policy applies");
    }
    for r in &results {
        let str_of = |v: &Value| s(v.as_str().unwrap_or("?"));
        let outcome = &r["outcome"];
        println!(
            "policy {} ({}, {}): {}",
            str_of(&r["name"]),
            str_of(&r["level"]),
            str_of(&r["mode"]),
            str_of(&outcome["kind"]),
        );
        for m in outcome["messages"].as_array().into_iter().flatten() {
            println!("  - {}", str_of(m));
        }
        if let Some(m) = outcome["message"].as_str() {
            println!("  - {}", s(m));
        }
        let reads = policies
            .iter()
            .find(|p| p["policy_id"] == r["policy_id"] && p["version"] == r["version"]);
        for rule in reads
            .and_then(|p| p["rules"].as_array())
            .into_iter()
            .flatten()
        {
            let fields: Vec<&str> = rule["reads"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|f| is_situational(f))
                .collect();
            // The most specific fields: `trigger` is dropped when
            // `trigger.ref` is there too.
            let trigger_reads: Vec<String> = fields
                .iter()
                .filter(|f| {
                    !fields.iter().any(|g| {
                        g.len() > f.len() && g.starts_with(**f) && g[f.len()..].starts_with('.')
                    })
                })
                .map(|f| s(f))
                .collect();
            if !trigger_reads.is_empty() {
                let at = if trigger_reads.iter().any(|f| f == "time") {
                    ", at the server's current time"
                } else {
                    ""
                };
                println!(
                    "  rule {} reads {}: decided for {} on {}{at}",
                    rule["index"].as_u64().unwrap_or(0),
                    trigger_reads.join(", "),
                    s(event),
                    s(git_ref),
                );
            }
        }
    }
    match decision["outcome"]["kind"].as_str() {
        Some("allow") => {
            println!("outcome: allow");
            0
        }
        Some("deny") => {
            println!("outcome: deny");
            for m in decision["outcome"]["messages"]
                .as_array()
                .into_iter()
                .flatten()
            {
                println!("  - {}", s(m.as_str().unwrap_or("?")));
            }
            1
        }
        other => {
            eprintln!(
                "error: the server answered no outcome ({})",
                s(other.unwrap_or("none"))
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_situational;

    /// A rule over the Principal or the time is labelled like one over the
    /// trigger: its answer depends on more than the Pipeline file.
    #[test]
    fn rules_over_the_trigger_the_principal_or_the_time_are_labelled() {
        for f in ["trigger", "trigger.ref", "principal", "time"] {
            assert!(is_situational(f), "{f}");
        }
        for f in [
            "steps",
            "pipeline.deploy",
            "project.name",
            "timeline",
            "principals",
        ] {
            assert!(!is_situational(f), "{f}");
        }
    }
}
