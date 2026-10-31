//! ADR-0073 §3: the conditional crown jewels. An Environment governance
//! write needs a step-up only when it **loosens** — the handler compares the
//! stored state with the request, asks when something widens and lets a
//! tightening through — and a gate approval needs one only when the run's
//! Environment has approvers (or, fail-closed, cannot be resolved). Every
//! branch is asserted both ways here, with step-up **enforced**.
//!
//! The governance writes run on a *real* Postgres store (`fresh_db`; skipped
//! without `SCARAB_TEST_DATABASE_URL`), including the optimistic check that
//! turns a change committed between the handler's read and its write into a
//! 409. The gate tests are hermetic (in-memory governance and Db), like
//! `environment_admission.rs`.

mod common;
mod step_up_harness;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::{json, Value};

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{Clock, Db, RunId, RunStatus, Scheduler, StepId, StepStatus};
use scarab_forge::RepoRef;
use scarab_identity::{ApiToken, ApiTokenStore, Role, Scope};
use scarab_project::{
    DeployGovernance, EnvironmentGovernanceStore, EnvironmentMigrationReport,
    EnvironmentProjectEligibility, EnvironmentProjectOverlay, EnvironmentProjectPrivilegeGrant,
    EnvironmentProjectRecord, EnvironmentProtectionRules, ImageGrant, OrgEnvironment,
    OrgEnvironmentError, OrgEnvironmentStore,
};
use scarab_server::{dispatch_run, DispatchKind};
use scarab_testkit::{FakeExecutor, FakeForge, InMemoryEnvironmentGovernance};
use scarab_webauthn::soft::SoftPasskey;

use step_up_harness::*;

const PROD: &str = "/v1/orgs/acme/environments/prod";
const DIGEST_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

/// An org admin signed in from a browser with a passkey enrolled, and no
/// window open: every loosening below is refused until one is.
async fn signed_in_admin(h: &Harness) -> (Sess, SoftPasskey) {
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    (s, soft)
}

