//! The Cache probe: one cached Step whose second Run restores the first Run's
//! Cache, beside every scenario's workload (git-bug 12e2f6b, ADR-0077 grill
//! amendment item 5). At two Depot replicas a Cache is saved to and restored
//! from its **home** replica; this shows whether that happens under the
//! scenario's fault.
//!
//! A Cache miss is correct by design in general (ADR-0065: a Cache is
//! warm-only and may be evicted), so the probe checks one narrow case,
//! invariant **C1** ([`c1`]): when the seed Run succeeded (it saved the Cache
//! to its home before the workload started) and no fault hit that home
//! replica, the probe's restore is a hit from the home. In every other case
//! C1 is not exercised and says why. The probe's Runs are not scenario Runs:
//! the other invariants and their minimums do not count them.
//!
//! A Cache needs a Project: the scheduler keys cache rows by the Run's project
//! and leaves the Cache off for an untenanted Run, and the inline
//! `POST /v1/runs` has no `cache:` at all. So the probe is a repo pipeline:
//! the harness writes [`CacheProbe::pipeline_yaml`] to a repo of the
//! in-cluster Forgejo (`up.sh` seeds Forgejo in every mode), registers a
//! connection to it in the chaos database, binds the repo without a webhook,
//! and dispatches the pipeline (`on: api`) twice:
//!
//! 1. the **seed** Run, before the workload, waited for: the cached Step finds
//!    no Cache and saves one;
//! 2. the **probe** Run, submitted with the workload so it runs under the
//!    fault: the cached Step restores the seed's Cache from its home.
//!
//! The pipeline text is in the scenario file, so a replay writes the same
//! pipeline. Each run gets a new salt (the lock file's content, so the cache
//! key): the seed of a replay starts cold too.

use std::time::Duration;

use base64::Engine as _;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api as KubeApi, ListParams, LogParams};
use serde::{Deserialize, Serialize};

use crate::env::{Api, TERMINAL_RUN_STATUSES};

/// The env var the executor sets on a cached Step's containers to the Cache's
/// home replica (`workspace_token::WORKSPACE_CACHE_URL_ENV` in
/// scarab-executor-k8s, which this crate does not link).
pub const CACHE_URL_ENV: &str = "SCARAB_WORKSPACE_CACHE_URL";

/// The Forgejo repo the probe's pipeline lives in.
pub const REPO: &str = "chaos-cache";
/// The pipeline's name (`.scarab/<name>.yaml`).
pub const PIPELINE: &str = "chaos-cache";
/// The Step with the `cache:`.
pub const CACHED_STEP: &str = "cached";
/// The cached directory, workspace-relative.
pub const CACHE_DIR: &str = "deps";
/// How long the harness waits for the seed Run before it submits the
/// workload anyway.
pub const SEED_TIMEOUT: Duration = Duration::from_secs(300);

/// The in-cluster Forgejo, as `deploy/kind/chaos.sh` exports it from
/// `deploy/kind/.state/forgejo.env`. `Debug` leaves the token out.
#[derive(Clone)]
pub struct Forgejo {
    /// The host's port-forward to Forgejo (`SCARAB_CHAOS_FORGEJO_URL`).
    pub url: String,
    /// The URL the server reaches Forgejo on, the connection's base URL
    /// (`SCARAB_CHAOS_FORGEJO_SERVER_URL`).
    pub server_url: String,
    pub token: String,
    pub owner: String,
}

impl std::fmt::Debug for Forgejo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forgejo")
            .field("url", &self.url)
            .field("server_url", &self.server_url)
            .field("token", &"<redacted>")
            .field("owner", &self.owner)
            .finish()
    }
}

impl Forgejo {
    /// `Err` names the first variable that is not set.
    pub fn from_env() -> Result<Forgejo, String> {
        let var = |k: &str| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{k} is not set"))
        };
        Ok(Forgejo {
            url: var("SCARAB_CHAOS_FORGEJO_URL")?
                .trim_end_matches('/')
                .to_string(),
            server_url: var("SCARAB_CHAOS_FORGEJO_SERVER_URL")?
                .trim_end_matches('/')
                .to_string(),
            token: var("SCARAB_CHAOS_FORGEJO_TOKEN")?,
            owner: var("SCARAB_CHAOS_FORGEJO_OWNER")?,
        })
    }
}

