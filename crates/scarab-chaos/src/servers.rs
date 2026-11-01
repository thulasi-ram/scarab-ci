//! Ranks 2 and 4 (ADR-0077 §3): faults on a server replica.
//!
//! **Rank 2, `kill-server`.** SIGKILL the replica that holds the producer's
//! launch claim while its drain is in flight. A launch intent stays
//! undispatched while its step runs; the replica that claimed it last
//! (`outbox.claimed_by`, renewed while it drives) is the one polling the Pod,
//! and the poll drives the drain (git-bug 068cc0b was two replicas driving
//! one drain). Killing that replica, rather than any replica, is what makes
//! the other one adopt a drain in flight. The claim owner is a replica id
//! (`scarab-server-<uuid>`, `crates/scarab-server/src/main.rs`), so it is
//! mapped to a Pod by [`owner_pod`]; when it cannot be, the seeded ordinal
//! picks the victim. Either way the scenario then checks who died: the claim
//! owner's writes (`claimed_until`, lease `expires_at`) stop growing only if
//! its process is gone ([`claim_role`]). A kill that did not hit the holder
//! is INCONCLUSIVE, not PASS.
//!
//! **Rank 4, `stop-leader`.** SIGSTOP the scheduler leader (the `leases` row
//! `resource = 'scheduler'`) and keep it stopped until a peer holds the
//! lease, then SIGCONT it. The stop is checked in `/proc/<pid>/status`; a
//! stop that does not take is a harness error. That the stopped process was
//! the leader is checked by the lease itself: while the leader is stopped
//! nobody renews `expires_at`. If it keeps being renewed, the stopped
//! replica was not the leader: it is resumed at once (recorded as a probe)
//! and the next replica is tried.

use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api as KubeApi, ListParams, LogParams};

use crate::nemesis::{fence_pack_rows, log, Stack};
use crate::node::{ContGuard, Node};
use crate::pods::{PodView, Views};
use crate::scenario::{
    ClaimObservation, Fault, LeaseObservation, NodeSignal, Plan, Trigger, LEASE_TTL_SECS,
};
use crate::time::now_ms;

/// The server container in the release's Deployment (`deploy/helm`).
pub const SERVER_CONTAINER: &str = "scarab-server";

/// The launch intent's kind in `outbox` (`scarab_engine::LAUNCH_STEP`).
pub const LAUNCH_STEP: &str = "launch_step";

#[derive(Debug, Clone, PartialEq)]
pub struct ServerPod {
    pub name: String,
    pub uid: String,
    /// The `scarab-server` container's id, `containerd://` stripped.
    pub container_id: Option<String>,
    pub restarts: i64,
    pub ready: bool,
    /// `lastState.terminated` of the `scarab-server` container.
    pub last_termination: serde_json::Value,
}

