//! Webhook delivery outcomes and the delivery row's try lease (ADR-0083),
//! as a contract both stores keep: each case runs on the in-memory twin
//! and on real Postgres (skipped without `SCARAB_TEST_DATABASE_URL`). The
//! Postgres-only cases hold a row lock in a raw transaction to build the
//! race instead of hoping for it.

mod common;

use std::sync::Arc;

use common::{fresh_db, wait_until_blocked};
use scarab_db_postgres::PostgresDb;
use scarab_engine::{
    Db, DeliveryOutcomeKind, DeliveryPipeline, DeliveryRefusal, DeliveryRunClaim, HeadQuery,
    HeadScope, OutboxId, OutboxMessage, OutcomeWrite, RefusedRun, RunId, RunStatus, Timestamp,
};
use scarab_forge::{DeliveryPins, DeliveryTry, ForgeConnectionStore, ForgeKind};
use scarab_testkit::{FakeClock, InMemoryDb};
use sqlx::PgPool;

const LEASE_MS: i64 = scarab_engine::UNFINISHED_CREATION_BOUND_MS;

/// One store under test, with what the contract needs that the ports do not
/// say: lapsing a lease and letting the store's clock move on.
enum Store {
    Mem(Arc<InMemoryDb>, Arc<FakeClock>),
    Pg(Arc<PostgresDb>, PgPool),
}

impl Store {
    fn mem() -> Self {
        let db = Arc::new(InMemoryDb::new());
        let clock = Arc::new(FakeClock::new(1_000));
        db.use_clock(clock.clone());
        Store::Mem(db, clock)
    }

    fn db(&self) -> &dyn Db {
        match self {
            Store::Mem(db, _) => db.as_ref(),
            Store::Pg(db, _) => db.as_ref(),
        }
    }

    fn registry(&self) -> &dyn ForgeConnectionStore {
        match self {
            Store::Mem(db, _) => db.as_ref(),
            Store::Pg(db, _) => db.as_ref(),
        }
    }

    async fn expire(&self, id: &str) {
        match self {
            Store::Mem(db, _) => db.expire_delivery_try(ForgeKind::GitHub, id),
            Store::Pg(_, pool) => {
                sqlx::query(
                    "UPDATE webhook_deliveries SET try_until = 0 WHERE forge = 'github' AND id = $1",
                )
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
            }
        }
    }

    /// Let the store's clock move on, so a later row's `at` is later.
    async fn later(&self) {
        match self {
            Store::Mem(_, clock) => clock.advance(1_000),
            Store::Pg(..) => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
        }
    }
}

/// Run `case` on the in-memory twin and on Postgres.
macro_rules! both {
    ($mem:ident, $pg:ident, $case:ident) => {
        #[tokio::test]
        async fn $mem() {
            $case(&Store::mem()).await;
        }

        #[tokio::test]
        async fn $pg() {
            let Some(tdb) = fresh_db().await else {
                eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
                return;
            };
            let db = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
            db.migrate().await.unwrap();
            $case(&Store::Pg(db, tdb.pool.clone())).await;
            tdb.cleanup().await;
        }
    };
}

fn key(delivery: &str, pipeline: &str) -> DeliveryPipeline {
    DeliveryPipeline {
        connection_id: "acme-conn".into(),
        delivery_id: delivery.into(),
        pipeline: pipeline.into(),
        forge: "github".into(),
    }
}

fn claim(delivery: &str, pipeline: &str, replaces: Option<&RunId>) -> DeliveryRunClaim {
    DeliveryRunClaim {
        key: key(delivery, pipeline),
        replaces: replaces.cloned(),
        at: Timestamp(1_000),
    }
}

fn refused_row(pipeline: &str) -> RefusedRun {
    RefusedRun {
        id: 0,
        org: "acme".into(),
        project: "app".into(),
        pipeline: pipeline.into(),
        trigger_kind: "push".into(),
        actor: None,
        git_ref: Some("refs/heads/main".into()),
        sha: Some("abc123".into()),
        title: None,
        reason_kind: "compile".into(),
        message: "pipeline: broken".into(),
        errors: serde_json::json!([]),
        created_at: Timestamp(1_000),
    }
}

