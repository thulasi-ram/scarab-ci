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
use scarab_executor_k8s::{K8sExecutor, PlacementConfig, ResultsEgress, WorkspaceFetch};
use scarab_executor_local::LocalExecutor;
use scarab_server::config::{Cli, Config, ExecutorKind, Role, StoreConfig};
use scarab_server::{
    converged, router, AppState, LogService, SecretInjectingExecutor, SystemClock,
};
use scarab_storage::ObjectStore;
use scarab_storage_s3::S3Storage;

/// Lifetime of the `browse`-scope workspace token this process mints for itself.
///
/// Short, because it is minted per request and never stored: the only thing the
/// window has to cover is one HTTP round trip to the workspace service plus clock
/// skew between the two Pods. Five minutes is generous for both and still means a
/// token captured off the wire is worthless almost immediately — unlike the
/// results token, which never expires at all (ADR-0061 D1.4 names that as one of
/// the three reasons this is a separate credential).
const BROWSE_TOKEN_TTL_SECS: i64 = 300;

/// The CAS sweeper's Depot durable-index probe (ADR-0067), adapting the one
/// [`scarab_workspace_client::WorkspaceClient`] this process holds to the
/// sweeper's [`scarab_server::retention::DepotDurableIndex`] seam (a newtype
/// because both the trait's home and the client are foreign to this bin).
/// Chunked `/have` durable answers per sweep pass; an error deliberately
/// keeps the torn-durability detector ON (see the seam's docs).
struct DepotDurableProbe(Arc<scarab_workspace_client::WorkspaceClient>);

