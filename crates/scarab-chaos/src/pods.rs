//! The Pod watcher (ADR-0077 §2): a kube watch on the release namespace that
//! writes one JSONL line per change to a step Pod or a Depot Pod, and keeps
//! the latest view of each for the nemesis.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, WatchEvent, WatchParams};
use serde::{Deserialize, Serialize};

use crate::time::{now_ms, parse_rfc3339_ms};

pub const STEP_CONTAINER: &str = "step";
pub const INIT_CONTAINER: &str = "scarab-workspace-init";
pub const EGRESS_CONTAINER: &str = "scarab-workspace-egress";
pub const ROOT_ANNOTATION: &str = "scarab.io/workspace-root";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContainerView {
    pub name: String,
    pub init: bool,
    /// `waiting`, `running` or `terminated`.
    pub state: String,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub exit_code: Option<i64>,
    pub restarts: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PodView {
    pub uid: String,
    pub name: String,
    pub run: Option<String>,
    pub step: Option<String>,
    pub attempt: Option<String>,
    /// `app.kubernetes.io/component` (`workspace` on a Depot Pod).
    pub component: Option<String>,
    pub phase: String,
    pub ready: bool,
    pub deleting: bool,
    pub has_root: bool,
    pub containers: Vec<ContainerView>,
}

impl PodView {
    pub fn from_json(p: &serde_json::Value) -> PodView {
        let md = &p["metadata"];
        let label = |k: &str| md["labels"][k].as_str().map(str::to_string);
        let mut containers = Vec::new();
        for (key, init) in [("initContainerStatuses", true), ("containerStatuses", false)] {
            for c in p["status"][key].as_array().into_iter().flatten() {
                let (state, body) = c["state"]
                    .as_object()
                    .and_then(|m| m.iter().next())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .unwrap_or_else(|| ("waiting".into(), serde_json::Value::Null));
                containers.push(ContainerView {
                    name: c["name"].as_str().unwrap_or_default().to_string(),
                    init,
                    state,
                    started_at_ms: body["startedAt"].as_str().and_then(parse_rfc3339_ms),
                    finished_at_ms: body["finishedAt"].as_str().and_then(parse_rfc3339_ms),
                    exit_code: body["exitCode"].as_i64(),
                    restarts: c["restartCount"].as_i64().unwrap_or(0),
                });
            }
        }
        let ready = p["status"]["conditions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["type"] == "Ready" && c["status"] == "True");
        PodView {
            uid: md["uid"].as_str().unwrap_or_default().to_string(),
            name: md["name"].as_str().unwrap_or_default().to_string(),
            run: label("scarab.io/run"),
            step: label("scarab.io/step"),
            attempt: label("scarab.io/attempt"),
            component: label("app.kubernetes.io/component"),
            phase: p["status"]["phase"].as_str().unwrap_or("Unknown").to_string(),
            ready,
            deleting: !md["deletionTimestamp"].is_null(),
            has_root: md["annotations"][ROOT_ANNOTATION].is_string(),
            containers,
        }
    }

    pub fn container(&self, name: &str) -> Option<&ContainerView> {
        self.containers.iter().find(|c| c.name == name)
    }

    pub fn is_depot(&self) -> bool {
        self.component.as_deref() == Some("workspace") && self.run.is_none()
    }
}

/// The drain is owed or running: the step container exited 0, the egress
/// helper still runs, and the Pod has no drain-record root annotation yet.
pub fn drain_in_flight(p: &PodView) -> bool {
    let step_done_ok = p
        .container(STEP_CONTAINER)
        .is_some_and(|c| c.state == "terminated" && c.exit_code == Some(0));
    let egress_running = p.container(EGRESS_CONTAINER).is_some_and(|c| c.state == "running");
    step_done_ok && egress_running && !p.has_root && !p.deleting
}

/// The fetcher is pulling the step's inputs.
pub fn feed_in_flight(p: &PodView) -> bool {
    p.container(INIT_CONTAINER).is_some_and(|c| c.state == "running") && !p.deleting
}

/// One line of `pods.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PodRecord {
    pub at_ms: i64,
    /// `added`, `modified` or `deleted`.
    pub event: String,
    pub pod: PodView,
}

/// Read a `pods.jsonl` back.
pub fn read_records(path: &std::path::Path) -> Result<Vec<PodRecord>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| format!("{}: {e}: {l}", path.display())))
        .collect()
}

/// The latest view of every recorded Pod, by UID.
pub type Views = Arc<Mutex<HashMap<String, PodView>>>;

fn interesting(v: &PodView) -> bool {
    v.run.is_some() || v.is_depot()
}

