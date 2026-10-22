//! Migration 0057's ADR-0071 §10 backfill against a **golden corpus** of
//! legacy per-Project environments and Run history, on real Postgres.
//!
//! The schema is walked one migration at a time (as `schema.rs` does) so the
//! corpus is seeded into the shapes that existed **before** 0057 — legacy
//! JSON exactly as older binaries wrote it, including rows from before
//! `require_reason` and `permit_k8s_overlay` existed — then 0057 runs, then
//! the backfill runs a **second** time. Pinned:
//!
//! - every migrated overlay and grant equals the pure
//!   `migrate_legacy_environment` of its legacy row (the function whose
//!   decisions the c1 proptests prove identical to the old rules);
//! - every created Environment has the permissive baseline; a pre-existing
//!   c-min Environment keeps its deadline and description;
//! - eligibility is definers ∪ run-referencers, and "all projects" for a name
//!   no Project defined (referenced-only, or c-min only);
//! - the split report lists merged and all-projects names, nothing else;
//! - the second run changes nothing.

mod common;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_project::{
    migrate_legacy_environment, EnvironmentGovernanceStore, EnvironmentProjectEligibility,
    EnvironmentProtectionRules, EnvironmentStore,
};
use sqlx::{PgPool, Row};

/// The legacy corpus: `(org, project, name, protection JSON)`, the JSON in
/// the shapes real rows hold.
fn corpus() -> Vec<(&'static str, &'static str, &'static str, serde_json::Value)> {
    use serde_json::json;
    vec![
        // Pre-ADR-0039/0055/0057: no privileged_images, permit_k8s_overlay or
        // require_reason keys at all; the pre-ADR-0071 secret scope shape.
        (
            "acme",
            "web",
            "prod",
            json!({
                "approvers": ["alice", "bob"],
                "wait_timer": 30,
                "allowed_refs": ["refs/heads/main"],
                "concurrency": 1,
                "secret_scope": { "Environment": { "org": "acme", "repo": "web", "environment": "prod" } },
                "oidc_subject": "scarab:org/acme/repo/web/env/prod"
            }),
        ),
        // A bare `main` (never matched a symbolic ref), require_reason, and a
        // privilege whitelist — merged with `web`'s `prod` above.
        (
            "acme",
            "api",
            "prod",
            json!({
                "approvers": [],
                "wait_timer": 0,
                "allowed_refs": ["main"],
                "concurrency": 3,
                "secret_scope": { "Environment": { "org": "acme", "repo": "api", "environment": "prod" } },
                "oidc_subject": "",
                "privileged_images": [
                    { "image_digest": "sha256:aaaa", "privileged": true, "capabilities": ["NET_ADMIN"] },
                    { "image_digest": "sha256:bbbb" }
                ],
                "require_reason": true
            }),
        ),
        // Empty refs, no approvers, only a raw-overlay permission.
        (
            "acme",
            "web",
            "staging",
            json!({
                "approvers": [],
                "wait_timer": 0,
                "allowed_refs": [],
                "concurrency": 1,
                "secret_scope": { "Org": { "org": "" } },
                "oidc_subject": "",
                "permit_k8s_overlay": true
            }),
        ),
        // Tags only, and an approver listed twice.
        (
            "acme",
            "api",
            "release",
            json!({
                "approvers": ["carol", "carol"],
                "wait_timer": 0,
                "allowed_refs": ["refs/tags/v*", "refs/tags/hotfix-*"],
                "concurrency": 0,
                "secret_scope": { "Org": { "org": "" } },
                "oidc_subject": "",
                "privileged_images": [],
                "permit_k8s_overlay": false,
                "require_reason": false
            }),
        ),
        // The same name in another org is another Environment.
        (
            "globex",
            "shop",
            "prod",
            json!({
                "approvers": ["dave"],
                "wait_timer": 0,
                "allowed_refs": ["refs/heads/*"],
                "concurrency": 1,
                "secret_scope": { "Org": { "org": "" } },
                "oidc_subject": ""
            }),
        ),
    ]
}

/// Run history: `(run id, org, project, environment)` deploy contexts.
const RUNS: &[(&str, &str, &str, &str)] = &[
    // Defined-and-run: adds nothing new.
    ("r-web-prod", "acme", "web", "prod"),
    // Referenced without a definition by a Project that never defined it:
    // must stay admitted after the migration → eligible for `prod`.
    ("r-mobile-prod", "acme", "mobile", "prod"),
    // A name no Project defined, only referenced → all projects.
    ("r-docs-preview", "acme", "docs", "preview"),
    ("r-docs-preview-2", "acme", "docs", "preview"),
];