/// The probe as planned and as it went. Stored in `scenario.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheProbe {
    /// `.scarab/chaos-cache.yaml` as written to the repo.
    pub pipeline_yaml: String,
    /// The lock file's content for this run (`<kind>-<seed>-<ms>`).
    #[serde(default)]
    pub salt: Option<String>,
    /// The Project the bind made (`org/project`).
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub seed_run: Option<String>,
    #[serde(default)]
    pub seed_status: Option<String>,
    #[serde(default)]
    pub probe_run: Option<String>,
    #[serde(default)]
    pub probe_status: Option<String>,
    /// What each cached Step's fetcher said about its Cache.
    #[serde(default)]
    pub restores: Vec<Restore>,
    /// The Cache home URL the executor gave the cached Step's Pods
    /// ([`CACHE_URL_ENV`] in their spec). Seed and probe share a key, so
    /// they share it. `None` at one Depot replica, or when no Pod was read.
    #[serde(default)]
    pub home_url: Option<String>,
    /// The Runs that last saved a `cache_entries` row of the probe (the row
    /// is an upsert, so a probe that saved hides the seed's save).
    #[serde(default)]
    pub saved_by: Vec<String>,
    /// Why the probe did not run, or stopped.
    #[serde(default)]
    pub skipped: Option<String>,
}

/// One cached Step's restore, from its `scarab-workspace-init` log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restore {
    /// `seed` or `probe`.
    pub role: String,
    pub run: String,
    pub pod: String,
    /// `hit`, `miss`, `skipped`, `none` (no restore was offered: the key
    /// had no Cache, or the lookup failed), or `unread` (the fetcher's log
    /// could not be read).
    pub outcome: String,
    #[serde(default)]
    pub dir: Option<String>,
    /// The URL the fetcher restored from.
    #[serde(default)]
    pub from: Option<String>,
    /// The Depot ordinal of `from` when it is a home replica's URL
    /// (`…-workspace-<n>.…`), `None` for the service URL.
    #[serde(default)]
    pub home_ordinal: Option<u32>,
    #[serde(default)]
    pub ms: Option<u64>,
    /// The log line, verbatim.
    #[serde(default)]
    pub line: Option<String>,
}

impl CacheProbe {
    pub fn new(image: &str) -> CacheProbe {
        CacheProbe {
            pipeline_yaml: pipeline_yaml(image),
            ..Default::default()
        }
    }

    /// The same pipeline, nothing observed (a replay).
    pub fn replay(&self) -> CacheProbe {
        CacheProbe {
            pipeline_yaml: self.pipeline_yaml.clone(),
            ..Default::default()
        }
    }

    /// The probe's Runs, seed first.
    pub fn runs(&self) -> Vec<String> {
        self.seed_run
            .iter()
            .chain(&self.probe_run)
            .cloned()
            .collect()
    }
}

/// The probe's pipeline. `lock` writes the lock file from the salt (the
/// cache key folds its blob hash); `cached` restores `deps/` when a Cache
/// exists for that key, and fills it when not. 4 MiB, so a restore moves a
/// real blob.
pub fn pipeline_yaml(image: &str) -> String {
    format!(
        r#"# Written by scarab-chaos (crates/scarab-chaos/src/cache.rs): the Cache
# probe of `just test chaos`. Dispatched through the API; no push trigger.
on:
  api: {{}}
interface:
  inputs:
    - {{ name: salt, type: string }}
steps:
  - id: lock
    image: {image}
    command: ["sh", "-c", "printf 'chaos cache %s\n' \"$SCARAB_PARAM_SALT\" > /workspace/chaos.lock"]
  - id: {CACHED_STEP}
    image: {image}
    needs: [lock]
    cache: {{ dirs: [{CACHE_DIR}], key: [chaos.lock] }}
    command: ["sh", "-c", "if [ -f /workspace/{CACHE_DIR}/built ]; then echo CACHE-WARM; else echo CACHE-COLD; mkdir -p /workspace/{CACHE_DIR} && head -c 4194304 /dev/urandom > /workspace/{CACHE_DIR}/blob && date > /workspace/{CACHE_DIR}/built; fi"]
"#
    )
}

