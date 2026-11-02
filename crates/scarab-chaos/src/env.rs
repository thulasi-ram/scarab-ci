//! Connections to the stack: the Scarab API, the kind cluster (only through
//! its own kubeconfig file, never the ambient one), Postgres and the bucket.

use std::path::Path;
use std::time::Duration;

use kube::config::{KubeConfigOptions, Kubeconfig};

/// The kube context `deploy/kind/up.sh` writes into `deploy/kind/.kubeconfig`.
pub const KIND_CONTEXT: &str = "kind-scarab-dev";

/// A kube client from the kind cluster's own kubeconfig. Refuses any file
/// whose context does not point at a loopback API server: kind publishes its
/// API on 127.0.0.1, a remote cluster never is.
pub async fn kube_client(kubeconfig: &Path) -> Result<kube::Client, String> {
    let kc = Kubeconfig::read_from(kubeconfig).map_err(|e| format!("{}: {e}", kubeconfig.display()))?;
    let ctx = kc
        .contexts
        .iter()
        .find(|c| c.name == KIND_CONTEXT)
        .and_then(|c| c.context.as_ref())
        .ok_or_else(|| format!("{}: no context {KIND_CONTEXT}", kubeconfig.display()))?;
    let server = kc
        .clusters
        .iter()
        .find(|c| c.name == ctx.cluster)
        .and_then(|c| c.cluster.as_ref())
        .and_then(|c| c.server.clone())
        .unwrap_or_default();
    if !(server.starts_with("https://127.0.0.1:") || server.starts_with("https://localhost:")) {
        return Err(format!(
            "refusing: context {KIND_CONTEXT} in {} points at '{server}', not a local kind API server",
            kubeconfig.display()
        ));
    }
    let opts = KubeConfigOptions {
        context: Some(KIND_CONTEXT.into()),
        ..Default::default()
    };
    let config = kube::Config::from_custom_kubeconfig(kc, &opts)
        .await
        .map_err(|e| format!("kubeconfig: {e}"))?;
    kube::Client::try_from(config).map_err(|e| format!("kube client: {e}"))
}

/// A Postgres pool on the chaos database, through a port-forward the recipe
/// opens straight to Postgres (not through toxiproxy: the checker is not
/// under test).
pub async fn pg(url: &str) -> Result<sqlx::PgPool, String> {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(20))
        .connect(url)
        .await
        .map_err(|e| format!("postgres: {e}"))
}