#[async_trait::async_trait]
impl scarab_server::retention::DepotDurableIndex for DepotDurableProbe {
    async fn durable_missing(
        &self,
        blobs: Vec<String>,
        trees: Vec<String>,
    ) -> Result<
        (
            std::collections::HashSet<String>,
            std::collections::HashSet<String>,
        ),
        String,
    > {
        use scarab_storage::content::ContentSource;
        let blob_ids: Vec<scarab_storage::BlobHash> =
            blobs.into_iter().map(scarab_storage::BlobHash).collect();
        let tree_ids: Vec<scarab_storage::TreeHash> =
            trees.into_iter().map(scarab_storage::TreeHash).collect();
        let (mb, mt) = self
            .0
            .missing(&blob_ids, &tree_ids)
            .await
            .map_err(|e| e.to_string())?;
        Ok((
            mb.into_iter().map(|b| b.0).collect(),
            mt.into_iter().map(|t| t.0).collect(),
        ))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Structured logging (ADR-0053): EnvFilter honors RUST_LOG; the JSON
    // formatter is the production default — SCARAB_LOG_FORMAT=text opts into
    // the human-readable dev format.
    {
        use tracing_subscriber::EnvFilter;
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        if std::env::var("SCARAB_LOG_FORMAT").as_deref() == Ok("text") {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        } else {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
    }

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

    // ADR-0061: the workspace service is a DATA-plane role. It shares this
    // binary — one image, so server↔service version skew is structurally
    // impossible under one Helm release — but NOT the durable core. Since
    // ADR-0067 part 2 it DOES connect to the same Postgres (for derived,
    // rebuildable rows: drain records, write ledgers — `workspaced::run`
    // builds its own pool), but it NEVER migrates: the control plane owns
    // every table's DDL, so a Depot rolled ahead of the control plane cannot
    // half-migrate anything. That deployment-ordering property is what the
    // old "never connects" guard was actually defending, and it survives.
    //
    // The early return is deliberately HERE, before anything below it runs.
    // Everything that follows is the durable core's composition root: it
    // migrates Postgres, connects and migrates the secrets store, reads the
    // OIDC PEM, and provisions forge connections — none of which the
    // workspace service has, needs, or may be allowed to do from N per-failure-
    // domain replicas. Do not move this down, and do not restructure the code
    // below it to make the role a branch: a branch is something a future edit
    // can fall through.
    if matches!(config.role, Role::Workspace) {
        return scarab_server::workspaced::run(&config).await;
    }

    // The config gate guarantees the URL for every role (ADR-0048/ADR-0067),
    // so the type is a `String`, not an `Option` with a dispatch-site expect.
    let database_url = config.database_url.clone();

    // This replica's identity for leases + outbox claims (ADR-0051): MUST be
    // unique per process — identical owners would make every replica believe
    // it holds every lease (leader election + tail dedup would be void).
    let replica_id = format!("scarab-server-{}", uuid::Uuid::new_v4());

    // Durable store — mandatory, already guaranteed by the config gate.
    let pg = PostgresDb::connect(&database_url).await?;
    pg.migrate().await?;
    // Keep a typed handle so the same Postgres adapter can back both the `Db`
    // port and the `EnvironmentStore` port (it implements both).
    let pg = Arc::new(pg);
    let db: Arc<dyn Db> = pg.clone();

    // Object store: MinIO/S3 when SCARAB_S3_BUCKET is set (the dev harness /
    // prod), else the EXPLICITLY chosen local directory — the config gate
    // refused to boot without one or the other (ADR-0067 part 1: the object
    // store is a hard requirement, never a silent fallback). One S3Storage backs
    // BOTH ports: the log/artifact ObjectStore and the workspace Cas
    // (ADR-0029/0045).
    // The CAS-leg parallelism comes from validated config (ADR-0061 s2), not from
    // an ambient env read inside the adapter — so `startup_report()` above has
    // already told the operator which value is live.
    let storage = Arc::new(
        match &config.store {
            StoreConfig::S3(s3) => S3Storage::s3(
                s3.bucket.clone(),
                &s3.endpoint,
                &s3.region,
                &s3.access_key,
                &s3.secret_key,
            )?,
            StoreConfig::LocalDir(dir) => S3Storage::local(dir)?,
        }
        .with_concurrency(config.cas_concurrency),
    );
    let store: Arc<dyn ObjectStore> = storage.clone();
    // The COLD tier: object storage, direct. This is the durable one — ADR-0061's
    // retention table gives it a TTL and calls it "the guarantee users are given".
    let cold_cas: Arc<dyn scarab_storage::Cas> = storage;

    // The Depot client (ADR-0061/0067): ONE client, two jobs. Its
    // `cache_only_cas` twin is the warm tier of the tiered READ handle below;
    // held concretely it is also the executor's drain-rendezvous handle,
    // because reading a fence's drain record back (`drain_record`) is a
    // client capability, not a `Cas` port method.
    //
    // The token is minted **per request**, in `browse` scope, for this process's
    // own use (`workspace_token::browse_claims`). Per-request rather than once at
    // boot because a token has an `exp`: a server that minted one at startup
    // would start 401-ing a day later from nothing anyone changed, and minting
    // one with no meaningful expiry would rebuild the results token's wart that
    // ADR-0061 D1.4 refused to inherit.
    let depot_client: Option<Arc<scarab_workspace_client::WorkspaceClient>> =
        config.workspace.as_ref().map(|ws| {
            use scarab_executor_k8s::workspace_token;
            let secret = ws.token_secret.clone();
            Arc::new(scarab_workspace_client::WorkspaceClient::with_minted_token(
                ws.url.clone(),
                move || {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    workspace_token::mint(
                        &secret,
                        &workspace_token::browse_claims(now + BROWSE_TOKEN_TTL_SECS),
                    )
                },
            ))
        });

    // The workspace `Cas` this process hands to Browse, the GC mark walk, the
    // rerun-widening oracle and the debug pod (ADR-0061 D1.6) — and the READ
    // half of the executor's drain.
    //
    // WARM = the workspace service, over HTTP. COLD = the object store above,
    // direct. **This is a read-and-repair handle now, not a write path**
    // (ADR-0064 control-plane half):
    //
    //  * **reads fall through to cold** when the service is unreachable. This
    //    process already holds object-store credentials, so going direct crosses
    //    no trust boundary and creates no second data path for Steps — the
    //    literal reading of "a warm miss is slower, never wrong". Note this is
    //    the OPPOSITE of a Step Pod, which must fail closed: a Pod has no
    //    credentials, by design (ADR-0042).
    //  * **the drain does NOT write through it.** The drain is in-Pod since
    //    git-bug 212bb13: `scarab-wsfetch` ingests /workspace straight to the
    //    Depot under its fenced token, durable bytes stream into the fence's
    //    PACKS as they arrive, and the drain record's index transaction is
    //    the durability gate (ADR-0067 part 4 — the flush RPC and the second
    //    pass it implied are deleted). This process only reads the record
    //    back (`drain_record`) and classifies.
    //  * **stray writes** (`put_blob`/`put_tree` from anything that is not the
    //    drain) keep the old cold-first rule: cold decides success, a warm
    //    failure is a warning plus a counter. The warm leg is the
    //    `cache_only_cas` twin: a fenceless PUT opens no pack and can never
    //    be durable on the Depot, so the cache-only label states on the wire
    //    what was always true — the durable copy is the direct cold write.
    let workspace_cas: Arc<dyn scarab_storage::Cas> = match &depot_client {
        Some(client) => {
            tracing::info!(
                url = %config.workspace.as_ref().map(|ws| ws.url.as_str()).unwrap_or_default(),
                "workspace snapshots: warm = the workspace service (cache-only leg), cold = \
                 the object store, direct (ADR-0061 D1.6 reads fall through to cold; drains \
                 are in-Pod into the Depot's packs — ADR-0067, no flush pass)"
            );
            Arc::new(
                scarab_storage::tiered::TieredCas::new(
                    Arc::new(client.cache_only_cas()),
                    cold_cas,
                )
                .fall_through_on_warm_error(),
            )
        }
        // No service configured: the object store IS the whole store. The
        // executor already refuses to launch a step that inherits a workspace in
        // this state (fail-closed) and its drain writes this handle directly
        // (durable by construction), so this path serves Browse and GC over
        // pre-ADR-0061 snapshots and drain-less dev.
        None => cold_cas,
    };
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // Secrets store (envelope-encrypted, ADR-0014): built up-front — before the
    // driver — so the launch path can resolve and inject env-scoped secrets
    // (ADR-0037). The KEK comes from the validated config; its absence is only
    // possible under SCARAB_DEV_INSECURE, already warned about above.
    let master_keys = match &config.master_keys {
        // First key = active writer, the rest decrypt-only (f37463a). Config
        // already rejected duplicates, so construction cannot fail here.
        Some(keys) => scarab_secrets_postgres::MasterKeySet::new(keys.clone())?,
        // Dev-insecure ephemeral: a one-key set, loud-warned above.
        None => {
            let mut key = [0u8; 32];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key);
            scarab_secrets_postgres::MasterKeySet::single(key)
        }
    };
    let secrets: Arc<dyn scarab_secrets::SecretProvider> = {
        let s = scarab_secrets_postgres::PostgresSecrets::connect(&database_url, master_keys)
            .await?;
        s.migrate().await?;
        // Boot rewrap sweep (f37463a): converge every row to the active key,
        // on EVERY core replica — the per-row CAS makes concurrent sweeps
        // safe and the work converges to zero, so no leader lease. A sweep
        // failure never blocks boot: reads still rewrap lazily.
        match s.rewrap_all(config.dev_insecure).await {
            Ok(sum) => {
                tracing::info!(
                    "secrets rewrap sweep: {} rewrapped, {} legacy rows upgraded, \
                     {} lost races, {} unreadable (skipped), {} still under \
                     non-active keys",
                    sum.rewrapped,
                    sum.upgraded_legacy,
                    sum.lost_races,
                    sum.unreadable,
                    sum.remaining,
                );
                scarab_server::metrics::set_secrets_rows_under_nonactive_key(sum.remaining);
            }
            Err(e) => tracing::warn!(
                "secrets rewrap sweep failed (boot continues; reads rewrap lazily): {e}"
            ),
        }
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
                artifact_ttl_ms: (config.retention_artifact_days as i64) * 24 * 60 * 60 * 1000,
            },
            scarab_server::retention::GcConfig {
                workspace_ttl_ms: (config.retention_workspace_days as i64) * 24 * 60 * 60 * 1000,
                // Never sweep objects younger than a day: protects an
                // in-flight ingest whose root is not yet recorded.
                grace_ms: 24 * 60 * 60 * 1000,
            },
            // The Depot durable-index probe (ADR-0067): durable drains write
            // packs, not loose objects, so the sweep must ask the pack index
            // before alarming a marked object as torn-cold residue. No Depot
            // configured = loose listing alone, status quo (this handle also
            // being how the drain-less shape is detected).
            depot_client
                .as_ref()
                .map(|c| {
                    Arc::new(DepotDurableProbe(c.clone()))
                        as Arc<dyn scarab_server::retention::DepotDurableIndex>
                }),
            Duration::from_secs(300),
        );
        tracing::info!(
            "retention sweeper on (logs {}d, workspace CAS {}d mark-sweep; metadata retained, ADR-0050)",
            config.retention_log_days,
            config.retention_workspace_days,
        );

