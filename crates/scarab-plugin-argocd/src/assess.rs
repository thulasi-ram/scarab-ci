//! What an Application's state means as a DeploymentStatus (ADR-0070 §5,
//! §11): a pure function of the Application, so every rule is a table test.
//!
//! The contract's two hard rules, as Argo expresses them:
//!
//! - **`failed` only when Argo has given up.** While retries remain, Argo
//!   keeps a failed attempt's operation `Running` and counts `retryCount`;
//!   the phase becomes `Failed`/`Error` only when retries are exhausted or
//!   the sync was terminated (a sync timeout terminates it). So a finished
//!   `Failed`/`Error` operation on the current revision is `failed`, and
//!   everything before it is `in_progress` with the reason as the label.
//!   Health adds one more give-up: a workload past its progress deadline
//!   (owner default 6) — Kubernetes itself has stopped waiting — and any
//!   Degraded Argo Rollout, which the Rollouts controller only reports once
//!   it has stopped (aborted, timed out, or an invalid spec).
//! - **`live` only when the target version is serving**: synced (live
//!   matches the target the version was computed from) and healthy, with no
//!   operation running.
//!
//! Vendor states that are not statuses become labels (ADR-0070 §6).

use serde::{Deserialize, Serialize};

use crate::argo::{Application, OperationState, ResourceResult};

/// The statuses a source may assert, as the broker spells them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    InProgress,
    Live,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::InProgress => "in_progress",
            Status::Live => "live",
            Status::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "in_progress" => Some(Status::InProgress),
            "live" => Some(Status::Live),
            "failed" => Some(Status::Failed),
            _ => None,
        }
    }
}

/// The longest label, in characters.
pub const MAX_LABEL_CHARS: usize = 512;

/// Unicode format characters a label never keeps (they change how text
/// reads without being seen).
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// A one-line label: Argo's messages are often multi-line. Whitespace and
/// control characters collapse to one space, invisible format characters go,
/// and the result is cut to [`MAX_LABEL_CHARS`].
pub fn clean_label(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_LABEL_CHARS * 4));
    let mut pending_space = false;
    for c in raw.chars() {
        if is_invisible_format(c) {
            continue;
        }
        if c.is_whitespace() || c.is_control() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    if out.chars().count() > MAX_LABEL_CHARS {
        out = out.chars().take(MAX_LABEL_CHARS - 1).collect();
        out.push('…');
    }
    out
}

/// A status and the label that explains it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub status: Status,
    pub label: String,
}

fn assessment(status: Status, label: impl Into<String>) -> Assessment {
    Assessment {
        status,
        label: clean_label(&label.into()),
    }
}

/// `"<prefix>: <message>"`, or the prefix alone without a message.
fn with_message(prefix: &str, message: Option<&str>) -> String {
    match message.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => format!("{prefix}: {m}"),
        None => prefix.to_string(),
    }
}

/// The Application's status and label right now.
pub fn assess(app: &Application) -> Assessment {
    if let Some(op) = app.status.operation_state.as_ref() {
        match op.phase.as_str() {
            "Running" => return running(app, op),
            "Terminating" => {
                return assessment(
                    Status::InProgress,
                    with_message("terminating", op.message.as_deref()),
                )
            }
            "Failed" | "Error" if operation_is_current(app, op) => return gave_up(op),
            // Succeeded, or a finished operation on another revision: the
            // sync and health state below say where things are.
            _ => {}
        }
    }
    match app.status.sync.status.as_str() {
        "OutOfSync" => {
            return assessment(
                Status::InProgress,
                if app.automated() {
                    "awaiting auto-sync"
                } else {
                    "awaiting sync"
                },
            )
        }
        "Synced" => {}
        _ => return assessment(Status::InProgress, "sync status unknown"),
    }
    let health = &app.status.health;
    let message = health_message(app);
    match health.status.as_str() {
        "Healthy" => assessment(Status::Live, "synced, healthy"),
        "Progressing" => assessment(
            Status::InProgress,
            with_message("progressing", message.as_deref()),
        ),
        "Suspended" => assessment(
            Status::InProgress,
            with_message("paused", message.as_deref()),
        ),
        "Missing" => assessment(
            Status::InProgress,
            with_message("missing", message.as_deref()),
        ),
        "Degraded" => match given_up_health(app) {
            Some(m) => assessment(Status::Failed, m),
            None => assessment(
                Status::InProgress,
                with_message("degraded", message.as_deref()),
            ),
        },
        _ => assessment(
            Status::InProgress,
            with_message("health unknown", message.as_deref()),
        ),
    }
}

/// An operation in flight: why it is still going.
fn running(app: &Application, op: &OperationState) -> Assessment {
    let retries = op.retry_count.unwrap_or(0);
    if retries > 0 {
        let limit = op
            .operation
            .retry
            .as_ref()
            .and_then(|r| r.limit)
            .or_else(|| {
                app.spec
                    .sync_policy
                    .as_ref()
                    .and_then(|p| p.retry.as_ref())
                    .and_then(|r| r.limit)
            });
        let prefix = match limit {
            Some(l) if l > 0 => format!("retry {retries}/{l}"),
            _ => format!("retry {retries}"),
        };
        return assessment(
            Status::InProgress,
            with_message(&prefix, op.message.as_deref()),
        );
    }
    if let Some(hook) = hooks(op).find(|h| h.hook_phase.as_deref() == Some("Running")) {
        return assessment(Status::InProgress, format!("hook {} running", hook.name));
    }
    // Argo's API carries no sync-wave field; its message names what the
    // operation waits on ("waiting for completion of hook batch/Job/x").
    assessment(
        Status::InProgress,
        with_message("syncing", op.message.as_deref()),
    )
}