impl ServerPod {
    pub fn from_json(p: &serde_json::Value) -> ServerPod {
        let status = p["status"]["containerStatuses"]
            .as_array()
            .and_then(|a| a.iter().find(|c| c["name"] == SERVER_CONTAINER))
            .cloned()
            .unwrap_or_default();
        ServerPod {
            name: p["metadata"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            uid: p["metadata"]["uid"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            container_id: status["containerID"]
                .as_str()
                .and_then(|id| id.rsplit("://").next())
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            restarts: status["restartCount"].as_i64().unwrap_or(0),
            ready: status["ready"].as_bool().unwrap_or(false),
            last_termination: status["lastState"]["terminated"].clone(),
        }
    }
}

/// The release's server Pods (not the Depot's), not being deleted, by name.
pub async fn server_pods(pods: &KubeApi<Pod>, release: &str) -> Result<Vec<ServerPod>, String> {
    let lp = ListParams::default().labels(&format!(
        "app.kubernetes.io/name=scarab,app.kubernetes.io/instance={release}"
    ));
    let list = pods
        .list(&lp)
        .await
        .map_err(|e| format!("list server Pods: {e}"))?;
    let mut out = Vec::new();
    for p in &list.items {
        let v = serde_json::to_value(p).map_err(|e| e.to_string())?;
        if v["metadata"]["deletionTimestamp"].is_null() {
            out.push(ServerPod::from_json(&v));
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The one Pod whose name the owner string contains (a replica id that
/// carries its Pod name), if exactly one does.
pub fn owner_names_pod<'a>(owner: &str, pods: &'a [ServerPod]) -> Option<&'a ServerPod> {
    let hits: Vec<&ServerPod> = pods
        .iter()
        .filter(|p| !p.name.is_empty() && owner.contains(&p.name))
        .collect();
    match hits.as_slice() {
        [one] => Some(one),
        _ => None,
    }
}

/// Map a replica id (lease owner, claim owner) to its server Pod: by name,
/// else by the one Pod whose current `scarab-server` log contains it.
/// Returns the Pod and how it was found.
pub async fn owner_pod(
    pods: &KubeApi<Pod>,
    owner: &str,
    servers: &[ServerPod],
) -> Option<(ServerPod, &'static str)> {
    if let Some(p) = owner_names_pod(owner, servers) {
        return Some((p.clone(), "owner-names-pod"));
    }
    let mut hits = Vec::new();
    for s in servers {
        let lp = LogParams {
            container: Some(SERVER_CONTAINER.into()),
            ..Default::default()
        };
        if let Ok(text) = pods.logs(&s.name, &lp).await {
            if text.contains(owner) {
                hits.push(s.clone());
            }
        }
    }
    match hits.as_slice() {
        [one] => Some((one.clone(), "owner-in-pod-log")),
        _ => None,
    }
}

/// The live claim on a step Attempt's launch intent: (owner, claimed_until).
/// `None` when the intent is dispatched, or its claim has lapsed.
pub async fn launch_claim(
    pg: &sqlx::PgPool,
    run: &str,
    step: &str,
    attempt: Option<&str>,
) -> Result<Option<(String, i64)>, String> {
    sqlx::query_as::<_, (String, i64)>(
        "SELECT claimed_by, claimed_until FROM outbox
         WHERE kind = $1 AND run_id = $2 AND payload->>'step' = $3
           AND ($4::text IS NULL OR payload->>'attempt' = $4)
           AND dispatched_at IS NULL AND dead_lettered_at IS NULL
           AND claimed_by IS NOT NULL
           AND claimed_until > (extract(epoch from now()) * 1000)::bigint
         ORDER BY id DESC LIMIT 1",
    )
    .bind(LAUNCH_STEP)
    .bind(run)
    .bind(step)
    .bind(attempt)
    .fetch_optional(pg)
    .await
    .map_err(|e| format!("postgres: {e}"))
}

/// The latest claim or lease expiry `owner` has written.
pub async fn owner_written_until(pg: &sqlx::PgPool, owner: &str) -> Result<Option<i64>, String> {
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT GREATEST((SELECT max(claimed_until) FROM outbox WHERE claimed_by = $1),
                         (SELECT max(expires_at) FROM leases WHERE owner = $1))",
    )
    .bind(owner)
    .fetch_one(pg)
    .await
    .map_err(|e| format!("postgres: {e}"))
}

/// Who the kill hit, from the claim owner's writes after it: still growing
/// between 5 s and 20 s after the kill means the owner is alive, so the kill
/// hit its peer.
pub fn claim_role(obs: &ClaimObservation) -> &'static str {
    match (
        &obs.owner,
        obs.owner_written_until_5s,
        obs.owner_written_until_20s,
    ) {
        (None, _, _) => "unknown",
        (Some(_), a, b) if b > a => "peer",
        (Some(_), Some(_), _) => "claim-holder",
        (Some(_), None, _) => "unknown",
    }
}

/// The scheduler lease row.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaseRow {
    pub owner: String,
    pub expires_at: i64,
    pub term: Option<i64>,
}

/// Whether `leases` has the `term` column (this branch adds it; the harness
/// works on either schema).
pub async fn lease_has_term(pg: &sqlx::PgPool) -> Result<bool, String> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = current_schema() AND table_name = 'leases' AND column_name = 'term')",
    )
    .fetch_one(pg)
    .await
    .map_err(|e| format!("postgres: {e}"))
}