/// The restore lines of a `scarab-workspace-init` log (`scarab-wsfetch: cache
/// restore …`). Each becomes a [`Restore`] with `role`, `run` and `pod`
/// left empty.
pub fn parse_restores(log: &str) -> Vec<Restore> {
    log.lines()
        .filter_map(|line| {
            let rest = line.split("scarab-wsfetch: cache restore ").nth(1)?;
            let outcome = if rest.starts_with("hit ") {
                "hit"
            } else if rest.starts_with("miss ") {
                "miss"
            } else if rest.starts_with("skipped") {
                "skipped"
            } else {
                return None;
            };
            let field = |k: &str| {
                rest.split_whitespace()
                    .find_map(|w| w.strip_prefix(k))
                    .map(|v| v.trim_end_matches(':').to_string())
            };
            let from = field("from=");
            let ms = rest
                .split(" in ")
                .nth(1)
                .and_then(|t| {
                    t.strip_suffix(" ms")
                        .or_else(|| t.split_whitespace().next())
                })
                .and_then(|n| n.trim().parse().ok())
                .filter(|_| outcome == "hit");
            Some(Restore {
                outcome: outcome.into(),
                dir: field("dir="),
                home_ordinal: from.as_deref().and_then(home_ordinal),
                from,
                ms,
                line: Some(line.trim().to_string()),
                ..Default::default()
            })
        })
        .collect()
}

/// The ordinal of a home replica's URL (`http://<release>-workspace-<n>.<ns>.svc…`),
/// `None` for anything else (the service URL has no ordinal).
pub fn home_ordinal(url: &str) -> Option<u32> {
    let host = url.split("://").nth(1).unwrap_or(url);
    let host = host.split(['/', ':', '.']).next()?;
    let (_, n) = host.rsplit_once("-workspace-")?;
    n.parse().ok()
}

/// The informational lines the verdict prints for the probe. `fault_pods`
/// are the Pods the scenario's faults hit, to say when one was the Cache's
/// home.
pub fn summary_lines(p: &CacheProbe, fault_pods: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(why) = &p.skipped {
        out.push(format!("cache probe skipped: {why}"));
    }
    let restored: Vec<&Restore> = p
        .restores
        .iter()
        .filter(|r| r.outcome != "none" && r.outcome != "unread")
        .collect();
    let hits = restored.iter().filter(|r| r.outcome == "hit").count();
    let misses = restored.iter().filter(|r| r.outcome == "miss").count();
    let from_home = restored.iter().filter(|r| r.home_ordinal.is_some()).count();
    if p.seed_run.is_some() || p.probe_run.is_some() {
        out.push(format!(
            "cache: {hits} hit(s), {misses} miss(es), {from_home} restore(s) from a home replica \
             (C1 above judges the probe's)"
        ));
    }
    for (role, run, status) in [
        ("seed", &p.seed_run, &p.seed_status),
        ("probe", &p.probe_run, &p.probe_status),
    ] {
        let Some(run) = run else { continue };
        let mut line = format!(
            "cache {role} run {run} ({})",
            status.as_deref().unwrap_or("status unknown")
        );
        let mine: Vec<&Restore> = p.restores.iter().filter(|r| &r.run == run).collect();
        if mine.is_empty() {
            line += ": no cached Step Pod log read";
        }
        for r in mine {
            line += &match r.outcome.as_str() {
                "none" => ": no restore offered (no Cache for the key)".to_string(),
                "unread" => ": the fetcher's log could not be read".to_string(),
                o => format!(
                    ": {o} dir={} from {}{}",
                    r.dir.as_deref().unwrap_or("?"),
                    match (r.home_ordinal, &r.from) {
                        (Some(n), _) => format!("home replica {n}"),
                        (None, Some(f)) => format!("{f} (not a home)"),
                        (None, None) => "?".into(),
                    },
                    r.ms.map(|ms| format!(" in {ms} ms")).unwrap_or_default()
                ),
            };
            if let Some(n) = r.home_ordinal {
                if fault_pods
                    .iter()
                    .any(|pod| home_ordinal_of_pod(pod) == Some(n))
                {
                    line += " (the fault deleted this home replica)";
                }
            }
        }
        out.push(line);
    }
    if !p.saved_by.is_empty() {
        out.push(format!("cache entry last saved by run(s) {:?}", p.saved_by));
    }
    out
}

