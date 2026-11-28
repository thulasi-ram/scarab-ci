//! An org's **Fork Run approval** setting (ADR-0086 §9): which Runs of fork
//! code wait, before any of their Steps starts, at a `manual` gate that
//! someone with write on the repository releases. Stored per org, changed
//! only by an org admin from a browser. An org with no stored setting has
//! [`ForkRunApproval::AgentStarted`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{IdentityError, PrincipalTokenKind};

/// The `name` of the `origin` link of the gate the setting adds to a Run
/// (`kind: fork_run_approval_gate`).
pub const FORK_RUN_APPROVAL_SETTING_NAME: &str = "fork_run_approval";

/// One org's Fork Run approval setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkRunApproval {
    /// No Run gets the gate.
    Off,
    /// A Run of fork code started through an `oauth` PrincipalToken (an app
    /// acting for someone, such as an agent) gets the gate. The default.
    #[default]
    AgentStarted,
    /// Every Run of fork code gets the gate.
    All,
}

impl ForkRunApproval {
    /// The stored and wire name: `off`, `agent_started` or `all`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::AgentStarted => "agent_started",
            Self::All => "all",
        }
    }

    /// The setting a stored name reads as; `None` for a name this binary
    /// does not know, which a reader refuses rather than defaults.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "agent_started" => Some(Self::AgentStarted),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// How many Runs it gates: `Off` < `AgentStarted` < `All`.
    fn rank(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::AgentStarted => 1,
            Self::All => 2,
        }
    }

    /// Changing from `self` to `next` gates fewer Runs (`all` → `agent_started`,
    /// `all` → `off`, `agent_started` → `off`). Loosening needs a step-up.
    pub fn loosened_by(self, next: ForkRunApproval) -> bool {
        next.rank() < self.rank()
    }

    /// Does a Run of fork code get the gate? `started_by_token_kind` is the
    /// kind of the PrincipalToken that started the Run, as its
    /// DelegationTrail records it; `None` when no token started it (a
    /// session, a forge event). A kind this binary does not know counts as
    /// an app's: the gate is never skipped by default.
    pub fn gates(self, started_by_token_kind: Option<&str>) -> bool {
        match self {
            Self::Off => false,
            Self::All => true,
            Self::AgentStarted => started_by_token_kind
                .is_some_and(|k| PrincipalTokenKind::parse(k) != Some(PrincipalTokenKind::Issued)),
        }
    }
}

/// Where each org's [`ForkRunApproval`] is kept.
#[async_trait]
pub trait ForkRunApprovalStore: Send + Sync {
    /// The setting stored for `org`; `None` when it has none (the default,
    /// [`ForkRunApproval::AgentStarted`], applies).
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
    use ForkRunApproval::*;

    #[test]
    fn agent_started_by_default_and_names_round_trip() {
        assert_eq!(ForkRunApproval::default(), AgentStarted);
        for s in [Off, AgentStarted, All] {
            assert_eq!(ForkRunApproval::parse(s.as_str()), Some(s));
        }
        assert_eq!(ForkRunApproval::parse("on"), None);
    }

    #[test]
    fn only_gating_fewer_runs_loosens() {
        for (from, to, loosens) in [
            (All, AgentStarted, true),
            (All, Off, true),
            (AgentStarted, Off, true),
            (Off, AgentStarted, false),
            (Off, All, false),
            (AgentStarted, All, false),
            (All, All, false),
        ] {
            assert_eq!(from.loosened_by(to), loosens, "{from:?} → {to:?}");
        }
    }

    #[test]
    fn which_fork_runs_get_the_gate() {
        assert!(!Off.gates(Some("oauth")));
        assert!(All.gates(None));
        assert!(All.gates(Some("issued")));
        assert!(AgentStarted.gates(Some("oauth")));
        assert!(AgentStarted.gates(Some("a-kind-from-a-later-release")));
        assert!(!AgentStarted.gates(Some("issued")));
        assert!(!AgentStarted.gates(None));
    }
}