fn status(key: &str) -> OutboxMessage {
    OutboxMessage {
        id: OutboxId(0),
        run: RunId(format!("status-{key}")),
        kind: "refusal_status".into(),
        payload: serde_json::json!({ "sha": "abc123" }),
        idempotency_key: format!("refusal_status:{key}"),
        at: Timestamp(1_000),
    }
}

fn refusal(pipeline: &str, status_key: &str, replaces: Option<&RunId>) -> DeliveryRefusal {
    DeliveryRefusal {
        outcome: DeliveryOutcomeKind::Refused,
        reason_kind: "compile".into(),
        refused: Some(refused_row(pipeline)),
        status: Some(status(status_key)),
        replaces: replaces.cloned(),
        at: Timestamp(1_000),
    }
}

fn claimed_run(write: OutcomeWrite) -> Option<RunId> {
    match write {
        OutcomeWrite::Claimed(Some(o)) => o.run_id,
        other => panic!("want a claimed outcome, got {other:?}"),
    }
}

/// The outbox rows `key`s refusal statuses left, still queued.
async fn queued_statuses(db: &dyn Db, keys: &[&str]) -> usize {
    db.claim_outbox("probe", Some("refusal_status"), 100, 0)
        .await
        .unwrap()
        .iter()
        .filter(|m| keys.iter().any(|k| m.idempotency_key.ends_with(k)))
        .count()
}

/// C1: the four answers of a try's begin. The first call is the first try;
/// a second while it holds the lease is held; an unfinished end clears the
/// lease and the next call takes it with what was pinned; a lapsed lease is
/// taken; a finished end makes every later call a replay. Kills: the two
/// stores disagreeing on any answer; a lease not cleared on an unfinished
/// end.
async fn try_lease_contract(s: &Store) {
    let r = s.registry();
    assert!(matches!(
        r.begin_delivery_try(ForgeKind::GitHub, "d1", LEASE_MS)
            .await
            .unwrap(),
        DeliveryTry::First { .. }
    ));
    assert_eq!(
        r.begin_delivery_try(ForgeKind::GitHub, "d1", LEASE_MS)
            .await
            .unwrap(),
        DeliveryTry::Held
    );
    // The same id of the other forge is its own delivery.
    assert!(matches!(
        r.begin_delivery_try(ForgeKind::Forgejo, "d1", LEASE_MS)
            .await
            .unwrap(),
        DeliveryTry::First { .. }
    ));
    let pins = DeliveryPins {
        config_sha: "sha1".into(),
        head_scope: Some("acme/app:refs/heads/main".into()),
        head_sha: Some("sha1".into()),
    };
    assert_eq!(
        r.pin_delivery(ForgeKind::GitHub, "d1", &pins)
            .await
            .unwrap(),
        pins
    );
    let other = DeliveryPins {
        config_sha: "sha2".into(),
        head_scope: None,
        head_sha: None,
    };
    assert_eq!(
        r.pin_delivery(ForgeKind::GitHub, "d1", &other)
            .await
            .unwrap(),
        pins,
        "the first pin wins"
    );

    r.end_delivery_try(ForgeKind::GitHub, "d1", false)
        .await
        .unwrap();
    match r
        .begin_delivery_try(ForgeKind::GitHub, "d1", LEASE_MS)
        .await
        .unwrap()
    {
        DeliveryTry::Again { pins: got, .. } => assert_eq!(got, Some(pins.clone())),
        other => panic!("an unfinished delivery with a free lease: {other:?}"),
    }
    assert_eq!(
        r.begin_delivery_try(ForgeKind::GitHub, "d1", LEASE_MS)
            .await
            .unwrap(),
        DeliveryTry::Held
    );
    s.expire("d1").await;
    assert!(matches!(
        r.begin_delivery_try(ForgeKind::GitHub, "d1", LEASE_MS)
            .await
            .unwrap(),
        DeliveryTry::Again { .. }
    ));

    r.end_delivery_try(ForgeKind::GitHub, "d1", true)
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            r.begin_delivery_try(ForgeKind::GitHub, "d1", LEASE_MS)
                .await
                .unwrap(),
            DeliveryTry::Finished
        );
    }
}
both!(try_lease_in_memory, try_lease_postgres, try_lease_contract);