        // Committed-fence expiry (git-bug 6499fb1, ADR-0065/0067): the
        // control plane selects AND executes — pointers only; the Depot's
        // rowless reclaimer collects the bytes a cadence later. On its own
        // small pool (a composition-root concern: `PgDb`'s pool stays
        // private, and the pass holds one connection for the reclaimer's
        // advisory lock plus one per victim transaction), on the sweeper's
        // cadence; cross-replica and cross-pass serialization is the
        // advisory lock itself, so no lease is needed.
        let expiry_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&database_url)
            .map_err(|e| format!("depot expiry pool: {e}"))?;
        // The per-run TTL source (ADR-0065 s2): the operator RetentionProfile
        // registry over the flat env fallbacks. A bad path/parse/validation
        // is a boot failure (ADR-0048), mirroring the placement config.
        let flat_ttls = scarab_server::depot_expiry::ExpiryTtls {
            pack_ttl_secs: (config.retention_pack_days as i64) * 24 * 60 * 60,
            workspace_ttl_secs: (config.retention_workspace_days as i64) * 24 * 60 * 60,
        };
        let retention_registry = match &config.retention_config_file {
            Some(path) => {
                let raw = std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read SCARAB_RETENTION_CONFIG_FILE {path}: {e}"))?;
                let file: scarab_server::depot_expiry::RetentionConfigFile =
                    serde_yaml::from_str(&raw)
                        .map_err(|e| format!("invalid retention config {path}: {e}"))?;
                scarab_server::depot_expiry::RetentionRegistry::new(file.profiles, flat_ttls)
                    .map_err(|e| format!("invalid retention config {path}: {e}"))?
            }
            None => scarab_server::depot_expiry::RetentionRegistry::flat(flat_ttls),
        };
        scarab_server::depot_expiry::spawn_expiry(
            expiry_pool,
            retention_registry,
            Duration::from_secs(300),
        );
        tracing::info!(
            "depot committed-fence expiry on (packs {}d default, per-run RetentionProfile \
             honoured; pin wins, borrow edges gate, pre-epoch floor — git-bug 6499fb1)",
            config.retention_pack_days,
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
            let issuer = scarab_server::oidc::Rs256Issuer::from_pem(oidc.issuer_url.clone(), &pem)
                .map_err(|e| format!("invalid OIDC signing key {}: {e}", oidc.signing_key_file))?;
            Some(Arc::new(issuer))
        }
        None => None,
    };

    // Bootstrap-free App PEM (enh 245a99c): env/file OVERRIDES the DB-stored
    // `_forge` credential so a fresh DB (or a GitOps deploy) needs no reseed PUT.
    // Inline `SCARAB_GITHUB_APP_PEM` wins; a bad `..._FILE` path is a boot
    // failure (ADR-0048), mirroring the OIDC signing key above.
    let github_app_pem: Option<String> = match (&config.github_app_pem, &config.github_app_pem_file)
    {
        (Some(pem), _) => Some(pem.clone()),
        (None, Some(path)) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read SCARAB_GITHUB_APP_PEM_FILE {path}: {e}"))?,
        ),
        (None, None) => None,
    };

    // Operator placement config (ADR-0055): the cluster baseline + named
    // PlacementProfile registry, one gitops-managed file. A bad path/parse is a
    // boot failure (ADR-0048), mirroring the files above. Empty when unset.
    let placement: PlacementConfig = match &config.placement_config_file {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read SCARAB_PLACEMENT_CONFIG_FILE {path}: {e}"))?;
            serde_yaml::from_str(&raw)
                .map_err(|e| format!("invalid placement config {path}: {e}"))?
        }
        None => PlacementConfig::default(),
    };

    // Declarative connections (ADR-0060 part D): the `connections:` block is
    // ALREADY parsed, validated and credential-resolved by the config gate — so
    // all that remains is applying it to the registry. Config-owned connections
    // are provisioned as real registry rows (so the forge router, the clone-step
    // enricher and webhook resolution need no special case), and a connection
    // declared in config that already exists as a DB-owned row is a single-owner
    // collision that REFUSES the boot rather than picking a winner.
    let credential_overrides = Arc::new(
        scarab_server::connections_config::CredentialOverrides::from_specs(&config.connections)
            .with_github_app_pem(github_app_pem.clone(), config.github_app_id.is_some()),
    );
    {
        let provisioned =
            scarab_server::connections_config::provision(pg.as_ref(), &config.connections)
                .await
                .map_err(|e| format!("refusing to start: {e}"))?;
        if !provisioned.owned.is_empty() {
            tracing::info!(
                connections = ?provisioned.owned,
                bound = ?provisioned.bound,
                "startup: provisioned config-owned forge connections (ADR-0060 part D)"
            );
        }
        // Ownership released, not deleted: a Project (and its Environments,
        // secrets, RBAC) hangs off a repo binding, so undeclaring a connection
        // hands it back to the UI instead of destroying governance.
        for id in &provisioned.released {
            tracing::warn!(
                connection = %id,
                "startup: connection is no longer declared in config — ownership released \
                 to the database; it is now editable/deletable in Settings"
            );
        }
    }

    // The production forge (ADR-0046): a registry-routed ForgePort — each call
    // resolves its repo through the ForgeConnection registry, constructs the
    // vendor adapter (GitHub App/token, Forgejo token) with credentials
    // fetched at use-time via the one resolution path (deployment override →
    // SecretProvider, ADR-0060 part D), and cached per connection.
    // One instance, two ports: the repo-routed `ForgePort` every run path uses,
    // and the connection-scoped `ForgeAdapters` the ADR-0060 onboarding endpoints
    // need (a connection with nothing bound yet has no repo to route through).
    // They share the adapter cache by construction.
    let registry_forge = Arc::new(
        scarab_server::forge_router::RegistryForge::new(
            pg.clone(),
            secrets.clone(),
            config.github_app_id.clone(),
            github_app_pem.clone(),
        )
        // Hooks this server registers must be signed with the secret its own
        // `/webhooks/forgejo` verifies, or every delivery comes back 401.
        .with_forgejo_webhook_secret(config.forgejo_webhook_secret.clone())
        .with_credential_overrides(credential_overrides.clone()),
    );
    let forge: Arc<dyn scarab_forge::ForgePort> = registry_forge.clone();
    // Startup validation (ADR-0046): every registered connection's credential
    // must resolve. A credential the DEPLOYMENT supplies (a config-declared
    // `credential.env`/`file`, or the kind-wide App PEM) is already in hand and
    // needs no secret store — the same override table the forge router uses
    // answers that, so the audit and the runtime can never disagree. Anything
    // else must resolve from SecretProvider, and missing material is a loud
    // DEGRADED warning: only the running server can PUT it, so refusing the boot
    // would deadlock a fresh database.
    {
        use scarab_forge::ForgeConnectionStore;
        match pg.list_connections().await {
            Ok(conns) => {
                for conn in conns {
                    if credential_overrides.covers(&conn) {
                        continue;
                    }
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
    let mut driver_handle: Option<tokio::task::JoinHandle<()>> = None;
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
                        // This is the drain's READ half (tiered, falls through
                        // to cold); the WRITE half is the Depot handle below.
                        .with_workspace_cas(workspace_cas.clone())
                        // The canonical clone image (ADR-0045).
                        .with_clone_image(config.clone_image.clone())
                        // Artifact harvest (ADR-0052): /scarab/artifacts →
                        // object blobs + Pod-annotation metadata.
                        .with_artifact_store(store.clone())
                        // Placement (ADR-0055): baseline + PlacementProfile registry.
                        .with_placement(placement.clone());
                    // The workspace service (ADR-0061 s3-feed): a Step with
                    // `needs:` is provisioned by an init container that dials it
                    // directly. Without this the executor REFUSES to launch such a
                    // step (fail-closed) rather than run it against a silently
                    // empty /workspace — the old control-plane exec-tar feed is
                    // deleted, not kept as a fallback.
                    if let Some(ws) = &config.workspace {
                        exec = exec.with_workspace_service(WorkspaceFetch {
                            url: ws.url.clone(),
                            token_secret: ws.token_secret.clone(),
                            fetcher_image: ws.fetcher_image.clone(),
                            helper_resources: scarab_pipeline::Resources {
                                cpu_millis: ws.helper_cpu_millis,
                                memory_mib: ws.helper_memory_mib,
                            },
                        });
                        // The drain's read-back half (ADR-0067): the Pod's
                        // helper drains straight into packs on the Depot; the
                        // control plane only reads the drain record through
                        // this client — a success record already MEANS durable.
                        if let Some(client) = &depot_client {
                            exec = exec.with_workspace_depot(client.clone());
                        }
                        tracing::info!(
                            url = %ws.url,
                            image = %ws.fetcher_image,
                            "workspace fetcher enabled (ADR-0061 s3-feed; EAGER — the node \
                             driver replaces this)"
                        );
                    } else {
                        tracing::warn!(
                            "no workspace service configured (SCARAB_WORKSPACE_TOKEN_SECRET \
                             unset): steps that inherit a workspace will be REFUSED at launch \
                             (ADR-0061)"
                        );
                    }
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
            let mut secret_exec =
                SecretInjectingExecutor::new(executor, db.clone(), secrets.clone(), logs.clone());
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
            driver_handle = Some(converged::spawn_driver(
                db.clone(),
                clock.clone(),
                executor,
                // Cache-key resolution at launch (ADR-0065 s1): the same CAS
                // the rerun-widening oracle reads.
                Some(Arc::new(scarab_server::retention::CasSnapshots(
                    workspace_cas.clone(),
                ))),
                Some(forge.clone()),
                // Feed the log pipeline from the executor's live tail (ADR-0013).
                Some(logs.clone()),
                replica_id.clone(),
                Duration::from_millis(500),
                visibility_ms,
                (config.step_timeout_secs as i64).saturating_mul(1000),
                config.public_url.clone(),
            ));
            tracing::info!("converged driver started");
        }
    }

    let mut state = AppState::new(db, clock, logs)
        // How the outside world reaches this server (SCARAB_PUBLIC_URL).
        // UNCONDITIONAL, and it must stay that way: every webhook Scarab
        // registers posts to `{public_url}/webhooks/{forge}`, so gating this on
        // any other feature (it used to ride on `with_oauth_login`) makes every
        // hook in a login-less deployment point at localhost and die silently.
        .with_public_url(config.public_url.clone())
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
        // The same wiring, connection-scoped: repo enumeration for the bind
        // pick-list and per-repo webhook registration (ADR-0060).
        .with_forge_adapters(registry_forge.clone())
        // /v1/secrets management with the secrets store built above (ADR-0014).
        .with_secrets(secrets.clone())
        // Artifact list/download (ADR-0052), served from the object store.
        .with_artifact_store(store.clone())
        // Read-only workspace browser (ADR-0029): serves a step's output
        // snapshot tree + file bytes for the run detail Inspector.
        .with_workspace_cas(workspace_cas.clone())
        // The cold tier's TIME bound (ADR-0061 s5), from the SAME config value the
        // GC sweeper above runs on — so what the UI promises and what the sweeper
        // enforces cannot drift.
        .with_snapshot_retention_days(config.retention_workspace_days)
        // The same credential-override table the forge router resolves through
        // (ADR-0060 part D), so the Settings health readout reports a
        // config-supplied credential as present rather than MISSING.
        .with_credential_overrides(credential_overrides.clone());
    // Debug shell (step-attach): only the k8s executor can exec into a running
    // Pod. A dedicated kube client, independent of the runs driver, so a
    // UI-only replica can still serve attach. Absent a cluster it stays off.
    if matches!(config.executor, ExecutorKind::K8s) {
        match K8sExecutor::connect(config.namespace.clone()).await {
            Ok(exec) => {
                // The workspace CAS lets the debug-pod re-materialize a finished
                // step's snapshot. One instance serves both attach and debug-pod.
                // Placement (ADR-0055) applies here too — a debug Pod must schedule
                // on the same tainted nodes as the real step.
                let mut exec = exec
                    .with_workspace_cas(workspace_cas.clone())
                    .with_placement(placement.clone());
                // A debug Pod re-materializes its snapshot through the SAME fetcher
                // a real step uses (ADR-0061 s3-feed) — the copy-pasted feed it
                // used to carry is gone (git-bug 64897db). Without a service, a
                // debug Pod *with* a snapshot reports Unavailable; one without a
                // snapshot still opens an empty shell.
                if let Some(ws) = &config.workspace {
                    exec = exec.with_workspace_service(WorkspaceFetch {
                        url: ws.url.clone(),
                        token_secret: ws.token_secret.clone(),
                        fetcher_image: ws.fetcher_image.clone(),
                        helper_resources: scarab_pipeline::Resources {
                            cpu_millis: ws.helper_cpu_millis,
                            memory_mib: ws.helper_memory_mib,
                        },
                    });
                }
                let exec = Arc::new(exec);
                state = state.with_attacher(exec.clone()).with_debug_launcher(exec);
                tracing::info!("debug shell: step-attach + debug-pod enabled (k8s exec)");
            }
            Err(e) => {
                tracing::warn!(error = %e, "debug shell: attach/debug-pod disabled — no cluster reachable");
            }
        }
    }
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
            .with_oauth_login(login)
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
    // The embedded web UI (ADR-0054): served when the baked dist exists
    // (the production image sets SCARAB_UI_DIR); dev stays API-only.
    let ui_dir = std::env::var("SCARAB_UI_DIR").unwrap_or_else(|_| "/usr/share/scarab/ui".into());
    if std::path::Path::new(&ui_dir).join("index.html").is_file() {
        state = state.with_ui_dir(&ui_dir);
        tracing::info!(dir = %ui_dir, "web UI embedded (same-origin, ADR-0054)");
    }
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&config.addr).await?;
    println!("listening on {}", config.addr);
    // Graceful shutdown (ADR-0053): SIGTERM/ctrl-c stops accepting, drains
    // in-flight connections (incl. SSE), then stops the driver — no torn
    // work on a rollout (safety still rests on crash-idempotency).
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    if let Some(handle) = driver_handle {
        tracing::info!("shutdown: stopping the converged driver");
        handle.abort();
        let _ = handle.await;
    }
    tracing::info!("shutdown complete");

    Ok(())
}

/// Resolves on SIGTERM (k8s rollout) or ctrl-c (dev).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        sig.recv().await;
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutdown signal received — draining");
}