async fn apply_through(pool: &PgPool, last_version: i64) {
    for m in MIGRATOR.iter().filter(|m| m.version <= last_version) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

async fn apply_0057(pool: &PgPool) {
    let m = MIGRATOR
        .iter()
        .find(|m| m.version == 57)
        .expect("migration 0057 exists");
    assert_eq!(m.description, "org environment governance");
    sqlx::raw_sql(&m.sql)
        .execute(pool)
        .await
        .expect("apply 0057");
}

async fn seed_corpus(pool: &PgPool) {
    for (org, project, name, protection) in corpus() {
        sqlx::query(
            "INSERT INTO environments (org, project, name, protection) VALUES ($1, $2, $3, $4)",
        )
        .bind(org)
        .bind(project)
        .bind(name)
        .bind(protection)
        .execute(pool)
        .await
        .unwrap();
    }
    for (id, org, project, environment) in RUNS {
        sqlx::query(
            "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at,
                               updated_at, deploy_org, deploy_project, deploy_environment,
                               deploy_git_ref)
             VALUES ($1, 'succeeded', 1, 1, 0, 0, $2, $3, $4, 'refs/heads/main')",
        )
        .bind(id)
        .bind(org)
        .bind(project)
        .bind(environment)
        .execute(pool)
        .await
        .unwrap();
    }
    // An ordinary CI run: no deploy context, so it names no Environment.
    sqlx::query(
        "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at, updated_at)
         VALUES ('r-ci', 'succeeded', 1, 1, 0, 0)",
    )
    .execute(pool)
    .await
    .unwrap();
    // c-min org Environments that predate 0057: `prod` (also defined per
    // Project) with its own deadline, and `sandbox`, defined nowhere else.
    for (name, deadline_ms, description) in [
        ("prod", 600_000_i64, "the c-min prod"),
        ("sandbox", 120_000, "a c-min sandbox"),
    ] {
        sqlx::query(
            "INSERT INTO org_environments
                 (org, name, description, deployment_deadline_ms, created_at, updated_at,
                  updated_by)
             VALUES ('acme', $1, $3, $2, 5, 5, 'amy')",
        )
        .bind(name)
        .bind(deadline_ms)
        .bind(description)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// Every row the backfill writes, as sorted text — what "the second run is a
/// no-op" compares. Timestamps included: a no-op rewrites none.
async fn governance_snapshot(pool: &PgPool) -> Vec<String> {
    let mut out = Vec::new();
    for (table, order) in [
        ("org_environments", "org, name"),
        (
            "environment_project_eligibility",
            "org, environment, project_pattern",
        ),
        ("environment_project_overlays", "org, environment, project"),
        (
            "environment_project_privilege_grants",
            "org, environment, project",
        ),
        ("environment_migration_report", "org, environment"),
    ] {
        let rows = sqlx::query(&format!(
            "SELECT row_to_json(t)::text AS r FROM {table} t ORDER BY {order}"
        ))
        .fetch_all(pool)
        .await
        .unwrap();
        out.extend(
            rows.iter()
                .map(|r| format!("{table}: {}", r.get::<String, _>("r"))),
        );
    }
    out
}

#[tokio::test]
async fn backfill_reproduces_the_pure_migration_and_is_a_no_op_the_second_time() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply_through(&pool, 56).await;
    seed_corpus(&pool).await;
    apply_0057(&pool).await;

    let first = governance_snapshot(&pool).await;
    sqlx::query("SELECT adr0071_backfill()")
        .execute(&pool)
        .await
        .expect("the backfill runs again");
    assert_eq!(
        governance_snapshot(&pool).await,
        first,
        "a second backfill changes nothing"
    );

    let db = PostgresDb::with_pool(pool.clone());

    // The legacy rows are frozen, not dropped, and still read as legacy rules.
    let legacy_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM legacy_project_environments")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(legacy_rows, corpus().len() as i64);

    // Every legacy row → exactly the pure migration of it.
    for (org, project, name, _) in corpus() {
        let legacy = db
            .get_environment(org, project, name)
            .await
            .unwrap()
            .expect("legacy row readable");
        let (want_overlay, want_grant) = migrate_legacy_environment(org, project, &legacy);
        let g = db
            .deploy_governance(org, project, name)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{org}/{name} has an org Environment"));
        assert!(g.eligible, "{org}/{project} stays eligible for {name}");
        assert_eq!(
            g.overlay,
            Some(want_overlay),
            "{org}/{project}/{name} overlay"
        );
        assert_eq!(
            g.privilege_grant, want_grant,
            "{org}/{project}/{name} grant"
        );
        assert_eq!(
            g.environment.protection,
            EnvironmentProtectionRules::default(),
            "{org}/{name} baseline is permissive"
        );
        // Concurrency is never carried (ADR-0071 §10 step 3), even where the
        // legacy row said 3.
        assert_eq!(g.effective_protection().deployment_concurrency, None);
    }

    // The two old-JSON shapes the pure function never saw as JSON: absent
    // keys read as off, and a bare `main` / duplicate approver stay verbatim.
    let api_prod = db
        .deploy_governance("acme", "api", "prod")
        .await
        .unwrap()
        .unwrap();
    let overlay = api_prod.overlay.unwrap();
    assert_eq!(overlay.rules.allowed_refs, vec!["main".to_string()]);
    assert!(overlay.rules.require_reason);
    let grant = api_prod.privilege_grant.unwrap();
    assert_eq!(grant.privileged_images.len(), 2);
    assert!(!grant.privileged_images[1].privileged);
    let web_prod = db
        .deploy_governance("acme", "web", "prod")
        .await
        .unwrap()
        .unwrap();
    assert!(!web_prod.overlay.as_ref().unwrap().rules.require_reason);
    assert_eq!(web_prod.overlay.unwrap().rules.wait_timer_s, 30);
    assert_eq!(web_prod.privilege_grant, None, "nothing to carry");
    let carol = db
        .deploy_governance("acme", "api", "release")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        carol.overlay.unwrap().rules.approvers,
        vec!["carol".to_string(), "carol".to_string()]
    );

    // A referenced-only Project stays admitted, with no overlay or grant —
    // the permissive rules it deployed under before.
    let mobile = db
        .deploy_governance("acme", "mobile", "prod")
        .await
        .unwrap()
        .unwrap();
    assert!(mobile.eligible);
    assert_eq!(mobile.overlay, None);
    assert_eq!(mobile.privilege_grant, None);

    // Eligibility.
    let eligibility = |org: &'static str, name: &'static str| {
        let db = &db;
        async move {
            db.deploy_governance(org, "nobody", name)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{org}/{name} exists"))
                .environment
                .eligibility
        }
    };
    let projects = |ps: &[&str]| {
        EnvironmentProjectEligibility::Projects(ps.iter().map(|p| p.to_string()).collect())
    };
    assert_eq!(
        eligibility("acme", "prod").await,
        projects(&["api", "mobile", "web"]),
        "definers ∪ run-referencers"
    );
    assert_eq!(eligibility("acme", "staging").await, projects(&["web"]));
    assert_eq!(eligibility("acme", "release").await, projects(&["api"]));
    assert_eq!(eligibility("globex", "prod").await, projects(&["shop"]));
    assert_eq!(
        eligibility("acme", "preview").await,
        EnvironmentProjectEligibility::AllProjects,
        "referenced-only → all projects"
    );
    assert_eq!(
        eligibility("acme", "sandbox").await,
        EnvironmentProjectEligibility::AllProjects,
        "a c-min Environment no Project defined admitted everyone before"
    );
    assert!(db
        .deploy_governance("acme", "nobody", "prod")
        .await
        .unwrap()
        .is_some_and(|g| !g.eligible));

    // Deadlines: a c-min Environment keeps its own; a created one gets the
    // one-hour default it already folded by.
    let deadline = |name: &'static str| {
        let db = &db;
        async move {
            db.deploy_governance("acme", "x", name)
                .await
                .unwrap()
                .unwrap()
                .environment
        }
    };
    let prod = deadline("prod").await;
    assert_eq!(prod.deployment_deadline.millis(), 600_000);
    assert_eq!(prod.description, "the c-min prod");
    assert_eq!(
        deadline("staging").await.deployment_deadline.millis(),
        3_600_000
    );
    assert_eq!(deadline("preview").await.updated_by, "migration:adr-0071");

    // The split report: merged names and all-projects names only.
    let report = sqlx::query(
        "SELECT org, environment, merged_projects, referenced_only, referencing_projects
         FROM environment_migration_report ORDER BY org, environment",
    )
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| {
        (
            r.get::<String, _>("org"),
            r.get::<String, _>("environment"),
            r.get::<Vec<String>, _>("merged_projects"),
            r.get::<bool, _>("referenced_only"),
            r.get::<Vec<String>, _>("referencing_projects"),
        )
    })
    .collect::<Vec<_>>();
    let s = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(
        report,
        vec![
            ("acme".into(), "preview".into(), s(&[]), true, s(&["docs"])),
            (
                "acme".into(),
                "prod".into(),
                s(&["api", "web"]),
                false,
                s(&["mobile"])
            ),
            ("acme".into(), "sandbox".into(), s(&[]), true, s(&[])),
        ]
    );

    tdb.cleanup().await;
}

/// The migrated schema is what `PostgresDb::migrate` builds too: a fresh
/// database migrates through 0057 (the backfill finding nothing) and the
/// governance writes refuse an Environment that does not exist.
#[tokio::test]
async fn a_fresh_database_migrates_and_governance_writes_need_an_environment() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.expect("migrations apply");
    assert!(db
        .deploy_governance("acme", "web", "prod")
        .await
        .unwrap()
        .is_none());
    let err = db
        .put_environment_eligibility(
            "acme",
            "prod",
            &EnvironmentProjectEligibility::AllProjects,
            "amy",
            1,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, scarab_project::OrgEnvironmentError::NotFound { .. }),
        "{err:?}"
    );
    tdb.cleanup().await;
}