fn home_ordinal_of_pod(pod: &str) -> Option<u32> {
    pod.rsplit_once("-workspace-")?.1.parse().ok()
}

/// What [`c1`] concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum C1 {
    /// Checked; `Err` holds the finding's detail.
    Checked(Result<(), String>),
    /// Not exercised, and why.
    NotExercised(String),
}

/// Invariant C1: when the seed Run succeeded and no fault hit the Cache's
/// home replica, every restore of the probe Run is a hit from that home.
///
/// The home is [`CacheProbe::home_url`], else the home a probe restore
/// names. A `pause-node` fault stops the whole kind node, every replica
/// with it, so it always hits the home. Not exercised: no seed success, no
/// probe Run, no home (one Depot replica), a fault on the home, a fetcher
/// log that could not be read, or no probe Pod seen.
pub fn c1(p: &CacheProbe, faults: &[crate::scenario::Fault]) -> C1 {
    let not = |why: String| C1::NotExercised(why);
    if let Some(why) = &p.skipped {
        return not(format!("the probe did not run to the end: {why}"));
    }
    if p.seed_status.as_deref() != Some("succeeded") {
        return not(format!(
            "the seed Run did not succeed ({})",
            p.seed_status.as_deref().unwrap_or("no status")
        ));
    }
    let Some(probe_run) = &p.probe_run else {
        return not("no probe Run was dispatched".into());
    };
    let probe: Vec<&Restore> = p.restores.iter().filter(|r| &r.run == probe_run).collect();
    let home = p
        .home_url
        .as_deref()
        .and_then(home_ordinal)
        .or_else(|| probe.iter().find_map(|r| r.home_ordinal));
    let Some(home) = home else {
        return not("the probe's Pods name no Cache home (one Depot replica?)".into());
    };
    if let Some(f) = faults
        .iter()
        .find(|f| f.action == "pause-node" || home_ordinal_of_pod(&f.pod) == Some(home))
    {
        return not(format!(
            "the fault ({} {}) hit the Cache's home replica {home}",
            f.action, f.pod
        ));
    }
    if probe.is_empty() {
        return not(format!("no Pod of the probe Run {probe_run} was read"));
    }
    if probe.iter().any(|r| r.outcome == "unread") {
        return not("a probe Pod's fetcher log could not be read".into());
    }
    let wrong: Vec<String> = probe
        .iter()
        .filter(|r| !(r.outcome == "hit" && r.home_ordinal == Some(home)))
        .map(|r| {
            format!(
                "pod {} restore {} from {}",
                if r.pod.is_empty() { "?" } else { &r.pod },
                r.outcome,
                r.from.as_deref().unwrap_or("nowhere")
            )
        })
        .collect();
    if wrong.is_empty() {
        C1::Checked(Ok(()))
    } else {
        C1::Checked(Err(format!(
            "the seed Run {} saved the Cache to home replica {home} and no fault hit it, \
             but the probe Run {probe_run} did not restore it from there: {}",
            p.seed_run.as_deref().unwrap_or("?"),
            wrong.join("; ")
        )))
    }
}

// --- the live half ------------------------------------------------------------------

fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

