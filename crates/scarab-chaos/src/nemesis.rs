//! The nemesis (ADR-0077 §3): submit the workload, wait for the trigger on
//! the planned run's Pod, wait the planned delay, fire the fault, then wait
//! for every Run to end or the quiesce bound to run out. Everything it did
//! goes into the scenario file. The faults: rank 1 deletes the planned Depot
//! replica with grace 0; ranks 2 and 4 signal a server replica from the
//! kind node (`crate::servers`); `pause-node` pauses the whole kind node
//! container past the sidecar reporting ceiling (git-bug 070d042);
//! `two-replicas` fires nothing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api as KubeApi, DeleteParams, ListParams, LogParams};

use crate::env::{Api, TERMINAL_RUN_STATUSES};
use crate::node::Node;
use crate::pods::{self, PodView, Views};
use crate::scenario::{classify, Fault, Kind, NodePause, Scenario, Trigger};
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

    if scenario.kind == Kind::PauseNode {
        // Before anything can pause the node: Ctrl-C or SIGTERM unpauses it.
        crate::node::unpause_on_signal()?;
    }

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
    if scenario.kind.targets_servers() && scenario.server_replicas < 2 {
        return Err(format!(
            "{} needs at least 2 server replicas; the release has {}",
            scenario.kind.as_str(),
            scenario.server_replicas
        ));
    }
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

    // The Cache probe's seed Run (`crate::cache`), before the workload and
    // waited for, so the probe Run below has a Cache to restore. A probe that
    // cannot run is a note, never a harness failure: it is informational.
    if let Some(probe) = scenario.cache.as_mut() {
        let salt = format!("{}-{}-{}", scenario.kind.as_str(), scenario.seed, now_ms());
        let seeded = match crate::cache::Forgejo::from_env() {
            Ok(fj) => crate::cache::seed(&stack.api, &fj, probe, salt).await,
            Err(e) => Err(e),
        };
        match seeded {
            Ok(()) => log(&format!(
                "cache probe: seed run {:?} {:?}",
                probe.seed_run, probe.seed_status
            )),
            Err(e) => {
                log(&format!("cache probe skipped: {e}"));
                probe.skipped = Some(e);
            }
        }
        scenario.write(&file)?;
    }

    scenario.started_at_ms = now_ms();
    let body = scenario.workload.pipeline();
    for _ in 0..scenario.workload.runs {
        let id = stack.api.create_run(&body).await?;
        log(&format!("created run {id}"));
        scenario.runs.push(id);
    }
    // The probe Run goes in with the workload, so its restore runs under the
    // fault.
    if let Some(probe) = scenario.cache.as_mut() {
        if let Err(e) = crate::cache::dispatch_probe(&stack.api, probe).await {
            log(&format!("cache probe not dispatched: {e}"));
            probe.skipped = Some(format!("the probe run was not dispatched: {e}"));
        } else if let Some(run) = &probe.probe_run {
            log(&format!("cache probe: dispatched probe run {run}"));
        }
    }
    scenario.write(&file)?;

    let target_run = scenario
        .runs
        .get(scenario.plan.target_run as usize)
        .cloned()
        .ok_or("plan.target_run is past the last run")?;
    let kind = scenario.kind;
    if kind.has_fault() {
        let holds = move |p: &PodView| match kind {
            Kind::MidDrain | Kind::KillServer | Kind::PauseNode => pods::drain_in_flight(p),
            Kind::MidFeed => pods::feed_in_flight(p),
            Kind::StopLeader => pods::step_running(p),
            Kind::TwoReplicas => false,
        };
        match wait_trigger(stack, &views, &scenario.runs, &target_run, kind, &holds).await {
            None => {
                scenario
                    .notes
                    .push(format!("trigger never held on run {target_run}; no fault fired"));
                log("trigger never held; no fault fired");
            }
            Some(t) => {
                log(&format!("trigger on {} ({}); firing in {} ms", t.pod, t.condition, scenario.plan.delay_ms));
                tokio::time::sleep(Duration::from_millis(scenario.plan.delay_ms)).await;
                let fault = match kind {
                    Kind::MidDrain | Kind::MidFeed => delete_depot(stack, &pods_api, &views, t, &scenario, &holds).await?,
                    Kind::KillServer => {
                        crate::servers::kill_server(stack, &Node::from_env(), &views, &t, &scenario.plan, &holds).await?
                    }
                    Kind::StopLeader => {
                        crate::servers::stop_leader(stack, &Node::from_env(), &views, &t, &scenario.plan, &holds).await?
                    }
                    Kind::PauseNode => pause_node(stack, &pods_api, &views, t, &Node::from_env(), &holds).await?,
                    Kind::TwoReplicas => unreachable!("two-replicas has no fault"),
                };
                scenario.faults.push(fault);
            }
        }
    }
    scenario.write(&file)?;

    // --- quiesce --------------------------------------------------------------
    let deadline_ms = scenario.quiesce_deadline_ms();
    // The probe Run is waited for too (its Pod logs are read below), under
    // the same bound.
    let mut waited = scenario.runs.clone();
    waited.extend(scenario.cache.as_ref().and_then(|c| c.probe_run.clone()));
    loop {
        if all_terminal(stack, &waited).await {
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

    // The release's own log lines about the scenario's step Pods and the
    // fault's target: what the drains, feeds and the fault did, for whoever
    // reads the scenario later, and for the checks that read it (D1). Every
    // scenario writes it, `two-replicas` included.
    let records = pods::read_records(&dir.join("pods.jsonl"))?;
    let since = ((now_ms() - scenario.started_at_ms) / 1000 + 60).max(60);
    let mut needles: Vec<String> = records
        .iter()
        .filter(|r| r.pod.run.as_ref().is_some_and(|x| scenario.runs.contains(x)))
        .map(|r| r.pod.name.clone())
        .collect();
    for f in &scenario.faults {
        needles.extend(f.trigger.as_ref().map(|t| t.pod.clone()));
        needles.push(f.pod.clone());
        needles.extend(f.claim.as_ref().and_then(|c| c.owner.clone()));
        needles.extend(f.lease.as_ref().map(|l| l.owner_before.clone()));
    }
    needles.sort();
    needles.dedup();
    let needles: Vec<&str> = needles.iter().map(String::as_str).filter(|n| !n.is_empty()).collect();
    let text = release_log_lines(&pods_api, since, &needles).await;
    let _ = std::fs::write(dir.join(RELEASE_LOG), text);
    if let Some(t) = scenario.faults.iter().find_map(|f| f.trigger.as_ref()) {
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

    // The Cache probe: each cached Step's fetcher log, while the Pods exist.
    if let Some(probe) = scenario.cache.as_mut() {
        crate::cache::observe(&stack.api, &pods_api, &stack.pg, probe).await;
        let text: String = probe
            .restores
            .iter()
            .map(|r| {
                format!(
                    "{} run {} pod {}: {}\n",
                    r.role,
                    r.run,
                    r.pod,
                    r.line.as_deref().unwrap_or("no `cache restore` line")
                )
            })
            .collect();
        let _ = std::fs::write(dir.join("cache-log.txt"), text);
    }

    // --- where it landed --------------------------------------------------------
    for fault in &mut scenario.faults {
        if let (Some(t), Some(w)) = (&fault.trigger, kind.window()) {
            let (start, end) = window(w, &records, &t.pod_uid);
            fault.landing = Some(classify(w, fault.fired_at_ms, start, end));
        }
    }
    scenario.write(&file)?;
    Ok(scenario)
}

/// The release's log lines about the scenario, beside `scenario.json`.
pub const RELEASE_LOG: &str = "release-log.txt";

/// The window on the target Pod, from the watcher's record: for a drain,
/// the step container's exit to the first record with the root annotation;
/// for a feed, the fetcher's start to its exit. `kind` is a window
/// (`Kind::window`); any other kind has none.
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
        Kind::TwoReplicas | Kind::KillServer | Kind::StopLeader | Kind::PauseNode => (None, None),
    }
}

