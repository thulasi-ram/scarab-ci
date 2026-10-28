//! `plugin_action` Steps (ADR-0072 §4, ADR-0074 §5): a Step whose Attempt
//! waits on a plugin's answer instead of a Pod.
//!
//! While the Attempt is `Running`, its [`PluginInvocation`] is offered to the
//! plugin installation it names. The plugin pulls it from the broker with a
//! long poll, which leases it for [`PLUGIN_INVOCATION_LEASE_MS`]; an
//! invocation not answered within its lease is offered again, so several
//! replicas of one plugin can pull from the same queue. The plugin answers
//! [`PluginAnswer::Pending`] (with a label; offered again after the action's
//! poll interval), [`PluginAnswer::Succeeded`] (with results) or
//! [`PluginAnswer::Failed`] (with a reason).
//!
//! Nothing here lives in memory: the queue is the invocation rows of Running
//! Attempts ([`crate::Db::lease_plugin_invocations`]), so a control-plane
//! restart loses no invocation. Every invocation carries its Attempt's fence
//! ({run, step, attempt}); an answer for an Attempt that is no longer the
//! Step's current one (cancelled, retried, superseded) is dropped.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{AttemptId, RunId, StepId, Timestamp};

/// How long an offered invocation stays leased to the replica that took it.
/// An invocation not answered within it is offered again.
pub const PLUGIN_INVOCATION_LEASE_MS: i64 = 60_000;

/// The longest `pending` label kept, in characters; a longer one is cut.
pub const MAX_PLUGIN_LABEL_CHARS: usize = 200;

/// The longest `failed` reason kept, in characters; a longer one is cut.
pub const MAX_PLUGIN_REASON_CHARS: usize = 2000;

/// The cap on a Step's results, encoded as JSON (ADR-0041): results are
/// small values, not blobs. The results-ingest endpoint and a plugin's
/// `succeeded` answer both apply it.
pub const MAX_STEP_RESULTS_BYTES: usize = 64 * 1024;

/// One offered invocation of a PluginAction: a durable row beside the
/// `plugin_action` Step's Attempt, written when the Attempt starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInvocation {
    /// [`plugin_invocation_id`] of the fence.
    pub id: String,
    pub run: RunId,
    pub step: StepId,
    pub attempt: AttemptId,
    /// The installation, by its name in `org` (never reused there).
    pub org: String,
    pub installation: String,
    pub action: String,
    /// The action's inputs, interpolated when the Attempt started.
    pub inputs: BTreeMap<String, serde_json::Value>,
    pub poll_interval_ms: i64,
    pub created_at: Timestamp,
    /// Not offered before this instant (the poll interval after a `pending`).
    pub next_offer_at: Timestamp,
    /// Leased to a plugin replica until this instant.
    pub lease_until: Option<Timestamp>,
    /// The latest `pending` label. Overwritten, not journaled.
    pub label: Option<String>,
    /// When the plugin last answered this invocation.
    pub last_answer_at: Option<Timestamp>,
}

/// The invocation id of a fence: deterministic, so recording an invocation
/// again after a crash is a no-op.
pub fn plugin_invocation_id(run: &RunId, step: &StepId, attempt: &AttemptId) -> String {
    let mut h = Sha256::new();
    for part in ["plugin-invocation", &run.0, &step.0, &attempt.0] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    h.finalize()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A plugin's answer to one invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginAnswer {
    /// Not decided yet. Core stores `label` on the Attempt and offers the
    /// invocation again after the action's poll interval.
    Pending { label: String },
    /// The Step succeeded; `results` become its results (ADR-0041).
    Succeeded {
        #[serde(default)]
        results: BTreeMap<String, serde_json::Value>,
    },
    /// The Step failed, for `reason`.
    Failed { reason: String },
}

/// What core did with an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerOutcome {
    /// Recorded: a label stored, or the Attempt closed.
    Accepted,
    /// Dropped: the invocation's fence is no longer current (the Attempt
    /// was cancelled, retried or superseded, or already finished).
    Stale,
}

/// Why an answer was refused before anything was recorded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnswerRefused {
    #[error("results exceed {MAX_STEP_RESULTS_BYTES} bytes")]
    ResultsTooLarge,
    #[error("result name `{0}` must be a non-empty file stem (no `/`, no whitespace)")]
    BadResultName(String),
}

/// Check `succeeded` results against the limits every Step's results obey.
pub fn check_results(results: &BTreeMap<String, serde_json::Value>) -> Result<(), AnswerRefused> {
    for name in results.keys() {
        if name.trim().is_empty() || name.contains('/') || name.chars().any(char::is_whitespace) {
            return Err(AnswerRefused::BadResultName(name.clone()));
        }
    }
    let encoded = serde_json::to_vec(results).unwrap_or_default();
    if encoded.len() > MAX_STEP_RESULTS_BYTES {
        return Err(AnswerRefused::ResultsTooLarge);
    }
    Ok(())
}

/// `s` cut to at most `max` characters.
pub fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_invocation_id_is_a_function_of_the_fence() {
        let id = |a: &str| {
            plugin_invocation_id(
                &RunId("r".into()),
                &StepId("live".into()),
                &AttemptId(a.into()),
            )
        };
        assert_eq!(id("a1"), id("a1"));
        assert_ne!(id("a1"), id("a2"));
        assert_eq!(id("a1").len(), 32);
    }

    #[test]
    fn answers_parse_from_their_wire_shape() {
        let pending: PluginAnswer =
            serde_json::from_value(json!({ "outcome": "pending", "label": "sync 1/5" })).unwrap();
        assert_eq!(
            pending,
            PluginAnswer::Pending {
                label: "sync 1/5".into()
            }
        );
        let ok: PluginAnswer = serde_json::from_value(
            json!({ "outcome": "succeeded", "results": { "images": ["a"] } }),
        )
        .unwrap();
        assert!(matches!(ok, PluginAnswer::Succeeded { results } if results.len() == 1));
        assert!(serde_json::from_value::<PluginAnswer>(
            json!({ "outcome": "failed", "reason": "x", "extra": 1 })
        )
        .is_err());
    }

    #[test]
    fn results_over_the_cap_or_with_a_bad_name_are_refused() {
        let big = BTreeMap::from([("r".to_string(), json!("x".repeat(MAX_STEP_RESULTS_BYTES)))]);
        assert_eq!(check_results(&big), Err(AnswerRefused::ResultsTooLarge));
        let bad = BTreeMap::from([("a/b".to_string(), json!(1))]);
        assert!(matches!(
            check_results(&bad),
            Err(AnswerRefused::BadResultName(_))
        ));
        assert!(check_results(&BTreeMap::from([("ok".to_string(), json!(1))])).is_ok());
    }
}
