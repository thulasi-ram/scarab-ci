//! An org's **Fork Run approval** setting (ADR-0086 §9): whether a Run of
//! fork code waits, before any of its Steps starts, at a `manual` gate that
//! someone with write on the repository releases. Off for an org that never
//! set it. Stored per org, changed only by an org admin from a browser.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::IdentityError;

/// The `name` of the `origin` link of the gate the setting adds to a Run
/// (`kind: fork_run_approval_gate`).
pub const FORK_RUN_APPROVAL_SETTING_NAME: &str = "fork_run_approval";

/// One org's Fork Run approval setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkRunApproval {
    /// A Run of fork code created while this is on gets the gate.
    pub enabled: bool,
}

impl ForkRunApproval {
    /// Changing from `self` to `next` turns the setting off: a Run of fork
    /// code would start with no approval. Loosening needs a step-up.
    pub fn loosened_by(self, next: ForkRunApproval) -> bool {
        self.enabled && !next.enabled
    }
}

/// Where each org's [`ForkRunApproval`] is kept.
#[async_trait]
pub trait ForkRunApprovalStore: Send + Sync {
    /// The setting stored for `org`; `None` when it has none (off).
    async fn fork_run_approval(&self, org: &str) -> Result<Option<ForkRunApproval>, IdentityError>;

    /// Store `setting` for `org`, replacing any stored before, as changed by
    /// `updated_by` at `updated_at` (unix ms). Returns the setting it
    /// replaced (`None`: the org had none), read in the same transaction as
    /// the write, so of two concurrent changes the second returns the
    /// first's setting.
    async fn put_fork_run_approval(
        &self,
        org: &str,
        setting: ForkRunApproval,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<Option<ForkRunApproval>, IdentityError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default_and_only_turning_it_off_loosens() {
        let off = ForkRunApproval::default();
        let on = ForkRunApproval { enabled: true };
        assert!(!off.enabled);
        assert!(on.loosened_by(off));
        assert!(!off.loosened_by(on));
        assert!(!on.loosened_by(on));
        assert!(!off.loosened_by(off));
    }
}