/// Watch `ns` until `stop` flips, appending a line to `out` whenever a step
/// or Depot Pod changes. A watch that ends or errors is re-listed and
/// resumed; a re-list records every Pod again as `added`.
pub async fn watch(
    pods: Api<Pod>,
    out: std::path::PathBuf,
    views: Views,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out)
        .map_err(|e| format!("{}: {e}", out.display()))?;
    let mut write = |event: &str, view: &PodView| -> Result<(), String> {
        let rec = PodRecord {
            at_ms: now_ms(),
            event: event.to_string(),
            pod: view.clone(),
        };
        let line = serde_json::to_string(&rec).map_err(|e| e.to_string())?;
        writeln!(file, "{line}").map_err(|e| e.to_string())
    };
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        let list = match pods.list(&ListParams::default()).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("scarab-chaos: pod list failed ({e}); retrying");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        let rv = list.metadata.resource_version.clone().unwrap_or_default();
        {
            let mut seen = HashMap::new();
            for p in &list.items {
                let v = PodView::from_json(&serde_json::to_value(p).map_err(|e| e.to_string())?);
                if interesting(&v) {
                    seen.insert(v.uid.clone(), v);
                }
            }
            let mut map = views.lock().expect("views lock");
            for (uid, v) in &seen {
                if map.get(uid) != Some(v) {
                    write("added", v)?;
                }
            }
            for (uid, v) in map.iter() {
                if !seen.contains_key(uid) {
                    write("deleted", v)?;
                }
            }
            *map = seen;
        }
        let stream = match pods.watch(&WatchParams::default().timeout(240), &rv).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("scarab-chaos: pod watch failed to start ({e}); re-listing");
                continue;
            }
        };
        futures::pin_mut!(stream);
        loop {
            let ev = tokio::select! {
                ev = stream.next() => ev,
                _ = stop.changed() => return Ok(()),
            };
            let Some(ev) = ev else { break };
            let (kind, pod) = match ev {
                Ok(WatchEvent::Added(p)) => ("added", p),
                Ok(WatchEvent::Modified(p)) => ("modified", p),
                Ok(WatchEvent::Deleted(p)) => ("deleted", p),
                Ok(WatchEvent::Bookmark(_)) => continue,
                Ok(WatchEvent::Error(s)) => {
                    eprintln!("scarab-chaos: pod watch error ({s:?}); re-listing");
                    break;
                }
                Err(e) => {
                    eprintln!("scarab-chaos: pod watch stream error ({e}); re-listing");
                    break;
                }
            };
            let v = PodView::from_json(&serde_json::to_value(&pod).map_err(|e| e.to_string())?);
            if !interesting(&v) {
                continue;
            }
            let mut map = views.lock().expect("views lock");
            if kind == "deleted" {
                map.remove(&v.uid);
                write(kind, &v)?;
            } else if map.get(&v.uid) != Some(&v) {
                map.insert(v.uid.clone(), v.clone());
                write(kind, &v)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(step_state: &str, exit: Option<i64>, egress: &str, root: bool) -> PodView {
        let mut annotations = serde_json::Map::new();
        if root {
            annotations.insert(ROOT_ANNOTATION.into(), "abc".into());
        }
        let state = |s: &str, exit: Option<i64>| match s {
            "terminated" => serde_json::json!({"terminated": {"exitCode": exit, "startedAt": "2026-11-01T00:00:00Z", "finishedAt": "2026-11-01T00:00:05Z"}}),
            "running" => serde_json::json!({"running": {"startedAt": "2026-11-01T00:00:00Z"}}),
            _ => serde_json::json!({"waiting": {"reason": "PodInitializing"}}),
        };
        PodView::from_json(&serde_json::json!({
            "metadata": {"uid": "u1", "name": "scarab-produce-a1-x",
                "labels": {"scarab.io/run": "r", "scarab.io/step": "produce", "scarab.io/attempt": "a1"},
                "annotations": annotations},
            "status": {"phase": "Running",
                "initContainerStatuses": [
                    {"name": EGRESS_CONTAINER, "state": state(egress, None), "restartCount": 0},
                    {"name": INIT_CONTAINER, "state": state("running", None), "restartCount": 0}],
                "containerStatuses": [{"name": STEP_CONTAINER, "state": state(step_state, exit), "restartCount": 0}]}
        }))
    }

    #[test]
    fn reads_the_fields_the_triggers_need() {
        let p = pod("terminated", Some(0), "running", false);
        assert_eq!(p.attempt.as_deref(), Some("a1"));
        let c = p.container(STEP_CONTAINER).unwrap();
        assert_eq!((c.state.as_str(), c.exit_code), ("terminated", Some(0)));
        assert_eq!(c.finished_at_ms.unwrap() - c.started_at_ms.unwrap(), 5_000);
        assert!(p.container(INIT_CONTAINER).unwrap().init);
    }

    #[test]
    fn the_drain_window_is_step_exited_zero_and_no_root_yet() {
        assert!(drain_in_flight(&pod("terminated", Some(0), "running", false)));
        assert!(!drain_in_flight(&pod("running", None, "running", false)));
        assert!(!drain_in_flight(&pod("terminated", Some(1), "running", false)));
        assert!(!drain_in_flight(&pod("terminated", Some(0), "running", true)));
        assert!(!drain_in_flight(&pod("terminated", Some(0), "terminated", false)));
    }

    #[test]
    fn the_feed_window_is_the_fetcher_running() {
        assert!(feed_in_flight(&pod("waiting", None, "waiting", false)));
    }
}
