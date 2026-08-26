//! The per-step WHY on a rerun plan (git-bug 4afaa3e): `RerunPlan.steps[]`
//! partitions the invalidation set into Target | Cascade | Regenerate |
//! RegenerateCascade, attributes each member (`because_of`), orders the list
//! for execution, and marks gates. Hermetic (InMemoryDb + a fake snapshot
//! oracle) — these are pure derivations over the DAG, so no store is involved
//! beyond "is this root present".

use std::collections::HashSet;
use std::sync::Arc;

use scarab_engine::{
    plan_rerun, AttemptId, Db, PlanReason, RunId, StepId, Timestamp, WorkspaceSnapshots,
};
use scarab_testkit::InMemoryDb;

/// A snapshot oracle over a fixed set of PRESENT roots — everything else is
/// definitively absent (the only answer allowed to widen).
struct FixedRoots(HashSet<String>);

#[async_trait::async_trait]
impl WorkspaceSnapshots for FixedRoots {
    async fn snapshot_present(&self, root: &str) -> bool {
        self.0.contains(root)
    }
    async fn file_blob_hash(&self, _root: &str, _path: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// Seed one step with its needs and a recorded output snapshot root.
async fn step(db: &InMemoryDb, run: &RunId, id: &str, needs: &[&str], root: &str) {
    let sid = StepId(id.into());
    let needs: Vec<StepId> = needs.iter().map(|n| StepId((*n).into())).collect();
    db.create_step_run(run, &sid, None, &needs, Timestamp(0))
        .await
        .unwrap();
    db.set_step_output(run, &sid, &AttemptId("a1".into()), root, Some(root))
        .await
        .unwrap();
}

/// The full partition on one DAG:
///
/// ```text
///   a → b → c(target) → d → e     (e also needs c)
///        \→ f                     g(manual gate) needs c
/// ```
///
/// `b`'s snapshot is gone, `a`'s is live: `b` is a Regenerate root (attributed
/// to its consumer `c`), `f` — b's descendant the target's cascade missed — is
/// RegenerateCascade, `d`/`e`/`g` are ordinary Cascade, `c` is the Target and
/// the only member with no `because_of`.
#[tokio::test]
async fn reasons_partition_the_set_and_attribute_every_member() {
    let db = InMemoryDb::new();
    let run = RunId("r1".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    step(&db, &run, "a", &[], "root-a").await;
    step(&db, &run, "b", &["a"], "root-b").await;
    step(&db, &run, "c", &["b"], "root-c").await;
    step(&db, &run, "d", &["c"], "root-d").await;
    step(&db, &run, "e", &["c", "d"], "root-e").await;
    step(&db, &run, "f", &["b"], "root-f").await;
    step(&db, &run, "g", &["c"], "root-g").await;
    db.set_step_gate(&run, &StepId("g".into()), "manual", None)
        .await
        .unwrap();

    // Only `b`'s root is missing; `a`'s survives, so the widening stops there.
    let oracle = FixedRoots(
        ["root-a", "root-c", "root-d", "root-e", "root-f", "root-g"]
            .into_iter()
            .map(String::from)
            .collect(),
    );

    let plan = plan_rerun(&db as &dyn Db, Some(&oracle), &run, &StepId("c".into()))
        .await
        .unwrap();

    // Execution order: topological over needs restricted to the set, ties by id.
    let order: Vec<&str> = plan.steps.iter().map(|s| s.step.0.as_str()).collect();
    assert_eq!(order, vec!["b", "c", "f", "d", "g", "e"]);

    // The partition + attribution, member by member.
    let of = |id: &str| {
        plan.steps
            .iter()
            .find(|s| s.step.0 == id)
            .unwrap_or_else(|| panic!("{id} missing from the plan"))
    };
    let b = of("b");
    assert_eq!(b.reason, PlanReason::Regenerate);
    assert_eq!(
        b.because_of,
        Some(StepId("c".into())),
        "a regenerate root is attributed to the consumer whose expired input dragged it in"
    );
    let c = of("c");
    assert_eq!(c.reason, PlanReason::Target);
    assert_eq!(c.because_of, None, "only the target has no because_of");
    let f = of("f");
    assert_eq!(
        f.reason,
        PlanReason::RegenerateCascade,
        "b's descendant the target's own cascade missed"
    );
    assert_eq!(f.because_of, Some(StepId("b".into())));
    for id in ["d", "e", "g"] {
        assert_eq!(of(id).reason, PlanReason::Cascade, "{id}");
    }
    assert_eq!(of("d").because_of, Some(StepId("c".into())));

    // The gate marker (amendment F4): the plan flags `g`, so the copy can say
    // "pauses for approval at g" instead of claiming it will run.
    assert!(of("g").is_gate, "g is a manual gate");
    for id in ["b", "c", "d", "e", "f"] {
        assert!(!of(id).is_gate, "{id} is a plain step");
    }

    // The set partitions exactly: every invalidated member appears once.
    assert_eq!(plan.steps.len(), plan.invalidated.len());
    let listed: HashSet<&StepId> = plan.steps.iter().map(|s| &s.step).collect();
    assert_eq!(listed, plan.invalidated.iter().collect::<HashSet<_>>());
}

/// Amendment F7: a multi-parent cascade member attributes to the MINIMUM
/// in-set need by id — pinned so the attribution can never flip with map
/// iteration order. `y` needs both `m` and `z` (both in the set): `m` wins.
#[tokio::test]
async fn multi_parent_cascade_attributes_to_the_minimum_in_set_need() {
    let db = InMemoryDb::new();
    let run = RunId("r2".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    step(&db, &run, "t", &[], "root-t").await;
    step(&db, &run, "m", &["t"], "root-m").await;
    step(&db, &run, "z", &["t"], "root-z").await;
    step(&db, &run, "y", &["z", "m"], "root-y").await;

    // No oracle: the plain cascade — the tie-break must hold without widening.
    let plan = plan_rerun(&db as &dyn Db, None, &run, &StepId("t".into()))
        .await
        .unwrap();
    let y = plan.steps.iter().find(|s| s.step.0 == "y").unwrap();
    assert_eq!(y.reason, PlanReason::Cascade);
    assert_eq!(
        y.because_of,
        Some(StepId("m".into())),
        "declared-needs order is [z, m]; the attribution is min BY ID, not first-declared"
    );
}

/// An ordinary (un-widened) plan still names its cascade: target + descendants
/// with reasons — this is the common case the confirm popover renders.
#[tokio::test]
async fn an_ordinary_rerun_plan_names_target_and_cascade() {
    let db = InMemoryDb::new();
    let run = RunId("r3".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    step(&db, &run, "push", &[], "root-push").await;
    step(&db, &run, "deploy-staging", &["push"], "root-ds").await;

    let oracle = FixedRoots(
        ["root-push", "root-ds"]
            .into_iter()
            .map(String::from)
            .collect(),
    );
    let plan = plan_rerun(
        &db as &dyn Db,
        Some(&oracle),
        &run,
        &StepId("push".into()),
    )
    .await
    .unwrap();
    assert!(!plan.is_widened());
    let order: Vec<(&str, PlanReason)> = plan
        .steps
        .iter()
        .map(|s| (s.step.0.as_str(), s.reason))
        .collect();
    assert_eq!(
        order,
        vec![
            ("push", PlanReason::Target),
            ("deploy-staging", PlanReason::Cascade)
        ]
    );
    assert_eq!(
        plan.steps[1].because_of,
        Some(StepId("push".into())),
        "the cascade member is attributed to the target it descends from"
    );
}

/// `Arc<InMemoryDb>` sanity: the oracle is memoised per root within one call,
/// so a diamond that consumes the same expired root twice widens once and
/// reports one ExpiredInput per producer.
#[tokio::test]
async fn a_shared_expired_producer_is_one_regenerate_root_not_two() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r4".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    step(&db, &run, "base", &[], "root-base").await;
    step(&db, &run, "left", &["base"], "root-left").await;
    step(&db, &run, "right", &["base"], "root-right").await;
    step(&db, &run, "join", &["left", "right"], "root-join").await;

    // `left` and `right` both consume base's snapshot; it is gone.
    let oracle = FixedRoots(
        ["root-left", "root-right", "root-join"]
            .into_iter()
            .map(String::from)
            .collect(),
    );
    // Target `join`: neither parent re-runs at first, so the boundary probes
    // left+right roots (present) — then nothing widens. Target `left` instead:
    // its consumed root-base is gone → base regenerates, and base's OTHER
    // descendant `right` joins as RegenerateCascade.
    let plan = plan_rerun(
        &*db as &dyn Db,
        Some(&oracle),
        &run,
        &StepId("left".into()),
    )
    .await
    .unwrap();
    let regenerates: Vec<&str> = plan
        .steps
        .iter()
        .filter(|s| s.reason == PlanReason::Regenerate)
        .map(|s| s.step.0.as_str())
        .collect();
    assert_eq!(regenerates, vec!["base"]);
    assert_eq!(plan.expired.len(), 1, "one ExpiredInput per producer");
    let right = plan.steps.iter().find(|s| s.step.0 == "right").unwrap();
    assert_eq!(right.reason, PlanReason::RegenerateCascade);
    assert_eq!(right.because_of, Some(StepId("base".into())));
}
