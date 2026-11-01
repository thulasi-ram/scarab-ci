//! `scarab-chaos`: run a scenario, replay one, or check one.
//!
//!   scarab-chaos run --kind mid-drain --seed 7 --out-root DIR   run, then check
//!   scarab-chaos replay DIR/scenario.json --out-root DIR         same plan, new run, then check
//!   scarab-chaos check DIR/scenario.json                         check a finished run again
//!   scarab-chaos cancel-leftovers                                cancel every non-terminal Run in the chaos database
//!
//! `run` and `replay` refuse while the chaos database holds a non-terminal
//! Run: it would resume beside the new workload and add load and timing the
//! scenario file does not record. `just test chaos` runs `cancel-leftovers` when it
//! exits, before it switches the release back.
//!
//! Exit code: 0 PASS, 1 FAIL (an invariant does not hold), 2 the harness
//! could not run or check, 3 INCONCLUSIVE (the fault did not exercise what
//! the scenario names; see `check::exercised`).
//!
//! Kinds: mid-drain, mid-feed (rank 1: a Depot Pod deleted with grace 0),
//! two-replicas (rank 2 with no fault), kill-server (rank 2: SIGKILL of the
//! producer's claim holder), stop-leader (rank 4: SIGSTOP of the scheduler
//! leader past the lease TTL, then SIGCONT). See `scenario::Kind`.
//!
//! Connections come from the environment `deploy/kind/chaos.sh` sets:
//! SCARAB_CHAOS_API, SCARAB_CHAOS_KUBECONFIG, SCARAB_CHAOS_NAMESPACE,
//! SCARAB_CHAOS_DATABASE_URL, SCARAB_CHAOS_S3_ENDPOINT, SCARAB_CHAOS_S3_BUCKET,
//! SCARAB_CHAOS_S3_ACCESS_KEY, SCARAB_CHAOS_S3_SECRET_KEY, and for the
//! server signals SCARAB_CHAOS_NODE (the kind node container).

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use scarab_chaos::{check, env, nemesis, pods, scenario};

#[derive(Parser)]
#[command(name = "scarab-chaos", about = "ADR-0077 fault scenarios and invariant checker")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a fresh scenario from a seed, then check it.
    Run {
        #[arg(long, value_enum)]
        kind: scenario::Kind,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Directory the per-run directory is created in.
        #[arg(long)]
        out_root: PathBuf,
        #[arg(long, default_value_t = 2)]
        server_replicas: u32,
        #[arg(long, default_value_t = 2)]
        depot_replicas: u32,
    },
    /// Fire the plan of an earlier scenario file again (no RNG), then check.
    Replay {
        file: PathBuf,
        #[arg(long)]
        out_root: PathBuf,
    },
    /// Check a finished scenario (its directory holds pods.jsonl).
    Check { file: PathBuf },
    /// Cancel every non-terminal Run in the chaos database through the API
    /// and wait (up to 120 s) until each is terminal.
    CancelLeftovers,
}

/// Refuse to start a scenario beside Runs of an earlier session.
async fn refuse_leftovers(conn: &Conn) -> Result<(), String> {
    let left = env::non_terminal_runs(&conn.stack.pg).await?;
    if left.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} non-terminal Run(s) in the chaos database from an earlier session: {left:?}. \
         They would run beside this scenario's workload. `just test chaos` cancels them when it exits; \
         rerun it.",
        left.len()
    ))
}

