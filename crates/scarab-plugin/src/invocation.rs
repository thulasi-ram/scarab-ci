//! The invocation protocol on the broker (ADR-0074 §5): how a Plugin pulls
//! the invocations of its PluginActions and answers them. These are the wire
//! shapes; the broker serves them under `/_scarab/plugin/v1`.
//!
//! - `GET …/invocations?wait=<secs>&limit=<n>` long-polls for invocations of
//!   the caller's installation and leases each one it returns. An invocation
//!   not answered before `lease_expires_at` is offered again, to any replica.
//! - `POST …/invocations/{id}/answer` with an [`InvocationAnswer`]. The answer
//!   is [`AnswerResponse::Stale`] when the invocation's fence is no longer
//!   current (the Step was cancelled, retried or superseded): stop working on
//!   it. A Plugin keeps no state between answers; it decides each one from
//!   the inputs and the vendor's current state, and must accept a repeat
//!   (dispatch is at-least-once, ADR-0072 §4).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The longest a long poll waits for an invocation, in seconds.
pub const MAX_POLL_WAIT_SECS: u64 = 30;
/// The most invocations one poll returns.
pub const MAX_INVOCATIONS_PER_POLL: u32 = 32;

/// The Attempt an invocation belongs to (ADR-0021). Use it as the
/// idempotency key for anything the action does.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationFence {
    pub run: String,
    pub step: String,
    pub attempt: String,
}

/// One leased invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferedInvocation {
    /// Answer it at `…/invocations/{id}/answer`.
    pub id: String,
    pub fence: InvocationFence,
    /// The action's name, as the manifest declares it.
    pub action: String,
    /// The Step's `with`, resolved when its Attempt started.
    pub inputs: BTreeMap<String, serde_json::Value>,
    /// Unix ms; unanswered by then, the invocation is offered again.
    pub lease_expires_at: i64,
    /// The label of this invocation's last `pending` answer, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_label: Option<String>,
}

/// `GET …/invocations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvocationsPage {
    /// Empty when the wait ended with nothing to offer: poll again.
    pub invocations: Vec<OfferedInvocation>,
}

/// `POST …/invocations/{id}/answer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum InvocationAnswer {
    /// Not decided yet; `label` says where it stands ("sync retry 2/5").
    /// Offered again after the action's poll interval.
    Pending { label: String },
    /// Done; `results` become the Step's results (`${{ outputs.<step>.* }}`),
    /// at most 64 KiB as JSON.
    Succeeded {
        #[serde(default)]
        results: BTreeMap<String, serde_json::Value>,
    },
    /// Done, and the Step fails for `reason`.
    Failed { reason: String },
}

/// What the broker did with an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AnswerResponse {
    Accepted,
    /// The fence is no longer current; nothing was recorded.
    Stale,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_answer_wire_shape_is_tagged_by_outcome() {
        assert_eq!(
            serde_json::to_value(InvocationAnswer::Pending {
                label: "sync 1/5".into()
            })
            .unwrap(),
            json!({ "outcome": "pending", "label": "sync 1/5" })
        );
        assert_eq!(
            serde_json::to_value(AnswerResponse::Stale).unwrap(),
            json!({ "outcome": "stale" })
        );
        let failed: InvocationAnswer =
            serde_json::from_value(json!({ "outcome": "failed", "reason": "hook failed" }))
                .unwrap();
        assert_eq!(
            failed,
            InvocationAnswer::Failed {
                reason: "hook failed".into()
            }
        );
    }
}