pub async fn scheduler_lease(
    pg: &sqlx::PgPool,
    with_term: bool,
) -> Result<Option<LeaseRow>, String> {
    let sql = if with_term {
        "SELECT owner, expires_at, term::bigint FROM leases WHERE resource = 'scheduler'"
    } else {
        "SELECT owner, expires_at, NULL::bigint FROM leases WHERE resource = 'scheduler'"
    };
    Ok(sqlx::query_as::<_, (String, i64, Option<i64>)>(sql)
        .fetch_optional(pg)
        .await
        .map_err(|e| format!("postgres: {e}"))?
        .map(|(owner, expires_at, term)| LeaseRow {
            owner,
            expires_at,
            term,
        }))
}

async fn sleep_until_ms(at_ms: i64) {
    let left = at_ms - now_ms();
    if left > 0 {
        tokio::time::sleep(Duration::from_millis(left as u64)).await;
    }
}

/// Wait (up to 120 s) for the kubelet to restart `victim`'s server container
/// in the same Pod. Returns whether the last termination was the SIGKILL,
/// and the evidence.
async fn wait_restarted(pods: &KubeApi<Pod>, release: &str, victim: &ServerPod) -> (bool, String) {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut last_seen = String::from("no Pod seen");
    loop {
        if let Ok(list) = server_pods(pods, release).await {
            match list.iter().find(|p| p.uid == victim.uid) {
                Some(now) if now.restarts > victim.restarts && now.ready => {
                    let t = &now.last_termination;
                    let by_kill = t["signal"] == 9 || t["exitCode"] == 137;
                    return (
                        by_kill,
                        format!(
                            "restarts {} -> {}, container {:?} -> {:?}, last termination {t}",
                            victim.restarts, now.restarts, victim.container_id, now.container_id
                        ),
                    );
                }
                Some(now) => last_seen = format!("{now:?}"),
                None => last_seen = format!("Pod uid {} is gone", victim.uid),
            }
        }
        if std::time::Instant::now() > deadline {
            return (
                false,
                format!("not restarted and ready within 120 s: {last_seen}"),
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Rank 2: SIGKILL the producer's claim holder. `holds` is the trigger
/// condition (the drain in flight), checked again at the moment of the kill.
pub async fn kill_server(
    stack: &Stack,
    node: &Node,
    views: &Views,
    t: &Trigger,
    plan: &Plan,
    holds: &impl Fn(&PodView) -> bool,
) -> Result<Fault, String> {
    let pods: KubeApi<Pod> = KubeApi::namespaced(stack.kube.clone(), &stack.namespace);
    let servers = server_pods(&pods, &stack.release).await?;
    if servers.len() < 2 {
        return Err(format!(
            "kill-server needs 2 server replicas, found {}",
            servers.len()
        ));
    }
    let claim = launch_claim(&stack.pg, &t.run, &t.step, t.attempt.as_deref()).await?;
    let mapped = match &claim {
        Some((owner, _)) => owner_pod(&pods, owner, &servers).await,
        None => None,
    };
    let (victim, basis) = match mapped {
        Some((p, basis)) => (p, basis.to_string()),
        None => (
            servers[plan.victim_ordinal as usize % servers.len()].clone(),
            match &claim {
                Some((owner, _)) => format!("seed (claim owner {owner} not mapped to a Pod)"),
                None => "seed (no live launch claim on the producer)".into(),
            },
        ),
    };
    let container_id = victim.container_id.clone().ok_or_else(|| {
        format!(
            "server Pod {} has no {SERVER_CONTAINER} container id",
            victim.name
        )
    })?;
    let pid = node.container_pid(&container_id).await?;
    let fence_rows = match &t.attempt {
        Some(a) => fence_pack_rows(&stack.pg, &t.run, &t.step, a).await,
        None => None,
    };
    let held = views
        .lock()
        .expect("views lock")
        .get(&t.pod_uid)
        .is_some_and(holds);

    let fired_at_ms = now_ms();
    node.signal(pid, "KILL").await?;
    log(&format!(
        "SIGKILL from node {} to PID {pid}, container {container_id} of {} ({basis}); claim {claim:?}; condition held: {held}",
        node.name, victim.name
    ));

    let mut obs = ClaimObservation {
        owner: claim.as_ref().map(|c| c.0.clone()),
        claimed_until: claim.as_ref().map(|c| c.1),
        ..Default::default()
    };
    if let Some(owner) = obs.owner.clone() {
        sleep_until_ms(fired_at_ms + 5_000).await;
        obs.owner_written_until_5s = owner_written_until(&stack.pg, &owner).await?;
        sleep_until_ms(fired_at_ms + 20_000).await;
        obs.owner_written_until_20s = owner_written_until(&stack.pg, &owner).await?;
    }
    let role = claim_role(&obs);
    let (delivered, evidence) = wait_restarted(&pods, &stack.release, &victim).await;
    log(&format!(
        "kill-server: victim was the {role}; delivered: {delivered} ({evidence})"
    ));

    Ok(Fault {
        action: "kill-server".into(),
        pod: victim.name.clone(),
        pod_uid: Some(victim.uid.clone()),
        grace_seconds: 0,
        trigger: Some(t.clone()),
        condition_held_at_fire: held,
        fence_pack_rows_at_fire: fence_rows,
        fired_at_ms,
        landing: None,
        signal: Some(NodeSignal {
            signal: "KILL".into(),
            node: node.name.clone(),
            container_id,
            pid,
            delivered,
            evidence,
        }),
        target_basis: Some(basis),
        victim_role: Some(role.into()),
        claim: Some(obs),
        ..Default::default()
    })
}

/// What the lease did while a replica was stopped.
enum LeaseOutcome {
    Takeover(LeaseRow, i64),
    StillRenewed,
    NoTakeover,
}

/// Rank 4: SIGSTOP the scheduler leader past the lease TTL, then SIGCONT.
/// An `Err` is a harness error (exit 2); a SIGSTOP the harness sent is
/// always followed by a SIGCONT, on every path (`ContGuard`).
pub async fn stop_leader(
    stack: &Stack,
    node: &Node,
    views: &Views,
    t: &Trigger,
    plan: &Plan,
    holds: &impl Fn(&PodView) -> bool,
) -> Result<Fault, String> {
    let pods: KubeApi<Pod> = KubeApi::namespaced(stack.kube.clone(), &stack.namespace);
    let servers = server_pods(&pods, &stack.release).await?;
    if servers.len() < 2 {
        return Err(format!(
            "stop-leader needs 2 server replicas, found {}",
            servers.len()
        ));
    }
    let with_term = lease_has_term(&stack.pg).await?;
    let before = scheduler_lease(&stack.pg, with_term)
        .await?
        .ok_or("no scheduler lease row: no replica has led yet")?;
    let mut order: Vec<(ServerPod, String)> = Vec::new();
    match owner_pod(&pods, &before.owner, &servers).await {
        Some((p, basis)) => order.push((p, basis.into())),
        None => {
            let first = plan.victim_ordinal as usize % servers.len();
            order.push((
                servers[first].clone(),
                format!("seed (lease owner {} not mapped to a Pod)", before.owner),
            ));
        }
    }
    for s in &servers {
        if !order.iter().any(|(p, _)| p.uid == s.uid) {
            order.push((
                s.clone(),
                "lease-renewal (an earlier replica was not the leader)".into(),
            ));
        }
    }
    let mut obs = LeaseObservation {
        owner_before: before.owner.clone(),
        expires_before: before.expires_at,
        term_before: before.term,
        ..Default::default()
    };

    for (victim, basis) in order {
        let container_id = victim.container_id.clone().ok_or_else(|| {
            format!(
                "server Pod {} has no {SERVER_CONTAINER} container id",
                victim.name
            )
        })?;
        let pid = node.container_pid(&container_id).await?;
        let held = views
            .lock()
            .expect("views lock")
            .get(&t.pod_uid)
            .is_some_and(holds);

        let fired_at_ms = now_ms();
        node.signal(pid, "STOP").await?;
        let mut guard = ContGuard::new(node, pid);
        let (stopped, state) = node.wait_state(pid, true, Duration::from_secs(3)).await;
        if !stopped {
            return Err(format!(
                "SIGSTOP from node {} to PID {pid} ({}) did not stop it within 3 s: {state}",
                node.name, victim.name
            ));
        }
        log(&format!(
            "SIGSTOP from node {} to PID {pid}, container {container_id} of {} ({basis}); {state}; lease {before:?}",
            node.name, victim.name
        ));

        // Writes in flight at the stop land within this; after it, only a
        // live leader moves `expires_at`.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let base = scheduler_lease(&stack.pg, with_term)
            .await?
            .ok_or("the scheduler lease row is gone")?;
        let deadline = fired_at_ms + ((LEASE_TTL_SECS + 30) * 1000) as i64;
        let outcome = loop {
            let row = scheduler_lease(&stack.pg, with_term)
                .await?
                .ok_or("the scheduler lease row is gone")?;
            if row.owner != before.owner {
                break LeaseOutcome::Takeover(row, now_ms());
            }
            if row.expires_at > base.expires_at {
                break LeaseOutcome::StillRenewed;
            }
            if now_ms() > deadline {
                break LeaseOutcome::NoTakeover;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };

        let role = match outcome {
            LeaseOutcome::StillRenewed => {
                node.signal(pid, "CONT").await?;
                guard.disarm();
                let (resumed, state) = node.wait_state(pid, false, Duration::from_secs(3)).await;
                if !resumed {
                    return Err(format!(
                        "SIGCONT to PID {pid} ({}) did not resume it: {state}",
                        victim.name
                    ));
                }
                let probe = format!(
                    "{} stopped for {} ms: the lease kept being renewed by {}, so it was not the leader",
                    victim.name,
                    now_ms() - fired_at_ms,
                    before.owner
                );
                log(&probe);
                obs.probes.push(probe);
                continue;
            }
            LeaseOutcome::Takeover(row, at) => {
                log(&format!("takeover: {row:?} at +{} ms", at - fired_at_ms));
                obs.owner_after = Some(row.owner);
                obs.term_after = row.term;
                obs.takeover_at_ms = Some(at);
                // Keep it stopped past the TTL and the seeded hold, so the
                // new leader acts before the old one resumes.
                sleep_until_ms(
                    (at + plan.hold_ms as i64)
                        .max(fired_at_ms + (LEASE_TTL_SECS as i64 + 1) * 1000),
                )
                .await;
                "leader"
            }
            LeaseOutcome::NoTakeover => {
                log("the lease stopped being renewed but no peer took it over");
                "leader-no-takeover"
            }
        };

        // The liveness probe can restart a stopped server before this point:
        // then there is nothing to resume, and the scenario did not test a
        // stale leader resuming.
        let (still, state_before_cont) = node.wait_state(pid, true, Duration::ZERO).await;
        let role = if still {
            node.signal(pid, "CONT").await?;
            guard.disarm();
            let (resumed, state) = node.wait_state(pid, false, Duration::from_secs(3)).await;
            if !resumed {
                return Err(format!(
                    "SIGCONT to PID {pid} ({}) did not resume it: {state}",
                    victim.name
                ));
            }
            role
        } else {
            guard.disarm();
            "restarted-while-stopped"
        };
        let resumed_at_ms = now_ms();
        log(&format!(
            "SIGCONT to PID {pid} after {} ms stopped ({role}); before it: {state_before_cont}",
            resumed_at_ms - fired_at_ms
        ));

        tokio::time::sleep(Duration::from_secs(10)).await;
        if let Some(row) = scheduler_lease(&stack.pg, with_term).await? {
            obs.owner_after_resume = Some(row.owner);
            obs.term_after_resume = row.term;
        }
        return Ok(Fault {
            action: "stop-leader".into(),
            pod: victim.name.clone(),
            pod_uid: Some(victim.uid.clone()),
            grace_seconds: 0,
            trigger: Some(t.clone()),
            condition_held_at_fire: held,
            fence_pack_rows_at_fire: None,
            fired_at_ms,
            landing: None,
            signal: Some(NodeSignal {
                signal: "STOP".into(),
                node: node.name.clone(),
                container_id,
                pid,
                delivered: true,
                evidence: state,
            }),
            target_basis: Some(basis),
            victim_role: Some(role.into()),
            resumed_at_ms: Some(resumed_at_ms),
            lease: Some(obs),
            ..Default::default()
        });
    }
    Err(format!(
        "stopped each server replica in turn and the scheduler lease of {} kept being renewed each time: \
         its owner is not one of the server Pods ({:?})",
        before.owner, obs.probes
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(name: &str) -> ServerPod {
        ServerPod::from_json(&serde_json::json!({
            "metadata": {"name": name, "uid": format!("uid-{name}")},
            "status": {"containerStatuses": [
                {"name": "other", "containerID": "containerd://zzz"},
                {"name": SERVER_CONTAINER, "containerID": "containerd://abc123", "restartCount": 2, "ready": true,
                 "lastState": {"terminated": {"exitCode": 137, "signal": 9}}}
            ]}
        }))
    }

    #[test]
    fn reads_the_server_container() {
        let p = pod("scarab-6c9-x1");
        assert_eq!(p.container_id.as_deref(), Some("abc123"));
        assert_eq!((p.restarts, p.ready), (2, true));
        assert_eq!(p.last_termination["signal"], 9);
    }

    #[test]
    fn an_owner_maps_to_the_one_pod_it_names() {
        let pods = vec![pod("scarab-6c9-x1"), pod("scarab-6c9-x2")];
        assert_eq!(
            owner_names_pod("scarab-server-scarab-6c9-x2-1f2e", &pods)
                .unwrap()
                .name,
            "scarab-6c9-x2"
        );
        // Today's replica id names no Pod.
        assert!(
            owner_names_pod("scarab-server-0b6f2c1e-1111-4222-8333-944445555666", &pods).is_none()
        );
        // A name that is a prefix of another matches both: not mapped.
        let pods = vec![pod("scarab-a"), pod("scarab-a1")];
        assert!(owner_names_pod("scarab-server-scarab-a1", &pods).is_none());
    }

    #[test]
    fn the_claim_owner_still_writing_means_the_kill_hit_its_peer() {
        let obs = |owner: Option<&str>, a: Option<i64>, b: Option<i64>| ClaimObservation {
            owner: owner.map(Into::into),
            owner_written_until_5s: a,
            owner_written_until_20s: b,
            ..Default::default()
        };
        assert_eq!(
            claim_role(&obs(Some("o"), Some(100), Some(100))),
            "claim-holder"
        );
        assert_eq!(claim_role(&obs(Some("o"), Some(100), Some(200))), "peer");
        assert_eq!(claim_role(&obs(Some("o"), None, Some(200))), "peer");
        assert_eq!(claim_role(&obs(Some("o"), None, None)), "unknown");
        assert_eq!(claim_role(&obs(None, None, None)), "unknown");
    }
}