/// A finished, failed operation: Argo gave up. The first failed hook is the
/// most specific reason; else the operation's own message.
fn gave_up(op: &OperationState) -> Assessment {
    let failed_hook = hooks(op).find(|h| {
        matches!(h.hook_phase.as_deref(), Some("Failed" | "Error"))
            || h.status.as_deref() == Some("SyncFailed")
    });
    let label = match failed_hook {
        Some(h) => with_message(&format!("hook {} failed", h.name), h.message.as_deref()),
        None => with_message("sync failed", op.message.as_deref()),
    };
    assessment(Status::Failed, label)
}

fn hooks(op: &OperationState) -> impl Iterator<Item = &ResourceResult> {
    op.sync_result
        .iter()
        .flat_map(|r| r.resources.iter())
        .filter(|r| r.hook_type.as_deref().is_some_and(|t| !t.is_empty()))
}

/// Whether a finished operation synced the revision the Application now
/// targets. One that synced an older revision says nothing about the
/// current version — its version was computed from the newer manifests —
/// so it must not fail it. Without revision fields to compare, it is taken
/// as current.
fn operation_is_current(app: &Application, op: &OperationState) -> bool {
    let (op_revision, op_revisions) = match (&op.sync_result, &op.operation.sync) {
        (Some(r), _) if r.revision.is_some() || !r.revisions.is_empty() => {
            (r.revision.as_deref(), r.revisions.as_slice())
        }
        (_, Some(s)) => (s.revision.as_deref(), s.revisions.as_slice()),
        _ => (None, &[][..]),
    };
    let sync = &app.status.sync;
    if !op_revisions.is_empty() && !sync.revisions.is_empty() {
        return op_revisions == sync.revisions.as_slice();
    }
    match (op_revision, sync.revision.as_deref()) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// The Application's health message, or the first resource's message for
/// the same health status (newer Argo versions keep it per resource only).
fn health_message(app: &Application) -> Option<String> {
    let health = &app.status.health;
    if let Some(m) = health.message.as_deref().filter(|m| !m.trim().is_empty()) {
        return Some(m.to_string());
    }
    app.status
        .resources
        .iter()
        .filter_map(|r| r.health.as_ref())
        .find(|h| h.status == health.status && h.message.is_some())
        .and_then(|h| h.message.clone())
}

/// The label for a Degraded Application whose rollout has been given up,
/// if it has: a degraded `argoproj.io/Rollout`, else a progress-deadline
/// message on the Application or a degraded resource. Any other Degraded
/// (a crash-looping Pod, a failing probe) may still recover: `in_progress`.
fn given_up_health(app: &Application) -> Option<String> {
    if let Some((name, message)) = degraded_rollout(app) {
        return Some(match message {
            Some(m) => with_message("degraded", Some(&m)),
            None => format!("degraded: rollout {name} gave up"),
        });
    }
    progress_deadline_message(app).map(|m| with_message("degraded", Some(&m)))
}

/// The first degraded Argo Rollout among the Application's resources, and
/// its health message.
///
/// A Rollout is Degraded only once its controller has stopped: Argo CD's
/// health check for `argoproj.io/Rollout` maps exactly three conditions to
/// Degraded — `InvalidSpec`, `RolloutAborted` (a canary whose analysis
/// failed, or a manual abort) and `ProgressDeadlineExceeded` — each with the
/// condition's own message; newer Rollouts give the phase as
/// `"<Reason>: <message>"` and Argo CD copies it. None of them recovers
/// without a new rollout or a human, so the kind alone decides, not the
/// message's wording. Verified 2026-10-22 against
/// https://github.com/argoproj/argo-cd/blob/v2.13.0/resource_customizations/argoproj.io/Rollout/health.lua
/// and argo-rollouts v1.7.2 `utils/rollout/rolloututil.go`
/// (`CalculateRolloutPhase`).
fn degraded_rollout(app: &Application) -> Option<(String, Option<String>)> {
    app.status
        .resources
        .iter()
        .filter(|r| r.group == "argoproj.io" && r.kind == "Rollout")
        .find_map(|r| {
            let h = r.health.as_ref().filter(|h| h.status == "Degraded")?;
            let message = h.message.clone().filter(|m| !m.trim().is_empty());
            Some((r.name.clone(), message))
        })
}

/// A message saying a workload exceeded its progress deadline, if any —
/// Kubernetes' own give-up for a rollout (`ProgressDeadlineExceeded`):
/// a Deployment's `exceeded its progress deadline`, or a Rollout's
/// `ReplicaSet "x" has timed out progressing.` (argo-rollouts
/// `ReplicaSetTimeOutMessage`/`RolloutTimeOutMessage`) where Argo CD put it
/// on the Application rather than on the resource.
fn progress_deadline_message(app: &Application) -> Option<String> {
    let says_deadline = |m: &str| {
        let m = m.to_ascii_lowercase();
        m.contains("progress deadline") || m.contains("timed out progress")
    };
    if let Some(m) = app
        .status
        .health
        .message
        .as_deref()
        .filter(|m| says_deadline(m))
    {
        return Some(m.to_string());
    }
    app.status
        .resources
        .iter()
        .filter_map(|r| r.health.as_ref())
        .filter(|h| h.status == "Degraded")
        .filter_map(|h| h.message.as_deref())
        .find(|m| says_deadline(m))
        .map(str::to_string)
}
