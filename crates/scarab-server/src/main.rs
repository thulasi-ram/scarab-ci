//! # scarab-server — the converged API/worker process binary.
//!
//! One binary, selectable roles (ADR-0016). `converged` runs the API plus the
//! scheduler + executor background loop in a single process — ideal for dev and
//! small installs; the same code scales out by running roles as separate
//! replicas, since Postgres (the outbox) is the coordination bus. This binary is
//! a thin composition root over the [`scarab_server`] library.
//!
//! Boot is fail-closed (ADR-0048): every `SCARAB_*` knob is read and validated
//! by [`scarab_server::config`] before anything connects or binds, and a
//! startup report makes the operational posture legible on line one.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use scarab_db_postgres::PostgresDb;
use scarab_engine::{Clock, Db, Executor};
use scarab_executor_k8s::{K8sExecutor, ResultsEgress};
use scarab_executor_local::LocalExecutor;
use scarab_server::config::{Cli, Config, ExecutorKind, StoreConfig};
use scarab_server::{converged, router, AppState, LogService, SecretInjectingExecutor, SystemClock};
use scarab_storage::ObjectStore;
use scarab_storage_s3::S3Storage;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // rustls 0.23 refuses to pick a process-level default CryptoProvider when
    // more than one backend is linked — and this binary links both: `ring`
    // (sqlx's `tls-rustls-ring`) and `aws-lc-rs` (pulled by object_store). Left
    // unset, the first TLS handshake — the kube client dialing the API server —
    // panics. Install `ring` explicitly (matching sqlx) before any TLS. The
    // Result is Err only if a provider is already installed; ignore it.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();

    // Spec export: write openapi.json and exit. Runs BEFORE the config gate —
    // this path must stay DB-free (ADR-0048 carve-out for pure tooling).
    if let Some(path) = &cli.emit_openapi {
        std::fs::write(path, scarab_server::openapi_json())?;
        println!("wrote OpenAPI document to {path}");
        return Ok(());
    }

    // The startup gate: refuse to boot on invalid configuration, before any
    // socket binds or connection is made (ADR-0048).
    let config = match Config::resolve(&cli) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("scarab-server: refusing to start: {e}");
            std::process::exit(1);
        }
    };
    for line in config.startup_report() {
        tracing::info!("startup: {line}");
        println!("startup: {line}");
    }
    // The dev escape hatch trades hard-fails for SCREAMING (ADR-0048): insecure
    // is opt-in and loud, never silent.
    for warning in config.boot_warnings() {
        tracing::warn!("{warning}");
        eprintln!("{warning}");
    }

    // Config smoke check: validated and reported; exit without side effects.
    if cli.dry_run {
        println!("(dry run — configuration valid; omit --dry-run to start)");
        return Ok(());
    }

    // This replica's identity for leases + outbox claims (ADR-0051): MUST be
    // unique per process — identical owners would make every replica believe
    // it holds every lease (leader election + tail dedup would be void).
    let replica_id = format!("scarab-server-{}", uuid::Uuid::new_v4());

    // Durable store — mandatory, already guaranteed by the config gate.
    let pg = PostgresDb::connect(&config.database_url).await?;
    pg.migrate().await?;
    // Keep a typed handle so the same Postgres adapter can back both the `Db`
    // port and the `EnvironmentStore` port (it implements both).
    let pg = Arc::new(pg);
    let db: Arc<dyn Db> = pg.clone();

    // Object store: MinIO/S3 when SCARAB_S3_BUCKET is set (the dev harness /
    // prod), else a local directory (zero-dependency dev). One S3Storage backs
    // BOTH ports: the log/artifact ObjectStore and the workspace Cas
    // (ADR-0029/0045).
    let storage = Arc::new(match &config.store {
        StoreConfig::S3(s3) => S3Storage::s3(
            s3.bucket.clone(),
            &s3.endpoint,
            &s3.region,
            &s3.access_key,
            &s3.secret_key,
        )?,
        StoreConfig::LocalDir(dir) => S3Storage::local(dir)?,
    });
    let store: Arc<dyn ObjectStore> = storage.clone();
    let workspace_cas: Arc<dyn scarab_storage::Cas> = storage;
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // Secrets store (envelope-encrypted, ADR-0014): built up-front — before the
    // driver — so the launch path can resolve and inject env-scoped secrets
    // (ADR-0037). The KEK comes from the validated config; its absence is only
    // possible under SCARAB_DEV_INSECURE, already warned about above.
    let master_key = config.master_key.unwrap_or_else(|| {
        let mut key = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key);
        key
    });
    let secrets: Arc<dyn scarab_secrets::SecretProvider> = {
        let s =
            scarab_secrets_postgres::PostgresSecrets::connect(&config.database_url, master_key)
                .await?;
        s.migrate().await?;
        Arc::new(s)
    };

    // Retention sweeper (ADR-0050): leader-gated, prunes terminal runs' log
    // blobs + index past the class TTL. Non-terminal (incl. gate-suspended)
    // runs are never eligible by construction; metadata is retained.
    if config.role.runs_driver() {
        scarab_server::retention::spawn_sweeper(
            db.clone(),
            workspace_cas.clone(),
            Arc::clone(&store),
            clock.clone(),
            replica_id.clone(),
            scarab_server::retention::RetentionConfig {
                log_ttl_ms: (config.retention_log_days as i64) * 24 * 60 * 60 * 1000,
            },
            scarab_server::retention::GcConfig {
                workspace_ttl_ms: (config.retention_workspace_days as i64) * 24 * 60 * 60 * 1000,
                // Never sweep objects younger than a day: protects an
                // in-flight ingest whose root is not yet recorded.
                grace_ms: 24 * 60 * 60 * 1000,
            },
            Duration::from_secs(300),
        );
        tracing::info!(
            "retention sweeper on (logs {}d, workspace CAS {}d mark-sweep; metadata retained, ADR-0050)",
            config.retention_log_days,
            config.retention_workspace_days,
        );
    }

    // The OIDC issuer (ADR-0015), built BEFORE the driver so the launch path
    // can mint per-attempt federation tokens; also serves JWKS + discovery.
    // The signing key is the configured PEM — persistent across restarts and
    // replicas — and any failure here is a boot failure (ADR-0048).
    let oidc_issuer: Option<Arc<scarab_server::oidc::Rs256Issuer>> = match &config.oidc {
        Some(oidc) => {
            let pem = std::fs::read_to_string(&oidc.signing_key_file).map_err(|e| {
                format!(
                    "cannot read SCARAB_OIDC_SIGNING_KEY_FILE {}: {e}",
                    oidc.signing_key_file
                )
            })?;
            let issuer =
                scarab_server::oidc::Rs256Issuer::from_pem(oidc.issuer_url.clone(), &pem)
                    .map_err(|e| {
                        format!("invalid OIDC signing key {}: {e}", oidc.signing_key_file)
                    })?;
            Some(Arc::new(issuer))
        }
        None => None,
    };

    // The production forge (ADR-0046): a registry-routed ForgePort — each call
    // resolves its repo through the ForgeConnection registry, constructs the
    // vendor adapter (GitHub App/token, Forgejo token) with credentials
    // fetched from SecretProvider at use-time, and caches it per connection.
    let forge: Arc<dyn scarab_forge::ForgePort> = Arc::new(
        scarab_server::forge_router::RegistryForge::new(
            pg.clone(),
            secrets.clone(),
            config.github_app_id.clone(),
        ),
    );
    // Startup validation (ADR-0046): every registered connection's credential
    // handle must resolve. Missing material is a loud DEGRADED warning — the
    // connection cannot serve until the secret is registered.
    {
        use scarab_forge::ForgeConnectionStore;
        match pg.list_connections().await {
            Ok(conns) => {
                for conn in conns {
                    if let Err(e) =
                        scarab_server::connection_credential(secrets.as_ref(), &conn).await
                    {
                        tracing::warn!(
                            connection = %conn.id,
                            credential_ref = %conn.credential_ref,
                            error = %e,
                            "startup: DEGRADED — forge connection credential unavailable"
                        );
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "startup: could not list forge connections"),
        }
    }

    // Results egress (ADR-0042): a shared HMAC secret both mints the k8s
    // sidecar's fence token and verifies it at the ingest endpoint.
    let results_egress = config.results_egress.as_ref().map(|egress| ResultsEgress {
        base_url: egress.api_url.clone(),
        token_secret: egress.token_secret.clone(),
        sidecar_image: egress.sidecar_image.clone(),
    });

    // Background scheduler + executor loop. The k8s backend best-effort
    // connects (without a reachable cluster the driver is skipped — a degraded
    // state, reported loudly); the local backend needs nothing but a working
    // host (dev/CLI, ADR-0036).
    if config.role.runs_driver() {
        let executor: Option<Arc<dyn Executor>> = match config.executor {
            ExecutorKind::Local => {
                tracing::info!("using the local (host-process) executor — dev/CLI backend");
                Some(Arc::new(
                    LocalExecutor::new().with_default_step_timeout_secs(config.step_timeout_secs),
                ))
            }
            ExecutorKind::K8s => match K8sExecutor::connect(config.namespace.clone()).await {
                Ok(mut exec) => {
                    exec = exec
                        .with_default_step_timeout_secs(config.step_timeout_secs)
                        // Workspace flow (ADR-0029/0045): materialize `needs`
                        // into /workspace, snapshot it back after the step.
                        .with_workspace_cas(workspace_cas.clone())
                        // The canonical clone image (ADR-0045).
                        .with_clone_image(config.clone_image.clone());
                    if let Some(egress) = results_egress.clone() {
                        exec = exec.with_results_egress(egress);
                        tracing::info!("results egress sidecar enabled (ADR-0042)");
                    }
                    Some(Arc::new(exec))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "startup: DEGRADED — no kubernetes cluster reachable; driver not started");
                    None
                }
            },
        };
        if let Some(executor) = executor {
            // Inject env-scoped secrets into deploy-run pods at launch
            // (ADR-0037).
            let mut secret_exec = SecretInjectingExecutor::new(
                executor,
                db.clone(),
                secrets.clone(),
                logs.clone(),
            );
            // Per-attempt OIDC federation tokens (ADR-0015), tmpfs-delivered.
            if let (Some(issuer), Some(oidc_cfg)) = (oidc_issuer.clone(), &config.oidc) {
                secret_exec = secret_exec.with_oidc(
                    issuer,
                    oidc_cfg.issuer_url.clone(),
                    oidc_cfg.audience.clone(),
                );
                tracing::info!("per-run OIDC token injection enabled (ADR-0015)");
            }
            let executor: Arc<dyn Executor> = Arc::new(secret_exec);
            // Enrich clone-step launches (ADR-0045): resolve the clone URL
            // from the registry and mint the short-TTL, read-only-for-forks
            // checkout credential — in memory only, delivered via tmpfs.
            let executor: Arc<dyn Executor> = Arc::new(
                scarab_server::clone_executor::CloneEnrichingExecutor::new(
                    executor,
                    pg.clone(),
                    forge.clone(),
                )
                // Build-step registry auth (ADR-0018): scoped REGISTRY_AUTH
                // secret first, forge-derived credential as fallback.
                .with_secrets(secrets.clone()),
            );
            // Local host processes finish instantly, so re-poll them quickly; k8s
            // Pods take time, so the default window avoids thundering re-claims.
            // A short window is safe either way — `launch` is idempotent (ADR-0021).
            let visibility_ms = match config.executor {
                ExecutorKind::Local => 1_000,
                ExecutorKind::K8s => 30_000,
            };
            // Forge-status posting (ADR-0046): the registry-routed forge posts
            // Running/Succeeded/Failed commit statuses with run deep-links.
            converged::spawn_driver(
                db.clone(),
                clock.clone(),
                executor,
                Some(forge.clone()),
                // Feed the log pipeline from the executor's live tail (ADR-0013).
                Some(logs.clone()),
                replica_id.clone(),
                Duration::from_millis(500),
                visibility_ms,
                (config.step_timeout_secs as i64).saturating_mul(1000),
                config.public_url.clone(),
            );
            tracing::info!("converged driver started");
        }
    }

    let mut state = AppState::new(db, clock, logs)
        // Environments + deployment history: the Postgres adapter is the store.
        // Enables /v1/environments/* and admission enforcement for
        // env-targeting runs (ADR-0024).
        .with_environments(pg.clone())
        // The ForgeConnection registry (ADR-0046): RepoRef→Project resolution,
        // installation auto-registration, webhook replay dedup.
        .with_forge_connections(pg.clone())
        // The acting forge (no more forge=None): webhook-triggered runs read
        // in-repo `.scarab` config through it for real.
        .with_forge(forge.clone())
        // /v1/secrets management with the secrets store built above (ADR-0014).
        .with_secrets(secrets.clone());
    if let Some(secret) = config.github_webhook_secret.clone() {
        state = state.with_github_webhook_secret(secret);
    }
    if let Some(secret) = config.forgejo_webhook_secret.clone() {
        state = state.with_forgejo_webhook_secret(secret);
    }
    // Results ingest (ADR-0042): enables POST …/steps/:step/results for the
    // egress sidecar, verified with the same secret that minted its token.
    if let Some(egress) = &config.results_egress {
        state = state.with_results_token_secret(egress.token_secret.clone());
    }
    // External-gate release tokens (ADR-0034): enables POST …/gates/:step/release.
    if let Some(secret) = config.gate_token_secret.clone() {
        state = state.with_gate_token_secret(secret);
    }
    // Real authn (ADR-0049): OAuth/OIDC login + PG-backed sessions. Absent
    // OAuth config the process only booted under SCARAB_DEV_INSECURE (ADR-0048
    // default-deny) — sessions stay unwired and authz is off, loudly.
    if let Some(oauth_cfg) = config.oauth.clone() {
        let login = Arc::new(scarab_server::oauth::OAuthAuthenticator::new(oauth_cfg));
        state = state
            .with_auth(login.clone(), pg.clone())
            .with_oauth_login(login, config.public_url.clone())
            // Scoped RBAC (ADR-0049 C2): per-request role-in-Org/Project from
            // the native bindings in Postgres.
            .with_rbac(pg.clone());
        tracing::info!("authn: OAuth/OIDC login + PG sessions + scoped RBAC wired (ADR-0049)");
    }
    // OIDC issuer for keyless federation (ADR-0014): serve JWKS + discovery so a
    // cloud provider can verify Scarab-minted tokens. The signing key is loaded
    // from the configured PEM — persistent across restarts/replicas — and any
    // failure here is a boot failure, not a degraded warn (ADR-0048).
    if let Some(issuer) = oidc_issuer.clone() {
        state = state.with_oidc(issuer);
        tracing::info!("OIDC issuer enabled (persistent signing key)");
    }
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&config.addr).await?;
    println!("listening on {}", config.addr);
    axum::serve(listener, app).await?;

    Ok(())
}
