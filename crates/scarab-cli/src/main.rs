//! # scarab-cli — the `scarab` developer CLI.
//!
//! `scarab run` is a working manual/API dispatch (ADR-0043 slice 3): it eats the
//! **same** dispatch API the UI does (invariant #5, one validator) — it sends
//! each `--param k=v` as a raw string and lets the server's
//! `resolve_params`/`coerce` do the typing, so there is no second, client-side
//! validator to drift. The other subcommands remain compiling stubs.

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "scarab", about = "Scarab durable CI — developer CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Trigger a pipeline run (manual/API dispatch, ADR-0043).
    Run(RunArgs),
    /// Lint a pipeline file (compile-time lints, e.g. missing-clone).
    Lint(FileArgs),
    /// Validate a pipeline file offline (compile + semantic checks).
    Validate(FileArgs),
    /// Stream a run's logs (replay + live tail via SSE).
    Logs(LogsArgs),
    /// Restart a step (and its dependents) of a run.
    Restart(RestartArgs),
}

/// A local pipeline file (offline — no server needed).
#[derive(Debug, Args)]
struct FileArgs {
    /// Path to the pipeline YAML (e.g. `.scarab/ci.yaml`).
    file: String,
}

#[derive(Debug, Args)]
struct LogsArgs {
    /// The run id.
    run: String,
    #[arg(long, env = "SCARAB_SERVER", default_value = "http://localhost:8080")]
    server: String,
    #[arg(long, env = "SCARAB_TOKEN")]
    token: Option<String>,
}

#[derive(Debug, Args)]
struct RestartArgs {
    /// The run id.
    run: String,
    /// The step to restart (its descendants re-run too, ADR-0027).
    step: String,
    #[arg(long, env = "SCARAB_SERVER", default_value = "http://localhost:8080")]
    server: String,
    #[arg(long, env = "SCARAB_TOKEN")]
    token: Option<String>,
}

/// `scarab run <org>/<repo> <pipeline> [--ref] [--param k=v]... [--api] [--describe]`.
#[derive(Debug, Args)]
struct RunArgs {
    /// The repository, `org/repo` (e.g. `acme/web`).
    repo: String,
    /// The pipeline to dispatch — a bare name (`deploy`) or a full
    /// `.scarab/*.yaml` path. Must declare the matching `on: manual`/`on: api`.
    pipeline: String,
    /// The ref to dispatch at (branch/tag/sha). The server resolves it to a
    /// concrete commit and pins the run to it. Default `HEAD`.
    #[arg(long, default_value = "HEAD")]
    r#ref: String,
    /// A launch parameter, `key=value`. Repeatable. Values are sent **as raw
    /// strings** — the server coerces + validates them against the pipeline's
    /// declared interface (no client-side typing, so the validator lives in one
    /// place). A value may itself contain `=` (only the first `=` splits).
    #[arg(long = "param", value_name = "KEY=VALUE", value_parser = parse_param)]
    param: Vec<(String, String)>,
    /// Dispatch the `api` trigger instead of `manual`.
    #[arg(long)]
    api: bool,
    /// Print the pipeline's typed parameter schema (calls the interface describe
    /// endpoint) instead of dispatching.
    #[arg(long)]
    describe: bool,
    /// Base URL of the Scarab server.
    #[arg(long, env = "SCARAB_SERVER", default_value = "http://localhost:8080")]
    server: String,
    /// Bearer token for authentication. When unset, no `Authorization` header is
    /// sent (a dev server with no session store treats callers as Owner).
    #[arg(long, env = "SCARAB_TOKEN")]
    token: Option<String>,
}

/// Split a `--param key=value` on the **first** `=`. A value may contain further
/// `=` (e.g. a base64 or query string); only the first separates key from value.
fn parse_param(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some(("", _)) => Err(format!("empty parameter name in `{s}`")),
        Some((k, v)) => Ok((k.to_string(), v.to_string())),
        None => Err(format!("expected `key=value`, got `{s}`")),
    }
}

/// Minimal percent-encoding for a query-string **value** — enough for a git ref
/// (which may contain `/`, and rarely reserved chars). Unreserved chars and `/`
/// pass through; everything else is `%XX`-encoded so the ref survives transit.
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Split `org/repo` into its two parts.
fn parse_repo(repo: &str) -> Result<(&str, &str), String> {
    match repo.split_once('/') {
        Some((org, name)) if !org.is_empty() && !name.is_empty() && !name.contains('/') => {
            Ok((org, name))
        }
        _ => Err(format!("expected `org/repo`, got `{repo}`")),
    }
}