/// Wait (up to `TRIGGER_TIMEOUT`, or until every Run has ended) for `holds`
/// on a Pod of `target_run`'s target step.
async fn wait_trigger(
    stack: &Stack,
    views: &Views,
    runs: &[String],
    target_run: &str,
    kind: Kind,
    holds: &impl Fn(&PodView) -> bool,
) -> Option<Trigger> {
    let deadline = std::time::Instant::now() + TRIGGER_TIMEOUT;
    loop {
        if let Some(p) = find_target(views, target_run, kind.target_step(), holds) {
            return Some(Trigger {
                run: target_run.to_string(),
                step: kind.target_step().into(),
                attempt: p.attempt.clone(),
                pod: p.name.clone(),
                pod_uid: p.uid.clone(),
                condition: match kind {
                    Kind::MidDrain | Kind::KillServer | Kind::PauseNode => {
                        "step exited 0, egress running, no workspace-root annotation".into()
                    }
                    Kind::MidFeed => "scarab-workspace-init running".into(),
                    Kind::StopLeader => "step container running".into(),
                    Kind::TwoReplicas => String::new(),
                },
                observed_at_ms: now_ms(),
            });
        }
        if std::time::Instant::now() > deadline || all_terminal(stack, runs).await {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Rank 1: delete the planned Depot replica with grace 0.
async fn delete_depot(
    stack: &Stack,
    pods_api: &KubeApi<Pod>,
    views: &Views,
    t: Trigger,
    scenario: &Scenario,
    holds: &impl Fn(&PodView) -> bool,
) -> Result<Fault, String> {
    let victim = format!("{}-workspace-{}", stack.release, scenario.plan.victim_ordinal);
    let held = views.lock().expect("views lock").get(&t.pod_uid).is_some_and(holds);
    let fence_rows = match (&t.attempt, scenario.kind) {
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
    Ok(Fault {
        action: "delete-pod".into(),
        pod: victim,
        pod_uid: victim_uid,
        grace_seconds: 0,
        trigger: Some(t),
        condition_held_at_fire: held,
        fence_pack_rows_at_fire: fence_rows,
        fired_at_ms,
        ..Default::default()
    })
}

/// `pause-node` (git-bug 070d042): `docker pause` the kind node for the
/// target Pod's sidecar reporting ceiling + a margin (`scenario::pause_secs`),
/// then `docker unpause` it, then re-open the harness's port-forwards and
/// wait until Postgres and the kube API answer again. The node is unpaused
/// on every path out of here (`UnpauseGuard`, and the signal handler
/// `run` installed).
async fn pause_node(
    stack: &Stack,
    pods_api: &KubeApi<Pod>,
    views: &Views,
    t: Trigger,
    node: &Node,
    holds: &impl Fn(&PodView) -> bool,
) -> Result<Fault, String> {
    let view = views.lock().expect("views lock").get(&t.pod_uid).cloned();
    let held = view.as_ref().is_some_and(holds);
    let grace = view.as_ref().and_then(|p| p.grace_secs);
    let planned_secs = crate::scenario::pause_secs(grace);
    let ceiling_secs = planned_secs - crate::scenario::PAUSE_MARGIN_SECS;
    let fence_rows = match &t.attempt {
        Some(a) => fence_pack_rows(&stack.pg, &t.run, &t.step, a).await,
        None => None,
    };
    let mut pause = NodePause {
        node: node.name.clone(),
        ceiling_secs,
        planned_secs,
        ..Default::default()
    };
    let mut errors = Vec::new();

    let mut guard = crate::node::UnpauseGuard::new(node);
    let fired_at_ms = now_ms();
    node.docker("pause").await?;
    match node.is_paused().await {
        Ok(p) => pause.paused = p,
        Err(e) => errors.push(format!("after the pause: {e}")),
    }
    log(&format!(
        "paused {} for {planned_secs} s (sidecar ceiling {ceiling_secs} s, grace {grace:?}); paused: {}; \
         condition held: {held}; fence pack rows: {fence_rows:?}",
        node.name, pause.paused
    ));
    tokio::time::sleep(Duration::from_secs(planned_secs)).await;
    let unpaused = node.docker("unpause").await;
    let resumed_at_ms = now_ms();
    match (unpaused, node.is_paused().await) {
        (Ok(()), Ok(false)) => {
            pause.unpaused = true;
            guard.disarm();
        }
        (u, p) => errors.push(format!("the unpause: {u:?}, paused after it: {p:?}")),
    }
    // A failed unpause is retried by the guard here, before anything waits on
    // the node.
    drop(guard);
    log(&format!(
        "unpaused {} after {} ms; unpaused: {}",
        node.name,
        resumed_at_ms - fired_at_ms,
        pause.unpaused
    ));

    // The port-forwards ran over the paused API server; re-open them, then
    // wait for what the rest of the run reads.
    restart_forwards();
    if let Err(e) = wait_reachable(stack, pods_api).await {
        errors.push(e);
    }
    if !errors.is_empty() {
        pause.error = Some(errors.join("; "));
    }
    Ok(Fault {
        action: "pause-node".into(),
        pod: node.name.clone(),
        trigger: Some(t),
        condition_held_at_fire: held,
        fence_pack_rows_at_fire: fence_rows,
        fired_at_ms,
        resumed_at_ms: Some(resumed_at_ms),
        pause: Some(pause),
        ..Default::default()
    })
}

/// End each `kubectl port-forward` the recipe keeps (`SCARAB_CHAOS_FORWARD_PIDFILES`,
/// colon-separated files that hold the current PID of each). The recipe's
/// loop starts it again on the same local port: one that ran across the
/// pause may still be up with a dead stream to the API server.
fn restart_forwards() {
    let Ok(files) = std::env::var("SCARAB_CHAOS_FORWARD_PIDFILES") else {
        log("SCARAB_CHAOS_FORWARD_PIDFILES is not set; the port-forwards are not restarted");
        return;
    };
    for f in files.split(':').filter(|f| !f.is_empty()) {
        let pid = std::fs::read_to_string(f).ok().and_then(|t| crate::node::parse_pid(&t));
        match pid {
            Some(pid) => {
                let res = std::process::Command::new("kill").arg(pid.to_string()).output();
                log(&format!("restarting the port-forward in {f} (PID {pid}): {:?}", res.map(|o| o.status)));
            }
            None => log(&format!("{f} holds no PID; its port-forward is not restarted")),
        }
    }
}

/// Wait (up to 300 s) until Postgres answers through its port-forward and the
/// kube API answers a list.
async fn wait_reachable(stack: &Stack, pods_api: &KubeApi<Pod>) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        let pg = tokio::time::timeout(
            Duration::from_secs(20),
            sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&stack.pg),
        )
        .await;
        let kube = tokio::time::timeout(Duration::from_secs(20), pods_api.list(&ListParams::default().limit(1))).await;
        match (pg, kube) {
            (Ok(Ok(_)), Ok(Ok(_))) => {
                log("Postgres and the kube API answer again");
                return Ok(());
            }
            (pg, kube) => {
                let last = format!(
                    "postgres: {:?}; kube: {:?}",
                    pg.map(|r| r.map(|_| ()).map_err(|e| e.to_string())),
                    kube.map(|r| r.map(|_| ()).map_err(|e| e.to_string()))
                );
                if std::time::Instant::now() > deadline {
                    return Err(format!("not reachable 300 s after the unpause: {last}"));
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
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
pub(crate) async fn fence_pack_rows(pg: &sqlx::PgPool, run: &str, step: &str, attempt: &str) -> Option<i64> {
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