#[track_caller]
fn assert_refused_for(status: StatusCode, body: &Value, op: &str, change: &str) {
    assert_step_up(status, body, "window");
    assert_eq!(body["op"], op, "{body}");
    let changes: Vec<&str> = body["summary"]["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("summary.changes: {body}"))
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert!(
        changes.iter().any(|c| c.contains(change)),
        "the human confirms what loosens — want `{change}` in {changes:?}"
    );
}

#[track_caller]
fn assert_ok(status: StatusCode, body: &Value) {
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ---------------------------------------------------------------------------
// Postgres-backed governance
// ---------------------------------------------------------------------------

struct Governed {
    h: Harness,
    pg: Arc<PostgresDb>,
    tdb: common::TestDb,
}

/// The PluginInstallations the sources name: the store refuses a source
/// naming no installation in its Org (c-min F1). Rows, not an install.
async fn install_named(pool: &sqlx::PgPool) {
    for statement in [
        "INSERT INTO plugin_installation_names (org, name, installation_id, installed_at)
         SELECT 'acme', n, substr(md5('acme/' || n), 1, 10), 0
         FROM unnest(ARRAY['argocd', 'argocd-eu']) AS n",
        "INSERT INTO plugin_installations
             (org, name, id, plugin, image, manifest, manifest_sha256, state, approved_by,
              approved_at)
         SELECT org, name, installation_id, 'argocd', 'test@sha256:0', '{}', '0', 'enabled',
                'amy', 0
         FROM plugin_installation_names",
    ] {
        sqlx::query(statement).execute(pool).await.unwrap();
    }
}

/// An enforcing harness over real Postgres governance, with `prod` in
/// `acme` (permissive baseline, every Project eligible). `governance` wraps
/// the store the governance writes go through (the race test's hook).
async fn governed_with(
    governance: impl FnOnce(Arc<PostgresDb>) -> Arc<dyn EnvironmentGovernanceStore>,
) -> Option<Governed> {
    let tdb = fresh_db().await?;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    install_named(&tdb.pool).await;
    pg.put_org_environment(&environment("prod")).await.unwrap();
    pg.put_environment_eligibility(
        "acme",
        "prod",
        &EnvironmentProjectEligibility::AllProjects,
        "seed",
        0,
    )
    .await
    .unwrap();
    let gov = governance(pg.clone());
    let pg_envs = pg.clone();
    let h = harness_with(move |st| {
        st.with_org_environments(pg_envs)
            .with_environment_governance(gov)
    });
    Some(Governed { h, pg, tdb })
}

async fn governed() -> Option<Governed> {
    governed_with(|pg| pg).await
}

fn environment(name: &str) -> OrgEnvironment {
    OrgEnvironment {
        org: "acme".into(),
        name: name.into(),
        description: String::new(),
        protection: Default::default(),
        eligibility: Default::default(),
        created_at: 0,
        updated_at: 0,
        updated_by: "seed".into(),
    }
}

fn rules(approvers: &[&str], allowed_refs: &[&str]) -> EnvironmentProtectionRules {
    EnvironmentProtectionRules {
        approvers: strings(approvers),
        allowed_refs: strings(allowed_refs),
        ..Default::default()
    }
}

async fn seed_protection(pg: &PostgresDb, r: EnvironmentProtectionRules) {
    pg.put_environment_protection("acme", "prod", &r, "seed", 0)
        .await
        .unwrap();
}

async fn stored_protection(pg: &PostgresDb) -> EnvironmentProtectionRules {
    pg.get_org_environment("acme", "prod")
        .await
        .unwrap()
        .unwrap()
        .protection
}

/// E1 on the baseline: dropping an approver, widening the refs (to a glob
/// the old list does not cover, or to "any ref") needs a step-up; adding an
/// approver or narrowing the refs does not.
#[tokio::test]
async fn protection_loosening_needs_a_step_up_and_tightening_does_not() {
    let Some(g) = governed().await else { return };
    let (h, pg) = (&g.h, &g.pg);
    let (s, mut soft) = signed_in_admin(h).await;
    let uri = format!("{PROD}/protection");
    let put = |body: Value| h.cookie("PUT", &uri, Some(body), &s);

    // Remove an approver → 403, and nothing was written.
    seed_protection(pg, rules(&["amy", "bob"], &[])).await;
    let (st, b) = put(json!({ "approvers": ["amy"] })).await;
    assert_refused_for(
        st,
        &b,
        "environment.protection.put",
        "removes approver `bob`",
    );
    assert_eq!(
        stored_protection(pg).await.approvers,
        strings(&["amy", "bob"])
    );

    // Add one → 200 with no confirmation.
    let (st, b) = put(json!({ "approvers": ["amy", "bob", "carol"] })).await;
    assert_ok(st, &b);
    assert_eq!(
        stored_protection(pg).await.approvers,
        strings(&["amy", "bob", "carol"])
    );

    // `main` → `refs/heads/*` widens → 403.
    seed_protection(pg, rules(&[], &["main"])).await;
    let (st, b) = put(json!({ "allowed_refs": ["refs/heads/*"] })).await;
    assert_refused_for(st, &b, "environment.protection.put", "widens allowed refs");

    // Clearing a restricted list allows any ref → 403.
    let (st, b) = put(json!({ "allowed_refs": [] })).await;
    assert_refused_for(st, &b, "environment.protection.put", "allows any ref");
    assert_eq!(stored_protection(pg).await.allowed_refs, strings(&["main"]));

    // `*` → `main` narrows → 200.
    seed_protection(pg, rules(&[], &["*"])).await;
    let (st, b) = put(json!({ "allowed_refs": ["main"] })).await;
    assert_ok(st, &b);
    assert_eq!(stored_protection(pg).await.allowed_refs, strings(&["main"]));

    // Writing exactly what is stored loosens nothing: exempt, 200 (and the
    // guard's "conditional handler never decided" tripwire stays quiet).
    let (st, b) = put(json!({ "allowed_refs": ["main"] })).await;
    assert_ok(st, &b);

    // With a window open, the loosening goes through.
    seed_protection(pg, rules(&["amy", "bob"], &["main"])).await;
    h.open_window(&s, &mut soft).await;
    let (st, b) = put(json!({ "approvers": ["amy"] })).await;
    assert_ok(st, &b);
    assert_eq!(stored_protection(pg).await.approvers, strings(&["amy"]));
    assert!(stored_protection(pg).await.allowed_refs.is_empty());
    // The operation's authorisation names the loosening it confirmed.
    let authorised = h.events_of("op.authorised");
    let last = authorised.last().expect("op.authorised recorded");
    assert_eq!(last.op.as_deref(), Some("environment.protection.put"));
    assert!(
        last.detail.to_string().contains("removes approver `bob`"),
        "{}",
        last.detail
    );

    g.tdb.cleanup().await;
}

/// E2: from "every project" to a list tightens; to "every project", or a
/// pattern the old list does not cover, loosens.
#[tokio::test]
async fn eligibility_widening_needs_a_step_up() {
    let Some(g) = governed().await else { return };
    let (h, pg) = (&g.h, &g.pg);
    let (s, mut soft) = signed_in_admin(h).await;
    let uri = format!("{PROD}/eligibility");
    let put = |body: Value| h.cookie("PUT", &uri, Some(body), &s);
    let stored = || async move {
        pg.get_org_environment("acme", "prod")
            .await
            .unwrap()
            .unwrap()
            .eligibility
    };

    // All → list: 200.
    let (st, b) = put(json!({ "all_projects": false, "projects": ["web"] })).await;
    assert_ok(st, &b);

    // List → All: 403.
    let (st, b) = put(json!({ "all_projects": true })).await;
    assert_refused_for(
        st,
        &b,
        "environment.eligibility.put",
        "admits every project",
    );

    // A pattern added: 403.
    let (st, b) = put(json!({ "all_projects": false, "projects": ["web", "api-*"] })).await;
    assert_refused_for(
        st,
        &b,
        "environment.eligibility.put",
        "admits projects matching `api-*`",
    );
    assert_eq!(
        stored().await,
        EnvironmentProjectEligibility::Projects(strings(&["web"]))
    );

    // A pattern removed: 200.
    let (st, b) = put(json!({ "all_projects": false, "projects": [] })).await;
    assert_ok(st, &b);

    // With a window open, widening goes through.
    h.open_window(&s, &mut soft).await;
    let (st, b) = put(json!({ "all_projects": true })).await;
    assert_ok(st, &b);
    assert_eq!(stored().await, EnvironmentProjectEligibility::AllProjects);

    g.tdb.cleanup().await;
}

/// The whole-Environment PUT writes a name and a description: no longer
/// governed (its E3 rule went with deployment sources, ADR-0074), and a body
/// naming a field ADR-0074 removed is a 400, not a silent no-op.
#[tokio::test]
async fn an_environment_put_needs_no_step_up_and_refuses_removed_fields() {
    let Some(g) = governed().await else { return };
    let (h, pg) = (&g.h, &g.pg);
    let (s, _soft) = signed_in_admin(h).await;
    let (st, b) = h
        .cookie(
            "PUT",
            PROD,
            Some(json!({ "description": "production" })),
            &s,
        )
        .await;
    assert_ok(st, &b);
    assert_eq!(
        pg.get_org_environment("acme", "prod")
            .await
            .unwrap()
            .unwrap()
            .description,
        "production"
    );
    for removed in [
        json!({ "deployment_sources": [] }),
        json!({ "deployment_deadline_ms": 60_000 }),
    ] {
        let (st, b) = h.cookie("PUT", PROD, Some(removed.clone()), &s).await;
        assert!(st.is_client_error(), "{removed}: {st} {b}");
    }
    g.tdb.cleanup().await;
}

async fn seed_grant(pg: &PostgresDb, images: Vec<ImageGrant>) {
    pg.put_environment_project_privilege_grant(
        &EnvironmentProjectPrivilegeGrant {
            org: "acme".into(),
            environment: "prod".into(),
            project: "web".into(),
            privileged_images: images,
            permit_k8s_overlay: false,
        },
        "seed",
        0,
    )
    .await
    .unwrap();
}

fn image(digest: &str, privileged: bool) -> ImageGrant {
    ImageGrant {
        image_digest: digest.into(),
        privileged,
        capabilities: Vec::new(),
    }
}

/// E4: a new digest, `privileged` switched on, or the raw overlay permitted
/// widens a Project's privileges; dropping a digest narrows them.
#[tokio::test]
async fn privilege_grant_widening_needs_a_step_up() {
    let Some(g) = governed().await else { return };
    let (h, pg) = (&g.h, &g.pg);
    let (s, mut soft) = signed_in_admin(h).await;
    let uri = format!("{PROD}/projects/web/privilege-grant");
    let put = |body: Value| h.cookie("PUT", &uri, Some(body), &s);
    let entry = |digest: &str, privileged: bool| json!({ "image_digest": digest, "privileged": privileged });

    // A first grant with nothing in it widens nothing: 200.
    let (st, b) = put(json!({ "privileged_images": [] })).await;
    assert_ok(st, &b);

    // A new digest → 403.
    let (st, b) = put(json!({ "privileged_images": [entry(DIGEST_A, false)] })).await;
    assert_refused_for(
        st,
        &b,
        "environment.privilege_grant.put",
        &format!("grants image `{DIGEST_A}`"),
    );

    // Drop a digest → 200.
    seed_grant(pg, vec![image(DIGEST_A, false), image(DIGEST_B, false)]).await;
    let (st, b) = put(json!({ "privileged_images": [entry(DIGEST_A, false)] })).await;
    assert_ok(st, &b);

    // privileged false → true → 403.
    let (st, b) = put(json!({ "privileged_images": [entry(DIGEST_A, true)] })).await;
    assert_refused_for(st, &b, "environment.privilege_grant.put", "run privileged");
    // The raw overlay permitted → 403.
    let (st, b) = put(json!({
        "privileged_images": [entry(DIGEST_A, false)],
        "permit_k8s_overlay": true
    }))
    .await;
    assert_refused_for(st, &b, "environment.privilege_grant.put", "k8s_overlay");
    let stored = pg
        .deploy_governance("acme", "web", "prod")
        .await
        .unwrap()
        .unwrap()
        .privilege_grant
        .unwrap();
    assert_eq!(stored.privileged_images, vec![image(DIGEST_A, false)]);
    assert!(!stored.permit_k8s_overlay);

    // With a window open, the widening goes through.
    h.open_window(&s, &mut soft).await;
    let (st, b) = put(json!({ "privileged_images": [entry(DIGEST_A, true)] })).await;
    assert_ok(st, &b);

    g.tdb.cleanup().await;
}

/// A Project's overlay: deleting it falls back to the (looser) baseline and
/// is always a crown jewel; removing an overlay approver or its gate
/// deadline loosens; adding an approver tightens.
#[tokio::test]
async fn overlay_loosening_and_delete_need_a_step_up() {
    let Some(g) = governed().await else { return };
    let (h, pg) = (&g.h, &g.pg);
    let (s, mut soft) = signed_in_admin(h).await;
    let uri = format!("{PROD}/projects/web/overlay");
    pg.put_environment_project_overlay(
        &EnvironmentProjectOverlay {
            org: "acme".into(),
            environment: "prod".into(),
            project: "web".into(),
            rules: rules(&["amy", "bob"], &[]),
        },
        "seed",
        0,
    )
    .await
    .unwrap();
    let overlay = || async move {
        pg.deploy_governance("acme", "web", "prod")
            .await
            .unwrap()
            .unwrap()
            .overlay
    };

    // Delete → 403.
    let (st, b) = h.cookie("DELETE", &uri, None, &s).await;
    assert_step_up(st, &b, "window");
    assert_eq!(b["op"], "environment.overlay.delete");

    // Remove an overlay approver → 403.
    let (st, b) = h
        .cookie("PUT", &uri, Some(json!({ "approvers": ["amy"] })), &s)
        .await;
    assert_refused_for(st, &b, "environment.overlay.put", "removes approver `bob`");
    assert!(overlay()
        .await
        .is_some_and(|o| o.rules.approvers.len() == 2));

    // Add an approver → 200.
    let (st, b) = h
        .cookie(
            "PUT",
            &uri,
            Some(json!({ "approvers": ["amy", "bob", "carol"] })),
            &s,
        )
        .await;
    assert_ok(st, &b);
    assert_eq!(overlay().await.unwrap().rules.approvers.len(), 3);

    // With a window open, the loosening and the delete go through.
    h.open_window(&s, &mut soft).await;
    let (st, b) = h
        .cookie("PUT", &uri, Some(json!({ "approvers": ["amy"] })), &s)
        .await;
    assert_ok(st, &b);
    let (st, b) = h.cookie("DELETE", &uri, None, &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");
    assert!(overlay().await.is_none());

    g.tdb.cleanup().await;
}

// ---------------------------------------------------------------------------
// The race: a change committed between the handler's read and its write
// ---------------------------------------------------------------------------

/// Postgres governance whose reads — what a conditional write judges its
/// loosenings against — let another admin commit a change right after
/// them, once, when armed. Constructs the interleaving rather than
/// scheduling it.
///
/// With `lensed`, `deploy_governance` also answers `None` for a Project the
/// Environment does not make eligible — a store that filters its
/// per-Project read by eligibility (git-bug 0485f41).
struct RacingGovernance {
    inner: Arc<PostgresDb>,
    armed: AtomicBool,
    concurrent: EnvironmentProtectionRules,
    lensed: bool,
}

impl RacingGovernance {
    async fn race(&self, org: &str, environment: &str) -> Result<(), OrgEnvironmentError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.inner
                .put_environment_protection(org, environment, &self.concurrent, "carol", 1)
                .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl EnvironmentGovernanceStore for RacingGovernance {
    async fn org_environment(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Option<OrgEnvironment>, OrgEnvironmentError> {
        let read = self.inner.org_environment(org, environment).await?;
        self.race(org, environment).await?;
        Ok(read)
    }
    async fn deploy_governance(
        &self,
        org: &str,
        project: &str,
        environment: &str,
    ) -> Result<Option<DeployGovernance>, OrgEnvironmentError> {
        let read = self
            .inner
            .deploy_governance(org, project, environment)
            .await?
            .filter(|g| !self.lensed || g.eligible);
        self.race(org, environment).await?;
        Ok(read)
    }
    async fn project_environment_governance(
        &self,
        org: &str,
        project: &str,
    ) -> Result<Vec<DeployGovernance>, OrgEnvironmentError> {
        self.inner
            .project_environment_governance(org, project)
            .await
    }
    async fn put_environment_protection_expecting(
        &self,
        org: &str,
        environment: &str,
        protection: &EnvironmentProtectionRules,
        expected: Option<&EnvironmentProtectionRules>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        self.inner
            .put_environment_protection_expecting(
                org,
                environment,
                protection,
                expected,
                updated_by,
                updated_at,
            )
            .await
    }
    async fn put_environment_eligibility_expecting(
        &self,
        org: &str,
        environment: &str,
        eligibility: &EnvironmentProjectEligibility,
        expected: Option<&EnvironmentProjectEligibility>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        self.inner
            .put_environment_eligibility_expecting(
                org,
                environment,
                eligibility,
                expected,
                updated_by,
                updated_at,
            )
            .await
    }
    async fn put_environment_project_overlay_expecting(
        &self,
        overlay: &EnvironmentProjectOverlay,
        expected: Option<Option<&EnvironmentProjectOverlay>>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<(), OrgEnvironmentError> {
        self.inner
            .put_environment_project_overlay_expecting(overlay, expected, updated_by, updated_at)
            .await
    }
    async fn delete_environment_project_overlay(
        &self,
        org: &str,
        environment: &str,
        project: &str,
    ) -> Result<bool, OrgEnvironmentError> {
        self.inner
            .delete_environment_project_overlay(org, environment, project)
            .await
    }
    async fn put_environment_project_privilege_grant_expecting(
        &self,
        grant: &EnvironmentProjectPrivilegeGrant,
        expected: Option<Option<&EnvironmentProjectPrivilegeGrant>>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<(), OrgEnvironmentError> {
        self.inner
            .put_environment_project_privilege_grant_expecting(
                grant, expected, updated_by, updated_at,
            )
            .await
    }
    async fn delete_environment_project_privilege_grant(
        &self,
        org: &str,
        environment: &str,
        project: &str,
    ) -> Result<bool, OrgEnvironmentError> {
        self.inner
            .delete_environment_project_privilege_grant(org, environment, project)
            .await
    }
    async fn environment_projects(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Vec<EnvironmentProjectRecord>, OrgEnvironmentError> {
        self.inner.environment_projects(org, environment).await
    }
    async fn environment_migration_report(
        &self,
        org: &str,
    ) -> Result<Vec<EnvironmentMigrationReport>, OrgEnvironmentError> {
        self.inner.environment_migration_report(org).await
    }
    async fn acknowledge_environment_migration_report(
        &self,
        org: &str,
        environment: &str,
        acknowledged_by: &str,
        acknowledged_at: i64,
    ) -> Result<Option<EnvironmentMigrationReport>, OrgEnvironmentError> {
        self.inner
            .acknowledge_environment_migration_report(
                org,
                environment,
                acknowledged_by,
                acknowledged_at,
            )
            .await
    }
}

/// The write was judged a tightening against what the handler read, but
/// another admin added an approver before it landed: writing it now would
/// drop that approver with no step-up. The store refuses it (409) and keeps
/// the concurrent change.
#[tokio::test]
async fn a_change_committed_between_read_and_write_is_a_409() {
    let racing: Arc<std::sync::OnceLock<Arc<RacingGovernance>>> = Default::default();
    let hook = racing.clone();
    let Some(g) = governed_with(move |pg| {
        let r = Arc::new(RacingGovernance {
            inner: pg,
            armed: AtomicBool::new(false),
            concurrent: rules(&["amy", "bob"], &[]),
            lensed: false,
        });
        hook.set(r.clone()).ok().unwrap();
        r
    })
    .await
    else {
        return;
    };
    let (h, pg) = (&g.h, &g.pg);
    let (s, _soft) = signed_in_admin(h).await;
    seed_protection(pg, rules(&["amy"], &[])).await;
    racing.get().unwrap().armed.store(true, Ordering::SeqCst);

    // Against the read ([amy]) this only adds carol: a tightening, exempt.
    let (st, b) = h
        .cookie(
            "PUT",
            &format!("{PROD}/protection"),
            Some(json!({ "approvers": ["amy", "carol"] })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert!(
        b.to_string().contains("changed concurrently"),
        "the 409 says to reload: {b}"
    );
    assert!(
        !racing.get().unwrap().armed.load(Ordering::SeqCst),
        "the interleaving happened"
    );
    assert_eq!(
        stored_protection(pg).await.approvers,
        strings(&["amy", "bob"]),
        "the concurrent change stands; bob was not silently dropped"
    );

    g.tdb.cleanup().await;
}

/// git-bug 0485f41: the read a governance write judges against is the
/// Environment itself, not some Project's view of it. A store whose
/// per-Project read filters by eligibility — a legal refinement — must not
/// turn every protection and eligibility write on a list-eligible
/// Environment into a 404.
#[tokio::test]
async fn governance_writes_read_the_environment_without_a_project_lens() {
    let Some(g) = governed_with(|pg| {
        Arc::new(RacingGovernance {
            inner: pg,
            armed: AtomicBool::new(false),
            concurrent: rules(&[], &[]),
            lensed: true,
        })
    })
    .await
    else {
        return;
    };
    let (h, pg) = (&g.h, &g.pg);
    let (s, _soft) = signed_in_admin(h).await;
    pg.put_environment_eligibility(
        "acme",
        "prod",
        &EnvironmentProjectEligibility::Projects(strings(&["web"])),
        "seed",
        0,
    )
    .await
    .unwrap();

    // Both tightenings: exempt, so the only way to fail is the read.
    let (st, b) = h
        .cookie(
            "PUT",
            &format!("{PROD}/protection"),
            Some(json!({ "approvers": ["amy"] })),
            &s,
        )
        .await;
    assert_ok(st, &b);
    let (st, b) = h
        .cookie(
            "PUT",
            &format!("{PROD}/eligibility"),
            Some(json!({ "all_projects": false, "projects": [] })),
            &s,
        )
        .await;
    assert_ok(st, &b);
    assert_eq!(stored_protection(pg).await.approvers, strings(&["amy"]));

    g.tdb.cleanup().await;
}

// ---------------------------------------------------------------------------
// Gate approval (hermetic)
// ---------------------------------------------------------------------------

/// A deploy into `prod` with an approval gate, and a plain pipeline with
/// one.
const DEPLOY_YAML: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], needs: [approve] }
"#;

const PLAIN_YAML: &str = r#"
on:
  manual: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], needs: [approve] }
"#;

fn forge() -> FakeForge {
    FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_file(".scarab/plain.yaml", PLAIN_YAML)
        .with_commit(
            "refs/heads/main",
            "1234567890abcdef1234567890abcdef12345678",
        )
}

fn prod_governance() -> Arc<InMemoryEnvironmentGovernance> {
    Arc::new(InMemoryEnvironmentGovernance::new().with_environment(
        "acme",
        "prod",
        EnvironmentProjectEligibility::AllProjects,
    ))
}

fn gated(governance: Arc<InMemoryEnvironmentGovernance>) -> Harness {
    harness_with(move |st| st.with_environment_governance(governance))
}

/// Dispatch `pipeline` and drive it to its approval gate.
async fn at_gate(h: &Harness, governance: &InMemoryEnvironmentGovernance, pipeline: &str) -> RunId {
    let run = dispatch_run(
        &forge(),
        h.db.as_ref(),
        h.clock.as_ref(),
        scarab_server::AdmissionPorts::governance(Some(governance)),
        "bob".into(),
        RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        "refs/heads/main".into(),
        pipeline.into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect("admitted");
    let exec = FakeExecutor::new();
    for _ in 0..4 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(
        h.db.as_ref(),
        h.clock.as_ref() as &dyn Clock,
        &exec,
        "sched",
    );
    for _ in 0..6 {
        sched.tick(&run).await.unwrap();
        if h.db.run_status(&run).await.unwrap() == Some(RunStatus::Suspended) {
            break;
        }
    }
    assert_eq!(
        h.db.run_status(&run).await.unwrap(),
        Some(RunStatus::Suspended)
    );
    run
}

fn approve_uri(run: &RunId) -> String {
    format!("/v1/runs/{}/gates/approve/approve", run.0)
}

async fn approvals(h: &Harness, run: &RunId) -> Vec<String> {
    scarab_engine::gate_approvers(&h.db.events(run).await.unwrap(), &StepId("approve".into()))
}

async fn gate_status(h: &Harness, run: &RunId) -> StepStatus {
    h.db.steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step == StepId("approve".into()))
        .unwrap()
        .status
}

async fn require_approvers(governance: &InMemoryEnvironmentGovernance, approvers: &[&str]) {
    governance
        .put_environment_protection("acme", "prod", &rules(approvers, &[]), "seed", 0)
        .await
        .unwrap();
}

/// An approval on a gate of an Environment with approvers is a crown jewel:
/// refused — and NOT recorded — until a window is open; then it releases.
#[tokio::test]
async fn approving_a_gate_of_an_environment_with_approvers_needs_a_step_up() {
    let governance = prod_governance();
    let h = gated(governance.clone());
    let (s, mut soft) = signed_in_admin(&h).await;
    let run = at_gate(&h, &governance, "deploy").await;
    require_approvers(&governance, &["amy"]).await;

    let (st, b) = h.cookie("POST", &approve_uri(&run), None, &s).await;
    assert_step_up(st, &b, "window");
    assert_eq!(b["op"], "gate.approve");
    assert!(
        approvals(&h, &run).await.is_empty(),
        "a refused approval is never recorded"
    );

    h.open_window(&s, &mut soft).await;
    let (st, b) = h.cookie("POST", &approve_uri(&run), None, &s).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["released"], true, "{b}");
    assert_eq!(approvals(&h, &run).await, strings(&["amy"]));
}

/// An Environment with no approvers, or a plain gate: no step-up, even for
/// a caller with no passkey at all.
#[tokio::test]
async fn approving_a_gate_without_approvers_or_environment_is_exempt() {
    let governance = prod_governance();
    let h = gated(governance.clone());
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;

    let deploy = at_gate(&h, &governance, "deploy").await;
    let (st, b) = h.cookie("POST", &approve_uri(&deploy), None, &s).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{b}");
    assert_eq!(gate_status(&h, &deploy).await, StepStatus::Succeeded);

    let plain = at_gate(&h, &governance, "plain").await;
    let (st, b) = h.cookie("POST", &approve_uri(&plain), None, &s).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{b}");
    assert_eq!(gate_status(&h, &plain).await, StepStatus::Succeeded);

    assert!(h.events_of("step_up.refused").is_empty());
}

/// A deploy whose Environment is gone cannot be judged: fail-closed, a
/// step-up — the approval would count if the Environment came back. With
/// one, the approval is recorded and the gate holds (409), as before.
#[tokio::test]
async fn approving_a_gate_of_a_deleted_environment_needs_a_step_up() {
    let governance = prod_governance();
    let h = gated(governance.clone());
    let (s, mut soft) = signed_in_admin(&h).await;
    let run = at_gate(&h, &governance, "deploy").await;
    governance.remove_environment("acme", "prod");

    let (st, b) = h.cookie("POST", &approve_uri(&run), None, &s).await;
    assert_step_up(st, &b, "window");
    assert!(approvals(&h, &run).await.is_empty());

    h.open_window(&s, &mut soft).await;
    let (st, b) = h.cookie("POST", &approve_uri(&run), None, &s).await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert_eq!(approvals(&h, &run).await, strings(&["amy"]));
    assert_ne!(gate_status(&h, &run).await, StepStatus::Succeeded);
}

/// The tightening operations never ask: reject and cancel (on a gate whose
/// Environment has approvers), plugin uninstall and kill, token revoke.
#[tokio::test]
async fn exempt_operations_never_need_a_step_up() {
    let governance = prod_governance();
    let h = gated(governance.clone());
    let (s, _soft) = signed_in_admin(&h).await;
    let rejected = at_gate(&h, &governance, "deploy").await;
    let cancelled = at_gate(&h, &governance, "deploy").await;
    require_approvers(&governance, &["amy"]).await;

    let (st, b) = h
        .cookie(
            "POST",
            &format!("/v1/runs/{}/gates/approve/reject", rejected.0),
            Some(json!({ "reason": "not today" })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{b}");

    let (st, b) = h
        .cookie(
            "POST",
            &format!("/v1/runs/{}/cancel", cancelled.0),
            None,
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{b}");

    let now = h.now().await;
    h.tokens
        .put(
            &ApiToken {
                id: "tok-1".into(),
                name: "ci".into(),
                owner_subject: "amy".into(),
                scope: Scope::Org("acme".into()),
                role: Role::Member,
                expires_at: now + 86_400_000,
                created_by: "amy".into(),
                created_at: now,
                last_used_at: None,
                revoked_at: None,
            },
            "hash-1",
        )
        .await
        .unwrap();
    let (st, b) = h
        .cookie("DELETE", "/v1/orgs/acme/tokens/tok-1", None, &s)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");

    for (method, uri) in [
        ("DELETE", "/v1/orgs/acme/plugins/argocd"),
        ("POST", "/v1/orgs/acme/plugins/argocd/kill"),
    ] {
        let (st, b) = h.cookie(method, uri, None, &s).await;
        assert!(!is_step_up(st, &b), "{method} {uri}: {st} {b}");
    }

    assert!(h.events_of("step_up.refused").is_empty());
}

// ---------------------------------------------------------------------------
// Plugin storage (ADR-0074 §7)
// ---------------------------------------------------------------------------

fn plugin_installation() -> scarab_plugin::PluginInstallation {
    scarab_plugin::PluginInstallation {
        id: "0000000001".into(),
        org: "acme".into(),
        name: "argocd".into(),
        plugin: "argocd".into(),
        image: format!("ghcr.io/acme/argocd@sha256:{}", "a".repeat(64)),
        manifest: scarab_plugin::PluginManifest {
            plugin: "argocd".into(),
            capabilities: vec![scarab_plugin::PluginCapability::UseConnection],
            connections: vec![scarab_plugin::ConnectionSlot {
                name: "argocd".into(),
                auth_schemes: vec!["bearer".into()],
                read_rules: vec![scarab_plugin::RequestRule {
                    method: "GET".into(),
                    path: "/api/version".into(),
                    query: vec![],
                }],
            }],
            config_keys: vec![],
            gates: Default::default(),
        },
        manifest_sha256: "0".repeat(64),
        config: Default::default(),
        state: scarab_plugin::InstallationState::Enabled,
        runtime_status: scarab_plugin::RuntimeStatus::Pending,
        runtime_message: String::new(),
        probe_passed_at: None,
        approved_by: "amy".into(),
        approved_at: 0,
        killed_by: None,
        killed_at: None,
        storage: None,
    }
}

/// Adding storage to an installation that has none, and removing it, need
/// no step-up; moving it to another claim or mount path does (window), and
/// the human confirms the move. A refused move stores nothing.
#[tokio::test]
async fn moving_plugin_storage_needs_a_step_up_and_adding_or_removing_does_not() {
    let Some(tdb) = fresh_db().await else { return };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    use scarab_plugin::PluginStore;
    pg.install(&plugin_installation()).await.unwrap();
    let plugins = pg.clone();
    let h = harness_with(move |st| {
        st.with_plugins(
            plugins,
            scarab_server::plugins::PluginPolicy {
                enabled: true,
                namespace: "scarab-plugins".into(),
                argocd_repo: "ghcr.io/acme/argocd".into(),
                dev_allow_tag: false,
            },
        )
    });
    let (s, mut soft) = signed_in_admin(&h).await;
    let uri = "/v1/orgs/acme/plugins/argocd/storage";
    let stored = || async {
        pg.installation("acme", "argocd")
            .await
            .unwrap()
            .unwrap()
            .storage
            .map(|s| (s.claim, s.mount_path))
    };
    let claim = |c: &str| (c.to_string(), "/data".to_string());

    // Added: 200, and the same body again is unchanged: 200.
    for _ in 0..2 {
        let (st, b) = h
            .cookie("PUT", uri, Some(json!({ "claim": "state-a" })), &s)
            .await;
        assert_ok(st, &b);
    }
    assert_eq!(stored().await, Some(claim("state-a")));

    // Moved to another claim, or another path: 403.
    let (st, b) = h
        .cookie("PUT", uri, Some(json!({ "claim": "state-b" })), &s)
        .await;
    assert_refused_for(
        st,
        &b,
        "plugin.storage.put",
        "storage moves from claim `state-a` at `/data` to claim `state-b` at `/data`",
    );
    let (st, b) = h
        .cookie(
            "PUT",
            uri,
            Some(json!({ "claim": "state-a", "mount_path": "/srv" })),
            &s,
        )
        .await;
    assert_refused_for(st, &b, "plugin.storage.put", "to claim `state-a` at `/srv`");
    assert_eq!(stored().await, Some(claim("state-a")));

    // Removed: 200 (exempt), and re-adding is exempt again.
    let (st, b) = h.cookie("DELETE", uri, None, &s).await;
    assert_ok(st, &b);
    assert_eq!(stored().await, None);
    let (st, b) = h
        .cookie("PUT", uri, Some(json!({ "claim": "state-b" })), &s)
        .await;
    assert_ok(st, &b);

    // With a window open, the move goes through.
    h.open_window(&s, &mut soft).await;
    let (st, b) = h
        .cookie("PUT", uri, Some(json!({ "claim": "state-a" })), &s)
        .await;
    assert_ok(st, &b);
    assert_eq!(stored().await, Some(claim("state-a")));
    tdb.cleanup().await;
}
