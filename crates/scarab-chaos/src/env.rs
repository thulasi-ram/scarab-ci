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

pub const TERMINAL_RUN_STATUSES: &[&str] = &["succeeded", "failed", "cancelled", "dead_lettered"];