/// Create the repo unless it exists (`deploy/kind/forgejo.sh` creates it in
/// every mode, so the repo count the e2e pagination case expects does not
/// depend on whether chaos ran), and write the pipeline on `main` unless it
/// already holds this text.
async fn write_pipeline(fj: &Forgejo, yaml: &str) -> Result<(), String> {
    let http = http()?;
    let auth = format!("token {}", fj.token);
    let repo_url = format!("{}/api/v1/repos/{}/{REPO}", fj.url, fj.owner);
    let got = http
        .get(&repo_url)
        .header("authorization", &auth)
        .send()
        .await
        .map_err(|e| format!("forgejo GET repo: {e}"))?;
    if got.status() == reqwest::StatusCode::NOT_FOUND {
        let resp = http
            .post(format!("{}/api/v1/user/repos", fj.url))
            .header("authorization", &auth)
            .json(&serde_json::json!({
                "name": REPO, "auto_init": true, "default_branch": "main", "private": false
            }))
            .send()
            .await
            .map_err(|e| format!("forgejo create repo: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "forgejo create repo: {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ));
        }
    } else if !got.status().is_success() {
        return Err(format!("forgejo GET repo: {}", got.status()));
    }
    let path = format!(".scarab/{PIPELINE}.yaml");
    let file_url = format!("{repo_url}/contents/{path}");
    let current: Option<(String, Vec<u8>)> = {
        let resp = http
            .get(format!("{file_url}?ref=main"))
            .header("authorization", &auth)
            .send()
            .await
            .map_err(|e| format!("forgejo GET {path}: {e}"))?;
        if resp.status().is_success() {
            let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            let b64: String = v["content"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let have = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap_or_default();
            v["sha"].as_str().map(|sha| (sha.to_string(), have))
        } else {
            None
        }
    };
    let content = base64::engine::general_purpose::STANDARD.encode(yaml);
    let req = match current {
        Some((_, have)) if have == yaml.as_bytes() => return Ok(()),
        Some((sha, _)) => http.put(&file_url).json(&serde_json::json!({
            "content": content, "sha": sha, "message": "scarab-chaos: the Cache probe", "branch": "main"
        })),
        None => http.post(&file_url).json(&serde_json::json!({
            "content": content, "message": "scarab-chaos: the Cache probe", "branch": "main"
        })),
    };
    let resp = req
        .header("authorization", &auth)
        .send()
        .await
        .map_err(|e| format!("forgejo write {path}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "forgejo write {path}: {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        ));
    }
    Ok(())
}

async fn wait_terminal(api: &Api, run: &str, timeout: Duration) -> Option<String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(s) = api.run_status(run).await {
            if TERMINAL_RUN_STATUSES.contains(&s.as_str()) {
                return Some(s);
            }
        }
        if std::time::Instant::now() > deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Before the workload: write the pipeline, bind the repo in the chaos
/// database, dispatch the seed Run and wait for it (up to
/// [`SEED_TIMEOUT`]). An `Err` means the probe cannot run; the scenario goes
/// on without it.
pub async fn seed(api: &Api, fj: &Forgejo, p: &mut CacheProbe, salt: String) -> Result<(), String> {
    write_pipeline(fj, &p.pipeline_yaml).await?;
    let conn = api
        .recreate_forgejo_connection(&fj.owner, &fj.server_url, &fj.token)
        .await?;
    let (org, project) = api.bind_repo(&conn, &fj.owner, REPO).await?;
    p.project = Some(format!("{org}/{project}"));
    p.salt = Some(salt.clone());
    let params = serde_json::json!({ "salt": salt });
    let run = api
        .dispatch(&org, &project, PIPELINE, "main", &params)
        .await?;
    p.seed_run = Some(run.clone());
    p.seed_status = wait_terminal(api, &run, SEED_TIMEOUT).await;
    if p.seed_status.is_none() {
        p.skipped = Some(format!(
            "the seed run {run} was not terminal {} s after dispatch; the probe was not dispatched",
            SEED_TIMEOUT.as_secs()
        ));
    }
    Ok(())
}

/// With the workload: dispatch the probe Run (only after a terminal seed).
pub async fn dispatch_probe(api: &Api, p: &mut CacheProbe) -> Result<(), String> {
    let (Some(project), Some(salt), Some(_)) = (&p.project, &p.salt, &p.seed_status) else {
        return Ok(());
    };
    let (org, project) = project.split_once('/').ok_or("bad project")?;
    let params = serde_json::json!({ "salt": salt });
    p.probe_run = Some(
        api.dispatch(org, project, PIPELINE, "main", &params)
            .await?,
    );
    Ok(())
}

/// After quiesce: each cached Step's fetcher log, and who last saved the
/// Cache row.
pub async fn observe(api: &Api, pods: &KubeApi<Pod>, pg: &sqlx::PgPool, p: &mut CacheProbe) {
    if let Some(run) = &p.probe_run {
        p.probe_status = api.run_status(run).await.ok();
    }
    let roles: Vec<(&str, String)> = [("seed", &p.seed_run), ("probe", &p.probe_run)]
        .into_iter()
        .filter_map(|(role, run)| run.clone().map(|r| (role, r)))
        .collect();
    let mut restores = Vec::new();
    for (role, run) in roles {
        let lp = ListParams::default()
            .labels(&format!("scarab.io/run={run},scarab.io/step={CACHED_STEP}"));
        let list = match pods.list(&lp).await {
            Ok(l) => l.items,
            Err(e) => {
                p.skipped
                    .get_or_insert(format!("could not list the cached Step's Pods: {e}"));
                continue;
            }
        };
        for pod in list {
            if p.home_url.is_none() {
                p.home_url = pod_cache_url(&pod);
            }
            let name = pod.metadata.name.unwrap_or_default();
            let log = pods
                .logs(
                    &name,
                    &LogParams {
                        container: Some(crate::pods::INIT_CONTAINER.into()),
                        ..Default::default()
                    },
                )
                .await;
            let mut seen = match &log {
                Ok(log) => parse_restores(log),
                Err(e) => vec![Restore {
                    outcome: "unread".into(),
                    line: Some(format!("could not read the fetcher's log: {e}")),
                    ..Default::default()
                }],
            };
            if seen.is_empty() {
                seen.push(Restore {
                    outcome: "none".into(),
                    ..Default::default()
                });
            }
            for mut r in seen {
                r.role = role.into();
                r.run = run.clone();
                r.pod = name.clone();
                restores.push(r);
            }
        }
    }
    p.restores = restores;
    let runs = p.runs();
    if !runs.is_empty() {
        p.saved_by = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT run_id FROM cache_entries WHERE run_id = ANY($1) ORDER BY run_id",
        )
        .bind(&runs)
        .fetch_all(pg)
        .await
        .unwrap_or_default();
    }
}

