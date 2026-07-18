//! Slice-5 ACCEPTANCE (ADR-0017, 0014, 0015, 0018): secrets + OIDC + BuildKit
//! proven together. A step consumes an envelope-encrypted secret from *real*
//! Postgres and it never reaches the logs; a run-scoped OIDC token verifies
//! against the JWKS; and a build step compiles to a rootless-BuildKit Pod whose
//! image digest is recorded as an artifact (the live build is #[ignore]-gated in
//! the executor's cluster test). Skips cleanly without SCARAB_TEST_DATABASE_URL.

mod common;

use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{Db, RunId, StepId, StepRun};
use scarab_executor_k8s::{build_pod, image_artifact, DEFAULT_CLONE_IMAGE, DEFAULT_STEP_TIMEOUT_SECS};
use scarab_identity::{Claims, OidcIssuer};
use scarab_secrets::{Secret, SecretProvider, SecretScope};
use scarab_secrets_postgres::PostgresSecrets;
use scarab_server::oidc::{verify, Rs256Issuer};
use scarab_server::{resolve_step_secrets, LogService};
use scarab_testkit::InMemoryObjectStore;

const ISSUER: &str = "https://scarab.example";
const AUD: &str = "sts.amazonaws.com";
const EXP_2100: i64 = 4_102_444_800;

fn scope() -> SecretScope {
    SecretScope::Repo {
        org: "acme".into(),
        repo: "app".into(),
    }
}

#[tokio::test]
async fn secret_used_not_logged_oidc_verifies_and_build_produces_digest() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();

    // --- 1. A step consumes an envelope-encrypted secret; it never hits logs. ---
    let secrets = PostgresSecrets::with_master(tdb.pool.clone(), [9u8; 32]);
    secrets.migrate().await.unwrap();
    secrets
        .put(
            &scope(),
            Secret {
                key: "DEPLOY_TOKEN".into(),
                value: b"prod-token-xyz".to_vec(),
            },
        )
        .await
        .unwrap();

    let db: Arc<dyn Db> = pg.clone();
    let logs = LogService::new(Arc::new(InMemoryObjectStore::new()), db);

    let env = resolve_step_secrets(&secrets, &logs, &scope(), &["DEPLOY_TOKEN".to_string()], false)
        .await
        .unwrap();
    assert_eq!(env, vec![("DEPLOY_TOKEN".to_string(), "prod-token-xyz".to_string())]);

    let (run, step, attempt) = (
        RunId("run-1".into()),
        StepId("deploy".into()),
        scarab_engine::AttemptId("a1".into()),
    );
    logs.append(&run, &step, &attempt, b"deploying with DEPLOY_TOKEN=prod-token-xyz\n")
        .await
        .unwrap();
    let stored = logs.read_all(&run, &step, &attempt).await.unwrap();
    let stored = String::from_utf8_lossy(&stored);
    assert!(!stored.contains("prod-token-xyz"), "secret leaked into logs: {stored}");
    assert!(stored.contains("DEPLOY_TOKEN=***"));

    // --- 2. A run-scoped OIDC token verifies against the JWKS. ---
    let issuer = Rs256Issuer::generate(ISSUER).unwrap();
    let claims = Claims {
        issuer: ISSUER.into(),
        subject: Claims::run_subject("acme", "app", "prod", "refs/heads/main"),
        audience: AUD.into(),
        run_id: "run-1".into(),
        attempt: "a1".into(),
        event: "push".into(),
        git_ref: "refs/heads/main".into(),
        sha: "cafebabe".into(),
        expires_at: EXP_2100,
    };
    let token = issuer.issue(claims).await.unwrap();
    let jwks = issuer.jwks();
    let (n, e) = (
        jwks["keys"][0]["n"].as_str().unwrap(),
        jwks["keys"][0]["e"].as_str().unwrap(),
    );
    let verified = verify(&token.0, n, e, AUD).expect("run token verifies against JWKS");
    assert_eq!(verified["sub"], "scarab:org/acme/repo/app/env/prod/ref/refs/heads/main");
    assert_eq!(verified["run_id"], "run-1");

    // --- 3. A build step compiles to a rootless-BuildKit Pod + records a digest. ---
    let build = scarab_engine::BuildConfig {
        context: "workspace".into(),
        dockerfile: "Dockerfile".into(),
        image: "registry.example/app:1.0".into(),
        push: true,
        ..Default::default()
    };
    let build_step = StepRun::new(run.clone(), StepId("image".into()));
    let build_spec = scarab_engine::StepSpec {
        image: String::new(),
        command: vec![],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        clone: None,
        build: Some(build.clone()),
        artifacts: vec![],
        placement_profiles: vec![], resources: Default::default(), k8s_overlay: None, oidc_token: None,
    };
    let pod = build_pod(
        "scarab-image",
        "scarab-run-1",
        &build_step,
        &build_spec,
        None,
        DEFAULT_STEP_TIMEOUT_SECS,
        false,
        DEFAULT_CLONE_IMAGE,
    );
    let container = &pod.spec.as_ref().unwrap().containers[0];
    assert_eq!(container.image.as_deref(), Some("moby/buildkit:rootless"));
    assert_eq!(container.security_context.as_ref().unwrap().privileged, Some(false));

    let artifact = image_artifact(&build, "sha256:deadbeef");
    assert_eq!(artifact.image, "registry.example/app:1.0");
    assert_eq!(artifact.digest, "sha256:deadbeef");

    tdb.cleanup().await;
}