async fn cancel_leftovers(conn: &Conn) -> Result<(), String> {
    let left = env::non_terminal_runs(&conn.stack.pg).await?;
    if left.is_empty() {
        println!("no non-terminal Runs in the chaos database");
        return Ok(());
    }
    for (id, status) in &left {
        println!("cancelling run {id} ({status})");
        conn.stack.api.cancel_run(id).await?;
    }
    for _ in 0..60 {
        let still = env::non_terminal_runs(&conn.stack.pg).await?;
        if still.is_empty() {
            println!("cancelled {} Run(s)", left.len());
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err(format!(
        "Runs still not terminal 120 s after cancel: {:?}",
        env::non_terminal_runs(&conn.stack.pg).await?
    ))
}

fn var(k: &str) -> Result<String, String> {
    std::env::var(k).map_err(|_| format!("{k} is not set (run through `just test chaos`)"))
}

struct Conn {
    stack: nemesis::Stack,
    bucket: object_store::aws::AmazonS3,
}

async fn connect() -> Result<Conn, String> {
    let kube = env::kube_client(Path::new(&var("SCARAB_CHAOS_KUBECONFIG")?)).await?;
    let pg = env::pg(&var("SCARAB_CHAOS_DATABASE_URL")?).await?;
    let bucket = env::bucket(
        &var("SCARAB_CHAOS_S3_ENDPOINT")?,
        &var("SCARAB_CHAOS_S3_BUCKET")?,
        &var("SCARAB_CHAOS_S3_ACCESS_KEY")?,
        &var("SCARAB_CHAOS_S3_SECRET_KEY")?,
    )?;
    Ok(Conn {
        stack: nemesis::Stack {
            api: env::Api::new(&var("SCARAB_CHAOS_API")?)?,
            kube,
            namespace: std::env::var("SCARAB_CHAOS_NAMESPACE").unwrap_or_else(|_| "scarab".into()),
            release: std::env::var("SCARAB_CHAOS_RELEASE").unwrap_or_else(|_| "scarab".into()),
            pg,
        },
        bucket,
    })
}

/// Check the scenario in `dir`, write `verdict.json` beside it and print the
/// verdict lines.
async fn check_dir(conn: &Conn, file: &Path) -> Result<check::Outcome, String> {
    let s = scenario::Scenario::read(file)?;
    let dir = file.parent().unwrap_or(Path::new("."));
    let records = pods::read_records(&dir.join("pods.jsonl"))?;
    let src = check::Sources {
        pg: &conn.stack.pg,
        bucket: &conn.bucket,
        kube: conn.stack.kube.clone(),
        namespace: &conn.stack.namespace,
    };
    let facts = check::gather(&src, &s, records).await?;
    let mut v = check::evaluate(&facts);
    let fault_pods: Vec<String> = s.faults.iter().map(|f| f.pod.clone()).collect();
    v.cache = s
        .cache
        .as_ref()
        .map(|c| scarab_chaos::cache::summary_lines(c, &fault_pods))
        .unwrap_or_default();
    let out = dir.join("verdict.json");
    std::fs::write(&out, serde_json::to_string_pretty(&v).map_err(|e| e.to_string())? + "\n")
        .map_err(|e| format!("{}: {e}", out.display()))?;
    let landing = s
        .faults
        .iter()
        .map(|f| {
            let mut line = format!(
                "{} {} at +{} ms after trigger, landed {}",
                f.action,
                f.pod,
                f.trigger.as_ref().map(|t| f.fired_at_ms - t.observed_at_ms).unwrap_or(0),
                f.landing.as_ref().map(|l| l.class.as_str()).unwrap_or("no window")
            );
            if let Some(sig) = &f.signal {
                line += &format!(
                    "; SIG{} to PID {} (container {}) delivered: {} ({})",
                    sig.signal, sig.pid, sig.container_id, sig.delivered, sig.evidence
                );
            }
            if let Some(role) = &f.victim_role {
                line += &format!("; victim: {role}, chosen by {}", f.target_basis.as_deref().unwrap_or("?"));
            }
            if let Some(r) = f.resumed_at_ms {
                line += &format!("; resumed after {} ms", r - f.fired_at_ms);
            }
            if let Some(l) = &f.lease {
                line += &format!(
                    "; lease {} (term {:?}) -> {:?} (term {:?}), after resume {:?} (term {:?})",
                    l.owner_before, l.term_before, l.owner_after, l.term_after, l.owner_after_resume, l.term_after_resume
                );
            }
            line
        })
        .collect::<Vec<_>>()
        .join("; ");
    println!("scenario: {}", file.display());
    println!(
        "  {} seed {} runs {:?}; faults: {}",
        s.kind.as_str(),
        s.seed,
        s.runs,
        if landing.is_empty() { "none fired".to_string() } else { landing }
    );
    for (inv, n) in &v.checked {
        let fails = v.findings.iter().filter(|f| &f.invariant == inv && f.severity == check::Severity::Fail).count();
        let state = match (fails, *n, inv.as_str()) {
            (0, 0, "I1b") => "not exercised (no retry in this scenario; not counted)".to_string(),
            (0, 0, _) => "not exercised".to_string(),
            (0, _, _) => "ok".to_string(),
            (k, _, _) => format!("FAIL x{k}"),
        };
        println!("  {inv:<4} {state} ({n} checked)");
    }
    for f in &v.findings {
        println!("  [{:?}] {}: {}", f.severity, f.invariant, f.detail);
    }
    for n in &s.notes {
        println!("  note: {n}");
    }
    for line in &v.cache {
        println!("  {line}");
    }
    for why in &v.inconclusive {
        println!("  inconclusive: {why}");
    }
    println!("VERDICT {} {} ({})", v.outcome.as_str(), s.kind.as_str(), out.display());
    Ok(v.outcome)
}

async fn main_inner() -> Result<check::Outcome, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run {
            kind,
            seed,
            out_root,
            server_replicas,
            depot_replicas,
        } => {
            let conn = connect().await?;
            refuse_leftovers(&conn).await?;
            let s = scenario::Scenario::new(kind, seed, scenario::Workload::default(), server_replicas, depot_replicas);
            let dir = nemesis::run_dir(&out_root, kind, seed, false);
            nemesis::run(&conn.stack, s, &dir).await?;
            check_dir(&conn, &dir.join("scenario.json")).await
        }
        Cmd::Replay { file, out_root } => {
            let conn = connect().await?;
            refuse_leftovers(&conn).await?;
            let old = scenario::Scenario::read(&file)?;
            let s = old.replay(&file.display().to_string());
            let dir = nemesis::run_dir(&out_root, s.kind, s.seed, true);
            nemesis::run(&conn.stack, s, &dir).await?;
            check_dir(&conn, &dir.join("scenario.json")).await
        }
        Cmd::Check { file } => {
            let conn = connect().await?;
            check_dir(&conn, &file).await
        }
        Cmd::CancelLeftovers => {
            let conn = connect().await?;
            cancel_leftovers(&conn).await?;
            Ok(check::Outcome::Pass)
        }
    }
}

#[tokio::main]
async fn main() {
    // 0 PASS, 1 FAIL (a finding), 2 the harness could not run or check, 3
    // INCONCLUSIVE (the fault did not exercise what the scenario names).
    match main_inner().await {
        Ok(check::Outcome::Pass) => {}
        Ok(check::Outcome::Fail) => std::process::exit(1),
        Ok(check::Outcome::Inconclusive) => std::process::exit(3),
        Err(e) => {
            eprintln!("scarab-chaos: {e}");
            std::process::exit(2);
        }
    }
}