/// The chaos bucket, through a port-forward straight to MinIO.
pub fn bucket(
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<object_store::aws::AmazonS3, String> {
    object_store::aws::AmazonS3Builder::new()
        .with_access_key_id(access_key)
        .with_secret_access_key(secret_key)
        .with_bucket_name(bucket)
        .with_region("us-east-1")
        .with_endpoint(endpoint)
        .with_allow_http(true)
        .build()
        .map_err(|e| format!("bucket: {e}"))
}

/// The Scarab API.
#[derive(Clone)]
pub struct Api {
    pub base: String,
    http: reqwest::Client,
}

impl Api {
    pub fn new(base: &str) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            http,
        })
    }

    pub async fn create_run(&self, body: &serde_json::Value) -> Result<String, String> {
        let resp = self
            .http
            .post(format!("{}/v1/runs", self.base))
            .json(body)
            .send()
            .await
            .map_err(|e| format!("POST /v1/runs: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("POST /v1/runs: {status}: {text}"));
        }
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{e}: {text}"))?;
        v["id"].as_str().map(str::to_string).ok_or_else(|| format!("no id in {text}"))
    }

    /// `POST /v1/runs/{id}/cancel`. 409 (already terminal) is not an error.
    pub async fn cancel_run(&self, id: &str) -> Result<(), String> {
        let resp = self
            .http
            .post(format!("{}/v1/runs/{id}/cancel", self.base))
            .send()
            .await
            .map_err(|e| format!("cancel run {id}: {e}"))?;
        let status = resp.status();
        if status.is_success() || status == reqwest::StatusCode::CONFLICT {
            return Ok(());
        }
        Err(format!("cancel run {id}: {status}: {}", resp.text().await.unwrap_or_default()))
    }

    /// A send whose non-2xx answer is an `Err` naming `what`; the body as
    /// JSON (`null` when empty).
    async fn json(&self, what: &str, req: reqwest::RequestBuilder) -> Result<serde_json::Value, String> {
        let resp = req.send().await.map_err(|e| format!("{what}: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("{what}: {status}: {text}"));
        }
        if text.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| format!("{what}: {e}: {text}"))
    }

    /// A Forgejo connection to `base_url` in this database, made fresh:
    /// `up.sh` mints a new Forgejo token on every run and revokes the old
    /// ones, so a connection an earlier session left holds a dead credential.
    /// Deletes every connection to `base_url` (unbinding its repos), then
    /// creates one serving `org`. Returns its id.
    pub async fn recreate_forgejo_connection(
        &self,
        org: &str,
        base_url: &str,
        token: &str,
    ) -> Result<String, String> {
        let want = base_url.trim_end_matches('/');
        let list = self
            .json("GET /v1/connections", self.http.get(format!("{}/v1/connections", self.base)))
            .await?;
        for c in list.as_array().into_iter().flatten() {
            let (Some(id), Some(url)) = (c["id"].as_str(), c["base_url"].as_str()) else {
                continue;
            };
            if url.trim_end_matches('/') == want {
                self.json(
                    "DELETE /v1/connections/{id}",
                    self.http
                        .delete(format!("{}/v1/connections/{id}?unbind_repos=true", self.base)),
                )
                .await?;
            }
        }
        let created = self
            .json(
                "POST /v1/connections",
                self.http
                    .post(format!("{}/v1/connections", self.base))
                    .json(&serde_json::json!({ "org": org, "kind": "forgejo", "base_url": want, "credential": token })),
            )
            .await?;
        created["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("POST /v1/connections: no id in {created}"))
    }

    /// Bind `owner/name` to connection `conn` without a webhook (the probe is
    /// dispatched, never pushed). Returns the (org, project) the bind made.
    pub async fn bind_repo(&self, conn: &str, owner: &str, name: &str) -> Result<(String, String), String> {
        let bound = self
            .json(
                "bind repo",
                self.http
                    .post(format!("{}/v1/connections/{conn}/repos", self.base))
                    .json(&serde_json::json!({ "owner": owner, "name": name, "register_webhook": false })),
            )
            .await?;
        match (bound["org"].as_str(), bound["project"].as_str()) {
            (Some(org), Some(project)) => Ok((org.to_string(), project.to_string())),
            _ => Err(format!("bind repo: no org/project in {bound}")),
        }
    }

    /// `POST /v1/repos/{org}/{repo}/dispatch` with kind `api`. Returns the Run id.
    pub async fn dispatch(
        &self,
        org: &str,
        repo: &str,
        pipeline: &str,
        r#ref: &str,
        params: &serde_json::Value,
    ) -> Result<String, String> {
        let created = self
            .json(
                "dispatch",
                self.http
                    .post(format!("{}/v1/repos/{org}/{repo}/dispatch", self.base))
                    .json(&serde_json::json!({
                        "ref": r#ref, "pipeline": pipeline, "kind": "api", "params": params,
                        "reason": "scarab-chaos Cache probe",
                    })),
            )
            .await?;
        created["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("dispatch: no id in {created}"))
    }

    /// `status` of `GET /v1/runs/{id}`.
    pub async fn run_status(&self, id: &str) -> Result<String, String> {
        let resp = self
            .http
            .get(format!("{}/v1/runs/{id}", self.base))
            .send()
            .await
            .map_err(|e| format!("GET run {id}: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("GET run {id}: {status}: {text}"));
        }
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{e}: {text}"))?;
        v["status"].as_str().map(str::to_string).ok_or_else(|| format!("no status in {text}"))
    }
}

/// Runs in the chaos database that are not terminal: (id, status). Only the
/// harness creates Runs there, so any such Run is a scenario's.
pub async fn non_terminal_runs(pg: &sqlx::PgPool) -> Result<Vec<(String, String)>, String> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT id, status FROM runs WHERE NOT (status = ANY($1)) ORDER BY created_at",
    )
    .bind(TERMINAL_RUN_STATUSES)
    .fetch_all(pg)
    .await
    .map_err(|e| format!("postgres: {e}"))
}

pub const TERMINAL_RUN_STATUSES: &[&str] = &["succeeded", "failed", "cancelled", "dead_lettered"];
