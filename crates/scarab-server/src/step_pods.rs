//! The step Pod sweep (git-bug 986542a): deletes the Pods, Services and
//! NetworkPolicies a finished Run left in the step namespace.
//!
//! The k8s executor keeps a step's Pod after the step ends (its Secrets are
//! deleted then, the Pod is not), so finished Pods piled up: 472 on the demo
//! box, 9 of them Pending for 72 days behind a dead-lettered Run. This sweep
//! runs in the retention sweeper's loop, under its leader lease, and deletes an
//! object only when all of these hold:
//!
//! - it carries `app.kubernetes.io/managed-by=scarab` and a `scarab.io/run`
//!   label;
//! - that Run exists in THIS server's database. Several installs can share one
//!   step namespace (the kind stack's dev, dogfood, e2e and chaos modes each
//!   have their own database), so a Pod whose Run this database does not know
//!   belongs to another install and is left alone;
//! - the Run is terminal (succeeded, failed, cancelled or dead-lettered). A
//!   live Run's Pods are what readoption after a restart reads;
//! - the Run finished more than the grace period ago
//!   (`SCARAB_RETENTION_STEP_POD_MINUTES`, default 60). The grace lets log
//!   capture finish and leaves a failed Pod to `kubectl describe` for a while;
//! - it is not a debug Pod that is still running: a debug Pod is started for a
//!   finished step on purpose, and someone may be in its shell.
//!
//! A Pod's Secrets are owner-referenced to it, so Kubernetes deletes them with
//! the Pod. Work is bounded per pass: pages of [`PAGE_SIZE`] objects, at most
//! [`MAX_PAGES`] pages per kind, at most [`DELETE_CAP`] deletions. A list or
//! delete error is logged and the pass goes on; nothing here stops the loop.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use scarab_engine::{Clock, Db, RunId, RunSettlement, Timestamp};
use scarab_executor_k8s::{run_label_value, ManagedKind, ManagedObject, ManagedPage};

/// Objects requested per list call.
pub const PAGE_SIZE: u32 = 250;
/// Pages read per kind per pass. Together with [`PAGE_SIZE`] this caps one
/// pass at 5,000 objects of each kind.
pub const MAX_PAGES: u32 = 20;
/// Deletions per pass, across all kinds. At the sweeper's five-minute cadence
/// this clears a backlog of a few hundred in a few passes.
pub const DELETE_CAP: usize = 200;

/// The default grace period, in minutes.
pub const DEFAULT_GRACE_MINUTES: u32 = 60;

const MANAGED_BY: &str = "app.kubernetes.io/managed-by";
const RUN_LABEL: &str = "scarab.io/run";
const DEBUG_LABEL: &str = "scarab.io/debug";

/// The step namespace, as the sweep sees it. Implemented for the k8s
/// executor; a deployment with another executor passes no cluster and the
/// sweep does not run.
#[async_trait::async_trait]
pub trait StepPodCluster: Send + Sync {
    /// One page of the objects of `kind` that carry the managed-by label.
    async fn list(
        &self,
        kind: ManagedKind,
        continue_token: Option<String>,
        limit: u32,
    ) -> Result<ManagedPage, String>;
    /// Delete one object. Already gone is success.
    async fn delete(&self, kind: ManagedKind, name: &str) -> Result<(), String>;
}

#[async_trait::async_trait]
impl StepPodCluster for scarab_executor_k8s::K8sExecutor {
    async fn list(
        &self,
        kind: ManagedKind,
        continue_token: Option<String>,
        limit: u32,
    ) -> Result<ManagedPage, String> {
        self.list_managed(kind, continue_token, limit)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete(&self, kind: ManagedKind, name: &str) -> Result<(), String> {
        self.delete_managed(kind, name)
            .await
            .map_err(|e| e.to_string())
    }
}

/// What the sweeper loop needs to run the sweep: the cluster and the grace.
#[derive(Clone)]
pub struct StepPodSweep {
    pub cluster: Arc<dyn StepPodCluster>,
    pub grace_ms: i64,
}

/// One object [`select_leftovers`] chose, with the Run it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub kind: ManagedKind,
    pub name: String,
    pub run: RunId,
}