/// C2: the first outcome written for a key wins, and nothing of the losing
/// write remains: no Run row, no second refused row, no second status. A
/// key clash is an answer, not an error. Kills: `ON CONFLICT DO NOTHING`
/// followed by a commit of the rest (a `creating` Run of the loser); a key
/// clash returned as an error.
async fn first_outcome_wins_contract(s: &Store) {
    let db = s.db();
    let (r1, r2) = (RunId("r-1".into()), RunId("r-2".into()));
    assert_eq!(
        db.begin_delivery_run_creation(&r1, 1, 1, &claim("d1", ".scarab/a.yaml", None))
            .await
            .unwrap(),
        OutcomeWrite::Written
    );
    let lost = db
        .begin_delivery_run_creation(&r2, 1, 1, &claim("d1", ".scarab/a.yaml", None))
        .await
        .unwrap();
    assert_eq!(claimed_run(lost), Some(r1.clone()));
    assert_eq!(
        db.run_status(&r2).await.unwrap(),
        None,
        "the loser's Run row"
    );

    let key_b = key("d1", ".scarab/b.yaml");
    assert_eq!(
        db.record_delivery_refusal(&key_b, &refusal(".scarab/b.yaml", "b-1", None))
            .await
            .unwrap(),
        OutcomeWrite::Written
    );
    let lost = db
        .record_delivery_refusal(&key_b, &refusal(".scarab/b.yaml", "b-2", None))
        .await
        .unwrap();
    match lost {
        OutcomeWrite::Claimed(Some(o)) => {
            assert_eq!(o.outcome, DeliveryOutcomeKind::Refused);
            assert_eq!(o.reason_kind.as_deref(), Some("compile"));
            assert!(o.refused_run_id.is_some());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        db.refused_runs_for_tenant("acme", "app", 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(queued_statuses(db, &["b-1", "b-2"]).await, 1);

    let key_c = key("d1", ".scarab/c.yaml");
    assert_eq!(
        db.record_delivery_outcome(&key_c, DeliveryOutcomeKind::NoMatch, Timestamp(1))
            .await
            .unwrap(),
        OutcomeWrite::Written
    );
    assert!(matches!(
        db.record_delivery_outcome(&key_c, DeliveryOutcomeKind::AllExcluded, Timestamp(2))
            .await
            .unwrap(),
        OutcomeWrite::Claimed(Some(o)) if o.outcome == DeliveryOutcomeKind::NoMatch
    ));

    let mut recorded: Vec<(String, DeliveryOutcomeKind)> = db
        .delivery_outcomes("acme-conn", "d1")
        .await
        .unwrap()
        .into_iter()
        .map(|o| (o.pipeline, o.outcome))
        .collect();
    recorded.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        recorded,
        [
            (".scarab/a.yaml".to_string(), DeliveryOutcomeKind::Run),
            (".scarab/b.yaml".to_string(), DeliveryOutcomeKind::Refused),
            (".scarab/c.yaml".to_string(), DeliveryOutcomeKind::NoMatch),
        ]
    );
    assert!(db
        .delivery_outcomes("other-conn", "d1")
        .await
        .unwrap()
        .is_empty());
}
both!(
    first_outcome_wins_in_memory,
    first_outcome_wins_postgres,
    first_outcome_wins_contract
);

/// C3: a replacement moves the outcome from the unfinished Run once and
/// marks it replaced; a second replacement from the same Run is claimed and
/// writes no Run. Kills: the `AND run_id = $old` guard dropped;
/// `replaced_by` or the new first row written outside the transaction.
async fn a_replacement_moves_the_outcome_once_contract(s: &Store) {
    let db = s.db();
    let (r1, r2, r3) = (
        RunId("r-1".into()),
        RunId("r-2".into()),
        RunId("r-3".into()),
    );
    db.begin_delivery_run_creation(&r1, 1, 1, &claim("d1", ".scarab/a.yaml", None))
        .await
        .unwrap();
    assert_eq!(
        db.begin_delivery_run_creation(&r2, 1, 1, &claim("d1", ".scarab/a.yaml", Some(&r1)))
            .await
            .unwrap(),
        OutcomeWrite::Written
    );
    let outcome = &db.delivery_outcomes("acme-conn", "d1").await.unwrap()[0];
    assert_eq!(outcome.run_id, Some(r2.clone()));
    let state = db.run_creation_state(&r1).await.unwrap().unwrap();
    assert_eq!(state.replaced_by, Some(r2.clone()));

    let lost = db
        .begin_delivery_run_creation(&r3, 1, 1, &claim("d1", ".scarab/a.yaml", Some(&r1)))
        .await
        .unwrap();
    assert_eq!(claimed_run(lost), Some(r2.clone()));
    assert_eq!(db.run_status(&r3).await.unwrap(), None);
    assert_eq!(
        db.run_creation_state(&r1)
            .await
            .unwrap()
            .unwrap()
            .replaced_by,
        Some(r2.clone())
    );

    // A refusal overwrites a recorded Run only while it still names it.
    let mut overwrite = refusal(".scarab/a.yaml", "a-1", Some(&r1));
    overwrite.outcome = DeliveryOutcomeKind::NotRun;
    overwrite.reason_kind = "not_head".into();
    overwrite.refused = None;
    assert!(matches!(
        db.record_delivery_refusal(&key("d1", ".scarab/a.yaml"), &overwrite)
            .await
            .unwrap(),
        OutcomeWrite::Claimed(_)
    ));
    overwrite.replaces = Some(r2.clone());
    assert_eq!(
        db.record_delivery_refusal(&key("d1", ".scarab/a.yaml"), &overwrite)
            .await
            .unwrap(),
        OutcomeWrite::Written
    );
    let outcome = &db.delivery_outcomes("acme-conn", "d1").await.unwrap()[0];
    assert_eq!(outcome.outcome, DeliveryOutcomeKind::NotRun);
    assert_eq!(outcome.reason_kind.as_deref(), Some("not_head"));
    assert_eq!(outcome.run_id, None);
}
both!(
    a_replacement_moves_the_outcome_once_in_memory,
    a_replacement_moves_the_outcome_once_postgres,
    a_replacement_moves_the_outcome_once_contract
);

/// C4: finishing a Run's creation is a compare-and-set on a Run still
/// creating, still `Pending` and not replaced. Kills: the unconditional
/// update of 0.7.1; the fake clearing `creating` where Postgres does not.
async fn finish_is_a_compare_and_set_contract(s: &Store) {
    let db = s.db();
    let open = RunId("r-open".into());
    db.begin_run_creation(&open, 1, 1, Timestamp(1))
        .await
        .unwrap();
    assert!(db.finish_run_creation(&open).await.unwrap());
    assert!(
        !db.finish_run_creation(&open).await.unwrap(),
        "finished twice"
    );

    let failed = RunId("r-failed".into());
    db.begin_run_creation(&failed, 1, 1, Timestamp(1))
        .await
        .unwrap();
    scarab_engine::fail_run_creation(db, &failed, "stopped".into(), Timestamp(2))
        .await
        .unwrap();
    assert!(!db.finish_run_creation(&failed).await.unwrap());
    assert!(db.run_creation_unfinished(&failed).await.unwrap());
    assert_eq!(
        db.run_status(&failed).await.unwrap(),
        Some(RunStatus::Failed)
    );

    let (old, new) = (RunId("r-old".into()), RunId("r-new".into()));
    db.begin_delivery_run_creation(&old, 1, 1, &claim("d1", ".scarab/a.yaml", None))
        .await
        .unwrap();
    db.begin_delivery_run_creation(&new, 1, 1, &claim("d1", ".scarab/a.yaml", Some(&old)))
        .await
        .unwrap();
    assert!(
        !db.finish_run_creation(&old).await.unwrap(),
        "a replaced Run"
    );
    assert!(db.run_creation_unfinished(&old).await.unwrap());
    assert!(db.finish_run_creation(&new).await.unwrap());
}
both!(
    finish_is_a_compare_and_set_in_memory,
    finish_is_a_compare_and_set_postgres,
    finish_is_a_compare_and_set_contract
);

/// A Run of `pipeline` in `org/app`, on `git_ref` (and pull request `pr`)
/// at `sha`, created at `at`.
#[allow(clippy::too_many_arguments)] // one row's columns
async fn seed_run(
    db: &dyn Db,
    id: &str,
    org: &str,
    pipeline: &str,
    git_ref: &str,
    pr: Option<i64>,
    sha: &str,
    at: i64,
) {
    let run = RunId(id.into());
    db.create_run(&run, 1, 1, Timestamp(at)).await.unwrap();
    db.set_run_tenant(&run, org, "app").await.unwrap();
    db.set_run_pipeline(&run, pipeline).await.unwrap();
    let kind = if pr.is_some() { "pull_request" } else { "push" };
    db.set_run_origin(&run, kind, None, Some(git_ref), Some(sha), pr, None)
        .await
        .unwrap();
}

fn head(scope: HeadScope, pipeline: Option<&str>, sha: &str, after: i64) -> HeadQuery {
    HeadQuery {
        org: "acme".into(),
        project: "app".into(),
        scope,
        pipeline_name: pipeline.map(str::to_string),
        pipeline_path: pipeline.map(|p| format!(".scarab/{p}.yaml")),
        head_sha: sha.into(),
        after: Timestamp(after),
    }
}

/// C5: the head rule's lookup. Only a Run of the same tenant, Pipeline and
/// scope created after the delivery counts, and only at another commit; a
/// newer delivery of the same scope at another commit counts when it
/// decided the same file. Kills: the Postgres query (which the server cases
/// never run) missing the lower bound, the scope, the tenant or the file.
async fn head_lookup_contract(s: &Store) {
    let db = s.db();
    let main = || HeadScope::Ref("refs/heads/main".into());
    seed_run(
        db,
        "before",
        "acme",
        "gamma",
        "refs/heads/main",
        None,
        "s0",
        100,
    )
    .await;
    seed_run(
        db,
        "other-ref",
        "acme",
        "gamma",
        "refs/heads/other",
        None,
        "s9",
        300,
    )
    .await;
    seed_run(
        db,
        "other-pipe",
        "acme",
        "delta",
        "refs/heads/main",
        None,
        "s8",
        300,
    )
    .await;
    seed_run(
        db,
        "other-org",
        "else",
        "gamma",
        "refs/heads/main",
        None,
        "s7",
        300,
    )
    .await;
    seed_run(
        db,
        "pr",
        "acme",
        "gamma",
        "refs/pull/7/head",
        Some(7),
        "h2",
        300,
    )
    .await;
    assert!(!db
        .head_moved(&head(main(), Some("gamma"), "s1", 200))
        .await
        .unwrap());
    assert!(db
        .head_moved(&head(HeadScope::PullRequest(7), Some("gamma"), "h1", 200))
        .await
        .unwrap());
    assert!(!db
        .head_moved(&head(HeadScope::PullRequest(7), Some("gamma"), "h2", 200))
        .await
        .unwrap());
    assert!(db
        .head_moved(&head(HeadScope::PullRequest(7), None, "h1", 200))
        .await
        .unwrap());
    seed_run(
        db,
        "same",
        "acme",
        "gamma",
        "refs/heads/main",
        None,
        "s1",
        400,
    )
    .await;
    assert!(!db
        .head_moved(&head(main(), Some("gamma"), "s1", 200))
        .await
        .unwrap());
    seed_run(
        db,
        "newer",
        "acme",
        "gamma",
        "refs/heads/main",
        None,
        "s2",
        500,
    )
    .await;
    assert!(db
        .head_moved(&head(main(), Some("gamma"), "s1", 200))
        .await
        .unwrap());

    // Newer deliveries: no Run of `beta` anywhere, but a later delivery of
    // `main` at another commit decided `.scarab/beta.yaml`.
    let r = s.registry();
    let at_old = match r
        .begin_delivery_try(ForgeKind::GitHub, "d-old", LEASE_MS)
        .await
        .unwrap()
    {
        DeliveryTry::First { at } => at,
        other => panic!("{other:?}"),
    };
    let scope = main().key("acme", "app");
    r.pin_delivery(
        ForgeKind::GitHub,
        "d-old",
        &DeliveryPins {
            config_sha: "s1".into(),
            head_scope: Some(scope.clone()),
            head_sha: Some("s1".into()),
        },
    )
    .await
    .unwrap();
    s.later().await;
    r.begin_delivery_try(ForgeKind::GitHub, "d-new", LEASE_MS)
        .await
        .unwrap();
    r.pin_delivery(
        ForgeKind::GitHub,
        "d-new",
        &DeliveryPins {
            config_sha: "s3".into(),
            head_scope: Some(scope),
            head_sha: Some("s3".into()),
        },
    )
    .await
    .unwrap();
    assert!(!db
        .head_moved(&head(main(), Some("beta"), "s1", at_old))
        .await
        .unwrap());
    db.record_delivery_outcome(
        &key("d-new", ".scarab/beta.yaml"),
        DeliveryOutcomeKind::NoMatch,
        Timestamp(at_old),
    )
    .await
    .unwrap();
    assert!(db
        .head_moved(&head(main(), Some("beta"), "s1", at_old))
        .await
        .unwrap());
    assert!(!db
        .head_moved(&head(main(), Some("beta"), "s3", at_old))
        .await
        .unwrap());
    assert!(!db
        .head_moved(&head(main(), Some("other"), "s1", at_old))
        .await
        .unwrap());
}
both!(
    head_lookup_in_memory,
    head_lookup_postgres,
    head_lookup_contract
);

/// R1's store half: a delivery and every outcome it recorded go together,
/// by the delivery's arrival, whatever the outcomes' own `at`.
async fn prune_takes_a_delivery_with_its_outcomes_contract(s: &Store) {
    let r = s.registry();
    let db = s.db();
    let at = match r
        .begin_delivery_try(ForgeKind::GitHub, "d-old", LEASE_MS)
        .await
        .unwrap()
    {
        DeliveryTry::First { at } => at,
        other => panic!("{other:?}"),
    };
    for (pipeline, outcome_at) in [(".scarab/a.yaml", at), (".scarab/b.yaml", i64::MAX / 2)] {
        db.record_delivery_outcome(
            &key("d-old", pipeline),
            DeliveryOutcomeKind::NoMatch,
            Timestamp(outcome_at),
        )
        .await
        .unwrap();
    }
    s.later().await;
    r.begin_delivery_try(ForgeKind::GitHub, "d-new", LEASE_MS)
        .await
        .unwrap();
    db.record_delivery_outcome(
        &key("d-new", ".scarab/a.yaml"),
        DeliveryOutcomeKind::NoMatch,
        Timestamp(at),
    )
    .await
    .unwrap();

    assert_eq!(
        db.prune_webhook_deliveries(Timestamp(at + 1), 1_000)
            .await
            .unwrap(),
        1
    );
    assert!(db
        .delivery_outcomes("acme-conn", "d-old")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        db.delivery_outcomes("acme-conn", "d-new")
            .await
            .unwrap()
            .len(),
        1
    );
    // The pruned id is a new delivery again; the kept one is held.
    assert!(matches!(
        r.begin_delivery_try(ForgeKind::GitHub, "d-old", LEASE_MS)
            .await
            .unwrap(),
        DeliveryTry::First { .. }
    ));
    assert_eq!(
        r.begin_delivery_try(ForgeKind::GitHub, "d-new", LEASE_MS)
            .await
            .unwrap(),
        DeliveryTry::Held
    );
}
both!(
    prune_takes_a_delivery_with_its_outcomes_in_memory,
    prune_takes_a_delivery_with_its_outcomes_postgres,
    prune_takes_a_delivery_with_its_outcomes_contract
);

fn allow_record(id: &str, run: &str) -> scarab_policy::store::DecisionRecord {
    use scarab_policy::evaluate::{DecisionOutcome, PolicyDecision};
    scarab_policy::store::DecisionRecord {
        id: id.into(),
        org: "acme".into(),
        project: Some("app".into()),
        run_id: Some(run.into()),
        step_id: None,
        attempt: None,
        decision: PolicyDecision {
            checkpoint: scarab_policy::policy::Checkpoint::RunCreate,
            schema_version: 1,
            input: serde_json::json!({}),
            policy_set_digest: "d".into(),
            outcome: DecisionOutcome::Allow,
            results: vec![],
        },
        at: 1,
    }
}

/// P1: an outcome row another transaction holds uncommitted. The Run writer
/// with the `run.create` decision (Postgres writes the decision, the first
/// row and the outcome together) waits for it, then answers claimed with
/// nothing written; so does the refusal writer. Kills: read-then-insert
/// (passes C2, raises a unique violation here); a violation mapped to a
/// store error, which leaves the delivery unfinished for good.
#[tokio::test]
async fn a_key_clash_under_a_lock_leaves_nothing_of_the_losing_write() {
    use scarab_policy::store::{
        PolicyStore, PolicyStoreError, RunCreationDelivery, RunCreationRow,
    };
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    let db = Arc::new(PostgresDb::with_pool(pool.clone()));
    db.migrate().await.unwrap();

    let mut holder = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO webhook_delivery_pipelines
           (connection_id, delivery_id, pipeline, forge, outcome, run_id, at)
         VALUES ('acme-conn', 'd1', '.scarab/a.yaml', 'github', 'run', 'r-first', 1)",
    )
    .execute(&mut *holder)
    .await
    .unwrap();
    let writer = {
        let db = db.clone();
        tokio::spawn(async move {
            let row = RunCreationRow {
                run_id: "r-loser".into(),
                ir_version: 1,
                event_schema_version: 1,
                created_at: 1,
                delivery: Some(RunCreationDelivery {
                    connection_id: "acme-conn".into(),
                    delivery_id: "d1".into(),
                    pipeline: ".scarab/a.yaml".into(),
                    forge: "github".into(),
                    replaces: None,
                }),
            };
            db.record_run_create_decision(&allow_record("dec-loser", "r-loser"), &row)
                .await
        })
    };
    wait_until_blocked(&pool, "%webhook_delivery_pipelines%").await;
    holder.commit().await.unwrap();
    assert!(matches!(
        writer.await.unwrap(),
        Err(PolicyStoreError::DeliveryClaimed)
    ));
    assert_eq!(db.run_status(&RunId("r-loser".into())).await.unwrap(), None);
    assert!(db.decision("dec-loser").await.unwrap().is_none());

    let mut holder = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO webhook_delivery_pipelines
           (connection_id, delivery_id, pipeline, forge, outcome, at)
         VALUES ('acme-conn', 'd1', '.scarab/b.yaml', 'github', 'no_match', 1)",
    )
    .execute(&mut *holder)
    .await
    .unwrap();
    let writer = {
        let db = db.clone();
        tokio::spawn(async move {
            db.record_delivery_refusal(
                &key("d1", ".scarab/b.yaml"),
                &refusal(".scarab/b.yaml", "b-loser", None),
            )
            .await
        })
    };
    wait_until_blocked(&pool, "%webhook_delivery_pipelines%").await;
    holder.commit().await.unwrap();
    assert!(matches!(
        writer.await.unwrap().unwrap(),
        OutcomeWrite::Claimed(Some(o)) if o.outcome == DeliveryOutcomeKind::NoMatch
    ));
    assert!(db
        .refused_runs_for_tenant("acme", "app", 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(queued_statuses(db.as_ref(), &["b-loser"]).await, 0);
    tdb.cleanup().await;
}

/// P2: two first tries of one delivery. One inserts the row and has not
/// committed; the other waits on the insert and is then held, not a second
/// first try. Kills: select-then-insert, which gives two first tries, both
/// exempt from the head rule.
#[tokio::test]
async fn two_first_tries_of_one_delivery_give_one_first_try() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    let db = Arc::new(PostgresDb::with_pool(pool.clone()));
    db.migrate().await.unwrap();

    let mut holder = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO webhook_deliveries (forge, id, at, unfinished, try_until)
         VALUES ('github', 'd1', 1, TRUE, 9223372036854775807)",
    )
    .execute(&mut *holder)
    .await
    .unwrap();
    let second = {
        let db = db.clone();
        tokio::spawn(async move {
            db.begin_delivery_try(ForgeKind::GitHub, "d1", LEASE_MS)
                .await
        })
    };
    wait_until_blocked(&pool, "%webhook_deliveries%").await;
    holder.commit().await.unwrap();
    assert_eq!(second.await.unwrap().unwrap(), DeliveryTry::Held);
    tdb.cleanup().await;
}