/// [`CACHE_URL_ENV`] from any container of `pod`.
fn pod_cache_url(pod: &Pod) -> Option<String> {
    let spec = pod.spec.as_ref()?;
    spec.init_containers
        .iter()
        .flatten()
        .chain(&spec.containers)
        .flat_map(|c| c.env.iter().flatten())
        .find(|e| e.name == CACHE_URL_ENV)
        .and_then(|e| e.value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pipeline the harness writes compiles, is dispatchable through the
    /// API, and gives the cached Step its `cache:` keyed on the lock file the
    /// Step before it writes.
    #[test]
    fn the_probe_pipeline_compiles_with_a_cache_and_an_api_trigger() {
        let ir = scarab_pipeline::compile_yaml(&pipeline_yaml("scarab-clone:dev"))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert!(ir.triggers.0.contains_key("api"));
        assert_eq!(ir.interface.inputs.len(), 1);
        assert_eq!(ir.interface.inputs[0].name, "salt");
        let cached = ir
            .steps
            .iter()
            .find(|s| s.id == CACHED_STEP)
            .expect("cached step");
        let cache = cached.cache.as_ref().expect("cache:");
        assert_eq!(cache.dirs, vec![CACHE_DIR.to_string()]);
        assert_eq!(cache.key, vec!["chaos.lock".to_string()]);
        assert_eq!(cached.needs.0, vec!["lock".to_string()]);
    }

    #[test]
    fn restore_lines_parse_and_a_home_url_names_its_ordinal() {
        let log = "\
scarab-wsfetch: fetched 1 root(s)
scarab-wsfetch: cache restore hit dir=deps root=abc123 from=http://scarab-workspace-1.scarab.svc in 85 ms
scarab-wsfetch: cache restore miss dir=deps root=abc123 from=http://scarab-workspace.scarab.svc:8080: 404 not found
scarab-wsfetch: cache restore skipped (source wins) dir=deps — the workspace already has content here
";
        let r = parse_restores(log);
        assert_eq!(r.len(), 3);
        assert_eq!(
            (
                r[0].outcome.as_str(),
                r[0].dir.as_deref(),
                r[0].home_ordinal,
                r[0].ms
            ),
            ("hit", Some("deps"), Some(1), Some(85))
        );
        assert_eq!(
            r[0].from.as_deref(),
            Some("http://scarab-workspace-1.scarab.svc")
        );
        assert_eq!(
            (r[1].outcome.as_str(), r[1].home_ordinal, r[1].ms),
            ("miss", None, None)
        );
        assert_eq!(
            r[1].from.as_deref(),
            Some("http://scarab-workspace.scarab.svc:8080")
        );
        assert_eq!(r[2].outcome, "skipped");
        assert_eq!(
            home_ordinal("http://scarab-workspace-12.ns.svc:8080/x"),
            Some(12)
        );
        assert_eq!(home_ordinal("http://scarab-workspace.ns.svc"), None);
    }

    #[test]
    fn the_summary_counts_hits_and_says_when_the_fault_hit_the_home() {
        let p = CacheProbe {
            seed_run: Some("r-seed".into()),
            seed_status: Some("succeeded".into()),
            probe_run: Some("r-probe".into()),
            probe_status: Some("succeeded".into()),
            restores: vec![
                Restore {
                    role: "seed".into(),
                    run: "r-seed".into(),
                    outcome: "none".into(),
                    ..Default::default()
                },
                Restore {
                    role: "probe".into(),
                    run: "r-probe".into(),
                    outcome: "hit".into(),
                    dir: Some("deps".into()),
                    from: Some("http://scarab-workspace-1.scarab.svc".into()),
                    home_ordinal: Some(1),
                    ms: Some(85),
                    ..Default::default()
                },
            ],
            saved_by: vec!["r-probe".into()],
            ..Default::default()
        };
        let lines = summary_lines(&p, &["scarab-workspace-1".into()]);
        assert!(
            lines[0].contains("1 hit(s), 0 miss(es), 1 restore(s) from a home replica"),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("seed run r-seed (succeeded): no restore offered"),
            "{lines:?}"
        );
        assert!(
            lines[2].contains(
                "hit dir=deps from home replica 1 in 85 ms (the fault deleted this home replica)"
            ),
            "{lines:?}"
        );
        assert!(lines[3].contains("r-probe"));
        // Nothing ran: only the reason.
        let skipped = CacheProbe {
            skipped: Some("SCARAB_CHAOS_FORGEJO_URL is not set".into()),
            ..Default::default()
        };
        assert_eq!(
            summary_lines(&skipped, &[]),
            vec!["cache probe skipped: SCARAB_CHAOS_FORGEJO_URL is not set".to_string()]
        );
    }

    /// A probe as `observe` records it: the seed saved, the probe restored
    /// `outcome` from `from`, the Pods' home is replica 1.
    fn recorded(outcome: &str, from: &str) -> CacheProbe {
        let restore = |role: &str, run: &str, outcome: &str, from: Option<&str>| Restore {
            role: role.into(),
            run: run.into(),
            pod: format!("{run}-cached"),
            outcome: outcome.into(),
            dir: Some("deps".into()),
            home_ordinal: from.and_then(home_ordinal),
            from: from.map(String::from),
            ..Default::default()
        };
        CacheProbe {
            seed_run: Some("r-seed".into()),
            seed_status: Some("succeeded".into()),
            probe_run: Some("r-probe".into()),
            probe_status: Some("succeeded".into()),
            home_url: Some("http://scarab-workspace-1.scarab.svc:8080".into()),
            restores: vec![
                restore("seed", "r-seed", "none", None),
                restore("probe", "r-probe", outcome, Some(from)),
            ],
            ..Default::default()
        }
    }

    fn fault(action: &str, pod: &str) -> crate::scenario::Fault {
        crate::scenario::Fault {
            action: action.into(),
            pod: pod.into(),
            ..Default::default()
        }
    }

    /// C1's checked side: the home was not hit, so a hit from it passes and a
    /// miss (or a restore from another replica) fails. A recorded delete of
    /// the OTHER replica, or a server kill, leaves the home untouched.
    #[test]
    fn c1_fails_a_probe_that_did_not_restore_from_an_unfaulted_home() {
        let home = "http://scarab-workspace-1.scarab.svc:8080";
        let other_replica = [fault("delete-pod", "scarab-workspace-0")];
        let server = [fault("kill-server", "scarab-7d9f-abcde")];
        assert_eq!(
            c1(&recorded("hit", home), &other_replica),
            C1::Checked(Ok(()))
        );
        assert_eq!(c1(&recorded("hit", home), &server), C1::Checked(Ok(())));
        assert_eq!(c1(&recorded("hit", home), &[]), C1::Checked(Ok(())));

        let miss = c1(&recorded("miss", home), &other_replica);
        assert!(
            matches!(&miss, C1::Checked(Err(d)) if d.contains("home replica 1") && d.contains("restore miss")),
            "{miss:?}"
        );
        let elsewhere = c1(
            &recorded("hit", "http://scarab-workspace-0.scarab.svc:8080"),
            &[],
        );
        assert!(matches!(elsewhere, C1::Checked(Err(_))), "{elsewhere:?}");
        let none = c1(&recorded("none", home), &[]);
        assert!(
            matches!(none, C1::Checked(Err(_))),
            "no restore offered after a saved seed: {none:?}"
        );
    }

    /// C1's not-exercised side: a failed seed, a fault on the home (a Pod
    /// delete of it, or a node pause, which stops every replica), one Depot
    /// replica (no home), or an unread fetcher log.
    #[test]
    fn c1_is_not_exercised_when_the_home_was_hit_or_the_seed_failed() {
        let home = "http://scarab-workspace-1.scarab.svc:8080";
        let ne = |r: C1| matches!(r, C1::NotExercised(_));
        assert!(ne(c1(
            &recorded("miss", home),
            &[fault("delete-pod", "scarab-workspace-1")]
        )));
        assert!(ne(c1(
            &recorded("miss", home),
            &[fault("pause-node", "scarab-dev-control-plane")]
        )));

        let mut seed_failed = recorded("miss", home);
        seed_failed.seed_status = Some("failed".into());
        assert!(ne(c1(&seed_failed, &[])));

        let mut one_replica = recorded("miss", "http://scarab-workspace.scarab.svc:8080");
        one_replica.home_url = None;
        assert!(ne(c1(&one_replica, &[])));

        let mut unread = recorded("hit", home);
        unread.restores[1].outcome = "unread".into();
        assert!(ne(c1(&unread, &[])));

        let mut no_probe = recorded("hit", home);
        no_probe.probe_run = None;
        assert!(ne(c1(&no_probe, &[])));
    }

    /// The token stays out of `Debug` output.
    #[test]
    fn forgejo_debug_redacts_the_token() {
        let fj = Forgejo {
            url: "http://127.0.0.1:3000".into(),
            server_url: "http://forgejo".into(),
            token: "s3cr3t-token".into(),
            owner: "scarab".into(),
        };
        let shown = format!("{fj:?}");
        assert!(
            !shown.contains("s3cr3t-token") && shown.contains("<redacted>"),
            "{shown}"
        );
    }
}