/// The `scarab.io/run` label values of the managed objects in `objects`,
/// deduplicated: the ids to ask the database about. A label is used as a Run
/// id as it stands; every id the server mints is a lowercase UUID, which the
/// executor stamps unchanged.
pub fn candidate_runs(objects: &[ManagedObject]) -> Vec<RunId> {
    let labels: BTreeSet<&str> = objects
        .iter()
        .filter(|o| o.labels.get(MANAGED_BY).map(String::as_str) == Some("scarab"))
        .filter_map(|o| o.labels.get(RUN_LABEL).map(String::as_str))
        .collect();
    labels.into_iter().map(|l| RunId(l.to_string())).collect()
}

/// Which of `objects` to delete, given what this database knows about their
/// Runs (`runs`, as [`Db::run_settlements`] returned it), the time `now`, the
/// grace period and how many deletions are left this pass. Pure; see the
/// module docs for the rules. A Run is matched to an object only when the
/// executor's label for that Run's id equals the object's label, so an id the
/// label mapping would change never matches anything.
pub fn select_leftovers(
    objects: &[ManagedObject],
    runs: &[RunSettlement],
    now: Timestamp,
    grace_ms: i64,
    cap: usize,
) -> Vec<Leftover> {
    // A Run matches a label only when the label is its id unchanged: an id the
    // label mapping would lowercase or truncate could share its label with
    // another Run, so its objects are kept.
    let by_label: HashMap<&str, &RunSettlement> = runs
        .iter()
        .filter(|s| run_label_value(&s.run) == s.run.0)
        .map(|s| (s.run.0.as_str(), s))
        .collect();
    objects
        .iter()
        .filter(|o| o.labels.get(MANAGED_BY).map(String::as_str) == Some("scarab"))
        .filter(|o| !(o.active && o.labels.get(DEBUG_LABEL).map(String::as_str) == Some("true")))
        .filter_map(|o| {
            let run = by_label.get(o.labels.get(RUN_LABEL)?.as_str())?;
            (run.status.is_terminal() && now.0 - run.settled_at.0 > grace_ms).then(|| Leftover {
                kind: o.kind,
                name: o.name.clone(),
                run: run.run.clone(),
            })
        })
        .take(cap)
        .collect()
}

/// What one sweep pass deleted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StepPodSweepReport {
    pub deleted: Vec<Leftover>,
}