/// Build the dispatch request body (ADR-0043). Params are carried as raw JSON
/// **strings**; the server's `coerce`/`validate` turns them into the declared
/// types — the CLI never types them itself (invariant #5).
fn build_dispatch_body(
    pipeline: &str,
    git_ref: &str,
    params: &[(String, String)],
    api: bool,
) -> serde_json::Value {
    let params: serde_json::Map<String, serde_json::Value> = params
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    serde_json::json!({
        "ref": git_ref,
        "pipeline": pipeline,
        "params": params,
        "kind": if api { "api" } else { "manual" },
    })
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Run(args) => run(args).await,
        Command::Lint(args) => lint(&args),
        Command::Validate(args) => validate(&args),
        Command::Logs(args) => logs(args).await,
        Command::Restart(args) => restart(args).await,
    };
    std::process::exit(code);
}

/// Compile a pipeline file offline (the same compiler the server runs —
/// invariant #5, one validator). Errors print one diagnostic per line.
fn compile_file(path: &str) -> Result<scarab_pipeline::PipelineIr, i32> {
    let yaml = match std::fs::read_to_string(path) {
        Ok(y) => y,
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            return Err(2);
        }
    };
    match scarab_pipeline::compile_yaml(&yaml) {
        Ok(ir) => Ok(ir),
        Err(scarab_pipeline::PipelineError::Validation(diags)) => {
            for d in diags {
                eprintln!("error: {d}");
            }
            Err(1)
        }
        Err(e) => {
            eprintln!("error: {e}");
            Err(1)
        }
    }
}

/// `scarab validate <file>`: compile + semantic checks, offline. Non-zero on
/// any failure.
fn validate(args: &FileArgs) -> i32 {
    match compile_file(&args.file) {
        Ok(ir) => {
            println!("ok: {} step(s), ir v{}", ir.steps.len(), ir.ir_version);
            0
        }
        Err(code) => code,
    }
}

/// `scarab lint <file>`: the compile-time lints (e.g. a push/PR pipeline with
/// no clone step, ADR-0045). Lint findings exit 1.
fn lint(args: &FileArgs) -> i32 {
    match compile_file(&args.file) {
        Ok(ir) => {
            let findings = scarab_pipeline::lint(&ir);
            if findings.is_empty() {
                println!("ok: no lint findings");
                0
            } else {
                for f in &findings {
                    eprintln!("lint: {f}");
                }
                1
            }
        }
        Err(code) => code,
    }
}

/// `scarab logs <run>`: replay + live-tail the run's logs (the same SSE the
/// UI streams), printing data lines until the server closes the stream.
async fn logs(args: LogsArgs) -> i32 {
    let base = args.server.trim_end_matches('/');
    let url = format!("{base}/v1/runs/{}/logs", args.run);
    let client = reqwest::Client::new();
    let resp = match with_auth(client.get(&url), &args.token).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: request to {url} failed: {e}");
            return 1;
        }
    };
    if !resp.status().is_success() {
        eprintln!("logs failed ({})", resp.status().as_u16());
        return 1;
    }
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                buf.extend_from_slice(&chunk);
                // SSE frames are newline-delimited; print each complete
                // `data:` line as it arrives (live tail).
                while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line);
                    if let Some(data) = line.strip_prefix("data:") {
                        println!("{}", data.trim_start().trim_end_matches('\n'));
                    }
                }
            }
            Ok(None) => return 0, // run settled; server closed the stream
            Err(e) => {
                eprintln!("error: log stream: {e}");
                return 1;
            }
        }
    }
}

/// `scarab restart <run> <step>`: re-arm a step + its dependents (ADR-0027).
async fn restart(args: RestartArgs) -> i32 {
    let base = args.server.trim_end_matches('/');
    let url = format!("{base}/v1/runs/{}/steps/{}/restart", args.run, args.step);
    let client = reqwest::Client::new();
    let resp = match with_auth(client.post(&url), &args.token).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: request to {url} failed: {e}");
            return 1;
        }
    };
    if resp.status().is_success() {
        println!("restart accepted: {}/{}", args.run, args.step);
        0
    } else {
        eprintln!(
            "restart failed ({}): {}",
            resp.status().as_u16(),
            resp.text().await.unwrap_or_default().trim()
        );
        1
    }
}

