//! The nemesis for rank 1 (ADR-0077 §3): submit the workload, wait for the
//! trigger on the planned run's Pod, wait the planned delay, delete the
//! planned Depot replica with grace 0, then wait for every Run to end or the
//! quiesce bound to run out. Everything it did goes into the scenario file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api as KubeApi, DeleteParams, ListParams, LogParams};

use crate::env::{Api, TERMINAL_RUN_STATUSES};
use crate::pods::{self, PodView, Views};
use crate::scenario::{classify, Fault, Kind, Scenario, Trigger};
use crate::time::now_ms;

pub struct Stack {
    pub api: Api,
    pub kube: kube::Client,
    pub namespace: String,
    /// The release name: Depot Pods are `<release>-workspace-<n>`.
    pub release: String,
    pub pg: sqlx::PgPool,
}

/// How long to wait for the trigger before giving up on the fault.
const TRIGGER_TIMEOUT: Duration = Duration::from_secs(600);
/// After the last Run ends, how long the watcher keeps recording, so Pod
/// teardown (sidecar release, deletes) lands in `pods.jsonl` before I8 runs.
const SETTLE: Duration = Duration::from_secs(45);

/// Run `scenario` (fresh or a replay) and write `<dir>/scenario.json` and
/// `<dir>/pods.jsonl`. Returns the filled scenario.
pub async fn run(stack: &Stack, mut scenario: Scenario, dir: &Path) -> Result<Scenario, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let file = dir.join("scenario.json");
    let pods_api: KubeApi<Pod> = KubeApi::namespaced(stack.kube.clone(), &stack.namespace);

    let views: Views = Default::default();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let watcher = tokio::spawn(pods::watch(
        pods_api.clone(),
        dir.join("pods.jsonl"),
        views.clone(),
        stop_rx,
    ));

    // The stack must be at the scenario's topology before anything runs: a
    // replay of a 2+2 file on a 1-server release is not the same scenario.
    let servers = wait_servers_ready(&pods_api, &stack.release, scenario.server_replicas as usize).await?;
    let depots = wait_depots_ready(&views, scenario.depot_replicas as usize).await?;
    log(&format!(
        "{} seed {}: servers {:?}; Depot replicas {:?}; plan {:?}",
        scenario.kind.as_str(),
        scenario.seed,
        servers,
        depots,
        scenario.plan
    ));

    scenario.started_at_ms = now_ms();
    let body = scenario.workload.pipeline();
    for _ in 0..scenario.workload.runs {
        let id = stack.api.create_run(&body).await?;
        log(&format!("created run {id}"));
        scenario.runs.push(id);
    }
    scenario.write(&file)?;

    let target_run = scenario
        .runs
        .get(scenario.plan.target_run as usize)
        .cloned()
        .ok_or("plan.target_run is past the last run")?;
    let victim = format!("{}-workspace-{}", stack.release, scenario.plan.victim_ordinal);

    // --- the trigger --------------------------------------------------------
    let kind = scenario.kind;
    let holds = move |p: &PodView| match kind {
        Kind::MidDrain => pods::drain_in_flight(p),
        Kind::MidFeed => pods::feed_in_flight(p),
    };
    let deadline = std::time::Instant::now() + TRIGGER_TIMEOUT;
    let trigger = loop {
        let found = find_target(&views, &target_run, kind.target_step(), &holds);
        if let Some(p) = found {
            break Some(Trigger {
                run: target_run.clone(),
                step: kind.target_step().into(),
                attempt: p.attempt.clone(),
                pod: p.name.clone(),
                pod_uid: p.uid.clone(),
                condition: match kind {
                    Kind::MidDrain => "step exited 0, egress running, no workspace-root annotation".into(),
                    Kind::MidFeed => "scarab-workspace-init running".into(),
                },
                observed_at_ms: now_ms(),
            });
        }
        if std::time::Instant::now() > deadline || all_terminal(stack, &scenario.runs).await {
            break None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    match trigger {
        None => {
            scenario
                .notes
                .push(format!("trigger never held on run {target_run}; no fault fired"));
            log("trigger never held; no fault fired");
        }
        Some(t) => {
            log(&format!("trigger on {} ({}); firing in {} ms", t.pod, t.condition, scenario.plan.delay_ms));
            tokio::time::sleep(Duration::from_millis(scenario.plan.delay_ms)).await;
            let held = views
                .lock()
                .expect("views lock")
                .get(&t.pod_uid)
                .is_some_and(holds);
            let fence_rows = match (&t.attempt, kind) {
                (Some(a), Kind::MidDrain) => fence_pack_rows(&stack.pg, &t.run, &t.step, a).await,
                _ => None,
            };
            let victim_uid = views
                .lock()
                .expect("views lock")
                .values()
                .find(|p| p.name == victim && !p.deleting)
                .map(|p| p.uid.clone());
            let fired_at_ms = now_ms();
            pods_api
                .delete(
                    &victim,
                    &DeleteParams {
                        grace_period_seconds: Some(0),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| format!("delete {victim}: {e}"))?;
            log(&format!(
                "deleted {victim} (uid {victim_uid:?}) with grace 0; condition held: {held}; fence pack rows: {fence_rows:?}"
            ));
            scenario.faults.push(Fault {
                action: "delete-pod".into(),
                pod: victim.clone(),
                pod_uid: victim_uid,
                grace_seconds: 0,
                trigger: Some(t),
                condition_held_at_fire: held,
                fence_pack_rows_at_fire: fence_rows,
                fired_at_ms,
                landing: None,
            });
        }
    }
    scenario.write(&file)?;

    // --- quiesce --------------------------------------------------------------
    let deadline_ms = scenario.quiesce_deadline_ms();
    loop {
        if all_terminal(stack, &scenario.runs).await {
            scenario.quiesced_at_ms = Some(now_ms());
            log("every run is terminal");
            break;
        }
        if now_ms() > deadline_ms {
            scenario
                .notes
                .push(format!("quiesce bound ({} s) ran out", scenario.quiesce_bound_secs));
            log("quiesce bound ran out");
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    tokio::time::sleep(SETTLE).await;
    let _ = stop_tx.send(true);
    watcher
        .await
        .map_err(|e| format!("watcher task: {e}"))?
        .map_err(|e| format!("watcher: {e}"))?;

    // The release's own log lines about the target Pod: what the drain or
    // feed did around the fault, for whoever reads the scenario later.
    if let Some(t) = scenario.faults.iter().find_map(|f| f.trigger.as_ref()) {
        let since = ((now_ms() - scenario.started_at_ms) / 1000 + 60).max(60);
        let text = release_log_lines(&pods_api, since, &[t.pod.as_str(), &scenario.faults[0].pod]).await;
        let _ = std::fs::write(dir.join("release-log.txt"), text);
        if kind == Kind::MidFeed {
            // The fetcher's own account of the fault (retries, resets).
            let lp = LogParams {
                container: Some(pods::INIT_CONTAINER.into()),
                ..Default::default()
            };
            let text = match pods_api.logs(&t.pod, &lp).await {
                Ok(t) => t,
                Err(e) => format!("could not read the fetcher's log: {e}\n"),
            };
            let _ = std::fs::write(dir.join("fetcher-log.txt"), text);
        }
    }

    // --- where it landed --------------------------------------------------------
    let records = pods::read_records(&dir.join("pods.jsonl"))?;
    for fault in &mut scenario.faults {
        if let Some(t) = &fault.trigger {
            let (start, end) = window(kind, &records, &t.pod_uid);
            fault.landing = Some(classify(kind, fault.fired_at_ms, start, end));
        }
    }
    scenario.write(&file)?;
    Ok(scenario)
}

/// The window on the target Pod, from the watcher's record: for a drain,
/// the step container's exit to the first record with the root annotation;
/// for a feed, the fetcher's start to its exit.
pub fn window(kind: Kind, records: &[pods::PodRecord], uid: &str) -> (Option<i64>, Option<i64>) {
    let of_pod: Vec<&pods::PodRecord> = records.iter().filter(|r| r.pod.uid == uid).collect();
    match kind {
        Kind::MidDrain => {
            let start = of_pod
                .iter()
                .filter_map(|r| r.pod.container(pods::STEP_CONTAINER)?.finished_at_ms)
                .min();
            let end = of_pod.iter().filter(|r| r.pod.has_root).map(|r| r.at_ms).min();
            (start, end)
        }
        Kind::MidFeed => {
            let c = |r: &&pods::PodRecord| r.pod.container(pods::INIT_CONTAINER).cloned();
            let start = of_pod.iter().filter_map(|r| c(r)?.started_at_ms).min();
            let end = of_pod.iter().filter_map(|r| c(r)?.finished_at_ms).min();
            (start, end)
        }
    }
}

fn find_target(
    views: &Views,
    run: &str,
    step: &str,
    holds: &impl Fn(&PodView) -> bool,
) -> Option<PodView> {
    views
        .lock()
        .expect("views lock")
        .values()
        .find(|p| p.run.as_deref() == Some(run) && p.step.as_deref() == Some(step) && holds(p))
        .cloned()
}

async fn wait_depots_ready(views: &Views, want: usize) -> Result<Vec<String>, String> {
    for _ in 0..240 {
        let ready: Vec<String> = {
            let map = views.lock().expect("views lock");
            let mut v: Vec<String> = map
                .values()
                .filter(|p| p.is_depot() && p.ready && !p.deleting)
                .map(|p| p.name.clone())
                .collect();
            v.sort();
            v
        };
        if ready.len() == want {
            return Ok(ready);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!("expected {want} ready Depot Pods within 120 s"))
}

/// Wait until exactly `want` server Pods of the release (the Deployment, not
/// the Depot) are ready and not being deleted.
async fn wait_servers_ready(pods: &KubeApi<Pod>, release: &str, want: usize) -> Result<Vec<String>, String> {
    let lp = ListParams::default().labels(&format!("app.kubernetes.io/name=scarab,app.kubernetes.io/instance={release}"));
    let mut ready = Vec::new();
    for _ in 0..240 {
        let list = pods.list(&lp).await.map_err(|e| format!("list server Pods: {e}"))?;
        ready = list
            .items
            .iter()
            .filter(|p| p.metadata.deletion_timestamp.is_none())
            .filter(|p| {
                p.status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
            })
            .filter_map(|p| p.metadata.name.clone())
            .collect();
        ready.sort();
        if ready.len() == want {
            return Ok(ready);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!("expected {want} ready server Pods within 120 s; ready: {ready:?}"))
}

async fn all_terminal(stack: &Stack, runs: &[String]) -> bool {
    for id in runs {
        match stack.api.run_status(id).await {
            Ok(s) if TERMINAL_RUN_STATUSES.contains(&s.as_str()) => {}
            _ => return false,
        }
    }
    true
}

/// `depot_packs` rows staged for the drain fence of (run, step, attempt).
async fn fence_pack_rows(pg: &sqlx::PgPool, run: &str, step: &str, attempt: &str) -> Option<i64> {
    let fence = scarab_workspace_client::drain_fence_key(run, step, attempt);
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM depot_packs WHERE fence_key = $1")
        .bind(fence)
        .fetch_one(pg)
        .await
        .ok()
}

/// Lines from every server and Depot Pod's log in the last `since_secs`
/// that mention any of `needles`, prefixed with the Pod name.
async fn release_log_lines(pods: &KubeApi<Pod>, since_secs: i64, needles: &[&str]) -> String {
    let mut out = String::new();
    let list = match pods
        .list(&ListParams::default().labels("app.kubernetes.io/instance=scarab"))
        .await
    {
        Ok(l) => l,
        Err(e) => return format!("could not list release Pods: {e}\n"),
    };
    for p in list.items {
        let name = p.metadata.name.unwrap_or_default();
        let lp = LogParams {
            since_seconds: Some(since_secs),
            ..Default::default()
        };
        match pods.logs(&name, &lp).await {
            Ok(text) => {
                for line in text.lines().filter(|l| needles.iter().any(|n| l.contains(n))) {
                    out.push_str(&format!("{name}: {line}\n"));
                }
            }
            Err(e) => out.push_str(&format!("{name}: could not read the log: {e}\n")),
        }
    }
    out
}

pub fn log(msg: &str) {
    eprintln!("scarab-chaos [{}]: {msg}", now_ms());
}

/// `<root>/<UTC-ish stamp>-<kind>-s<seed>`: one directory per scenario run.
pub fn run_dir(root: &Path, kind: Kind, seed: u64, replay: bool) -> PathBuf {
    let tag = if replay { "-replay" } else { "" };
    root.join(format!("{}-{}-s{seed}{tag}", now_ms(), kind.as_str()))
}