/// One sweep pass over the step namespace. A replica that does not hold the
/// retention lease returns an empty report without listing anything. A
/// database error ends the pass with `Err`; a list or delete error is logged
/// and the pass moves on.
pub async fn sweep_step_pods(
    db: &Arc<dyn Db>,
    cluster: &dyn StepPodCluster,
    clock: &Arc<dyn Clock>,
    owner: &str,
    grace_ms: i64,
) -> Result<StepPodSweepReport, String> {
    let lease = db
        .lease(
            crate::retention::RETENTION_LEASE,
            owner,
            crate::retention::RETENTION_LEASE_TTL_MS,
        )
        .await
        .map_err(|e| e.to_string())?;
    if lease.owner != owner {
        return Ok(StepPodSweepReport::default());
    }
    let now = clock.now().await;
    let mut report = StepPodSweepReport::default();
    'kinds: for kind in ManagedKind::ALL {
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            if report.deleted.len() >= DELETE_CAP {
                break 'kinds;
            }
            let page = match cluster.list(kind, token.take(), PAGE_SIZE).await {
                Ok(page) => page,
                Err(e) => {
                    tracing::warn!(?kind, error = %e, "step pod sweep: list failed");
                    continue 'kinds;
                }
            };
            let candidates = candidate_runs(&page.objects);
            let runs = if candidates.is_empty() {
                Vec::new()
            } else {
                db.run_settlements(&candidates)
                    .await
                    .map_err(|e| e.to_string())?
            };
            let cap = DELETE_CAP - report.deleted.len();
            for leftover in select_leftovers(&page.objects, &runs, now, grace_ms, cap) {
                match cluster.delete(leftover.kind, &leftover.name).await {
                    Ok(()) => report.deleted.push(leftover),
                    Err(e) => tracing::warn!(
                        ?kind,
                        name = %leftover.name,
                        run = %leftover.run.0,
                        error = %e,
                        "step pod sweep: delete failed (the next pass retries)"
                    ),
                }
            }
            match page.continue_token {
                Some(t) => token = Some(t),
                None => continue 'kinds,
            }
        }
    }
    if !report.deleted.is_empty() {
        let runs: BTreeSet<&str> = report.deleted.iter().map(|l| l.run.0.as_str()).collect();
        tracing::info!(
            deleted = report.deleted.len(),
            runs = ?runs,
            "step pod sweep: deleted objects of finished runs"
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarab_engine::RunStatus;
    use std::collections::BTreeMap;

    const GRACE: i64 = 60 * 60 * 1000;
    const NOW: Timestamp = Timestamp(10 * GRACE);

    fn obj(kind: ManagedKind, name: &str, run: Option<&str>) -> ManagedObject {
        let mut labels = BTreeMap::from([(MANAGED_BY.to_string(), "scarab".to_string())]);
        if let Some(run) = run {
            labels.insert(RUN_LABEL.to_string(), run.to_string());
        }
        ManagedObject {
            kind,
            name: name.to_string(),
            labels,
            active: false,
        }
    }

    fn pod(name: &str, run: &str) -> ManagedObject {
        obj(ManagedKind::Pod, name, Some(run))
    }

    fn settled(run: &str, status: RunStatus, at: i64) -> RunSettlement {
        RunSettlement {
            run: RunId(run.to_string()),
            status,
            settled_at: Timestamp(at),
        }
    }

    fn names(l: &[Leftover]) -> Vec<&str> {
        l.iter().map(|l| l.name.as_str()).collect()
    }

    const OLD: i64 = NOW.0 - GRACE - 1;

    #[test]
    fn a_pod_of_a_run_this_database_does_not_know_is_left_alone() {
        let objects = [pod("p", "other-install-run")];
        assert!(select_leftovers(&objects, &[], NOW, GRACE, 10).is_empty());
    }

    #[test]
    fn a_pod_of_a_non_terminal_run_is_left_alone_whatever_its_age() {
        for status in [RunStatus::Pending, RunStatus::Running, RunStatus::Suspended] {
            let objects = [pod("p", "r")];
            let runs = [settled("r", status, 0)];
            assert!(
                select_leftovers(&objects, &runs, NOW, GRACE, 10).is_empty(),
                "{status:?}"
            );
        }
    }

    #[test]
    fn a_terminal_run_inside_the_grace_keeps_its_pods() {
        let objects = [pod("p", "r")];
        for at in [NOW.0, NOW.0 - 1, NOW.0 - GRACE] {
            let runs = [settled("r", RunStatus::Failed, at)];
            assert!(
                select_leftovers(&objects, &runs, NOW, GRACE, 10).is_empty(),
                "settled at {at}"
            );
        }
    }

    #[test]
    fn every_terminal_status_past_the_grace_is_deleted() {
        for status in [
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
            RunStatus::DeadLettered,
        ] {
            let objects = [pod("p", "r")];
            let runs = [settled("r", status, OLD)];
            assert_eq!(
                select_leftovers(&objects, &runs, NOW, GRACE, 10),
                vec![Leftover {
                    kind: ManagedKind::Pod,
                    name: "p".into(),
                    run: RunId("r".into()),
                }],
                "{status:?}"
            );
        }
    }

    #[test]
    fn a_pending_pod_of_a_dead_lettered_run_is_deleted() {
        // The demo's 9 Pending Pods (Run 01a04915, dead-lettered 72 days ago).
        let mut p = pod("scarab-image-a1-x", "01a04915-31b0-7b91-aede-5a842d3244cb");
        p.active = true;
        let runs = [settled(
            "01a04915-31b0-7b91-aede-5a842d3244cb",
            RunStatus::DeadLettered,
            NOW.0 - 72 * 24 * GRACE,
        )];
        assert_eq!(
            names(&select_leftovers(&[p], &runs, NOW, GRACE, 10)),
            ["scarab-image-a1-x"]
        );
    }

    #[test]
    fn an_object_without_the_managed_by_label_is_left_alone() {
        let mut p = pod("p", "r");
        p.labels.remove(MANAGED_BY);
        let mut q = pod("q", "r");
        q.labels.insert(MANAGED_BY.into(), "helm".into());
        let runs = [settled("r", RunStatus::Succeeded, OLD)];
        assert!(select_leftovers(&[p, q], &runs, NOW, GRACE, 10).is_empty());
    }

    #[test]
    fn an_object_without_a_run_label_is_left_alone() {
        let objects = [obj(ManagedKind::Pod, "p", None)];
        let runs = [settled("r", RunStatus::Succeeded, OLD)];
        assert!(select_leftovers(&objects, &runs, NOW, GRACE, 10).is_empty());
    }

    #[test]
    fn a_running_debug_pod_is_left_alone_and_a_finished_one_is_deleted() {
        let mut running = pod("debug-running", "r");
        running.labels.insert(DEBUG_LABEL.into(), "true".into());
        running.active = true;
        let mut done = running.clone();
        done.name = "debug-done".into();
        done.active = false;
        let runs = [settled("r", RunStatus::Succeeded, OLD)];
        assert_eq!(
            names(&select_leftovers(&[running, done], &runs, NOW, GRACE, 10)),
            ["debug-done"]
        );
    }

    #[test]
    fn service_leftovers_are_deleted_with_the_run() {
        let objects = [
            obj(ManagedKind::Pod, "scarab-svc-db-1-t0", Some("r")),
            obj(ManagedKind::Service, "db", Some("r")),
            obj(ManagedKind::NetworkPolicy, "scarab-svc-db-1-t0", Some("r")),
        ];
        let runs = [settled("r", RunStatus::Cancelled, OLD)];
        let kinds: Vec<ManagedKind> = select_leftovers(&objects, &runs, NOW, GRACE, 10)
            .into_iter()
            .map(|l| l.kind)
            .collect();
        assert_eq!(kinds, ManagedKind::ALL);
    }

    #[test]
    fn the_cap_bounds_the_deletions() {
        let objects: Vec<ManagedObject> = (0..5).map(|i| pod(&format!("p{i}"), "r")).collect();
        let runs = [settled("r", RunStatus::Succeeded, OLD)];
        assert_eq!(
            names(&select_leftovers(&objects, &runs, NOW, GRACE, 3)),
            ["p0", "p1", "p2"]
        );
        assert!(select_leftovers(&objects, &runs, NOW, GRACE, 0).is_empty());
    }

    #[test]
    fn a_run_id_the_label_mapping_changes_never_matches() {
        // The executor lowercases a label; an id with capitals is stamped as a
        // different string, so the database row for it must not claim a Pod
        // labelled with the lowercase form.
        let objects = [pod("p", "run-a")];
        let runs = [settled("Run-A", RunStatus::Succeeded, OLD)];
        assert!(select_leftovers(&objects, &runs, NOW, GRACE, 10).is_empty());
    }

    #[test]
    fn mixed_page_deletes_only_what_the_rules_allow() {
        let objects = [
            pod("done-old", "done"),
            pod("done-fresh", "fresh"),
            pod("live", "live"),
            pod("foreign", "foreign"),
        ];
        let runs = [
            settled("done", RunStatus::Succeeded, OLD),
            settled("fresh", RunStatus::Succeeded, NOW.0 - 10),
            settled("live", RunStatus::Running, 0),
        ];
        assert_eq!(
            names(&select_leftovers(&objects, &runs, NOW, GRACE, 10)),
            ["done-old"]
        );
    }

    #[test]
    fn candidate_runs_are_the_distinct_run_labels_of_managed_objects() {
        let mut unmanaged = pod("u", "unmanaged-run");
        unmanaged.labels.remove(MANAGED_BY);
        let objects = [
            pod("a", "r1"),
            pod("b", "r1"),
            pod("c", "r2"),
            obj(ManagedKind::Pod, "d", None),
            unmanaged,
        ];
        assert_eq!(
            candidate_runs(&objects),
            vec![RunId("r1".into()), RunId("r2".into())]
        );
    }
}