/// Attach the bearer token, when one is configured, to a request.
fn with_auth(req: reqwest::RequestBuilder, token: &Option<String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

async fn run(args: RunArgs) -> i32 {
    let (org, repo) = match parse_repo(&args.repo) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let base = args.server.trim_end_matches('/');
    let client = reqwest::Client::new();

    if args.describe {
        return describe(&client, base, org, repo, &args).await;
    }

    let body = build_dispatch_body(&args.pipeline, &args.r#ref, &args.param, args.api);
    let url = format!("{base}/v1/repos/{org}/{repo}/dispatch");
    let req = with_auth(client.post(&url).json(&body), &args.token);

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: request to {url} failed: {e}");
            return 1;
        }
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        // Print the created run id (the server returns `{ id, status }`).
        match serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["id"].as_str().map(str::to_string))
        {
            Some(id) => println!("{id}"),
            None => println!("{text}"),
        }
        0
    } else {
        // The server's structured per-parameter / fail-closed message (unknown,
        // invalid, missing param, not-dispatchable, disallowed ref).
        eprintln!("dispatch failed ({}): {}", status.as_u16(), text.trim());
        1
    }
}

/// Print the pipeline's typed parameter schema via the interface describe
/// endpoint (ADR-0043 §4) — the CLI reads the exact schema the UI form renders.
async fn describe(
    client: &reqwest::Client,
    base: &str,
    org: &str,
    repo: &str,
    args: &RunArgs,
) -> i32 {
    let url = format!(
        "{base}/v1/repos/{org}/{repo}/pipelines/{}/interface?ref={}",
        args.pipeline,
        encode_query(&args.r#ref),
    );
    let req = with_auth(client.get(&url), &args.token);
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: request to {url} failed: {e}");
            return 1;
        }
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        eprintln!("describe failed ({}): {}", status.as_u16(), text.trim());
        return 1;
    }
    let doc: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: could not parse interface response: {e}");
            return 1;
        }
    };
    println!("{} @ {}", args.pipeline, doc["sha"].as_str().unwrap_or("?"));
    println!("  manual: {}  api: {}", doc["manual"], doc["api"]);
    match doc["inputs"].as_array() {
        Some(inputs) if !inputs.is_empty() => {
            println!("parameters:");
            for p in inputs {
                let name = p["name"].as_str().unwrap_or("?");
                let ty = p["type"].as_str().unwrap_or("string");
                let required = p["required"].as_bool().unwrap_or(true);
                let flag = if required { "required" } else { "optional" };
                print!("  - {name} ({ty}, {flag})");
                if let Some(def) = p.get("default") {
                    if !def.is_null() {
                        print!(" default={def}");
                    }
                }
                if let Some(opts) = p["options"].as_array() {
                    let opts: Vec<&str> = opts.iter().filter_map(|o| o.as_str()).collect();
                    print!(" options=[{}]", opts.join(", "));
                }
                if let Some(desc) = p["description"].as_str() {
                    print!(" — {desc}");
                }
                println!();
            }
        }
        _ => println!("parameters: (none)"),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_param_splits_on_first_equals() {
        assert_eq!(parse_param("region=us-east-1").unwrap(), ("region".into(), "us-east-1".into()));
    }

    #[test]
    fn parse_param_value_may_contain_equals() {
        // Only the FIRST `=` splits — a value with `=` (base64, query string) is
        // preserved verbatim so the server sees exactly what was typed.
        assert_eq!(
            parse_param("data=a=b=c").unwrap(),
            ("data".into(), "a=b=c".into())
        );
    }

    #[test]
    fn parse_param_rejects_missing_equals_and_empty_key() {
        assert!(parse_param("noequals").is_err());
        assert!(parse_param("=value").is_err());
    }

    #[test]
    fn parse_repo_splits_org_and_name() {
        assert_eq!(parse_repo("acme/web").unwrap(), ("acme", "web"));
        assert!(parse_repo("noslash").is_err());
        assert!(parse_repo("a/b/c").is_err());
        assert!(parse_repo("/web").is_err());
    }

    #[test]
    fn build_dispatch_body_sends_string_params_and_kind() {
        let body = build_dispatch_body(
            "deploy",
            "refs/heads/main",
            &[("region".into(), "eu-west-1".into()), ("replicas".into(), "5".into())],
            false,
        );
        assert_eq!(body["ref"], "refs/heads/main");
        assert_eq!(body["pipeline"], "deploy");
        assert_eq!(body["kind"], "manual");
        // Raw strings — no client-side coercion (the server types them).
        assert_eq!(body["params"]["region"], serde_json::json!("eu-west-1"));
        assert_eq!(body["params"]["replicas"], serde_json::json!("5"));
        assert!(body["params"]["replicas"].is_string(), "params stay strings client-side");
    }

    #[test]
    fn build_dispatch_body_api_kind() {
        let body = build_dispatch_body("deploy", "HEAD", &[], true);
        assert_eq!(body["kind"], "api");
        assert_eq!(body["params"], serde_json::json!({}));
    }
}
