//! An org's **Fork Run approval** setting (ADR-0086 §9): which Runs of fork
//! code wait, before any of their Steps starts, at a `manual` gate that
//! someone with write on the repository releases. Stored per org, changed
//! only by an org admin from a browser. An org with no stored setting has
//! [`ForkRunApproval::AgentStarted`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::IdentityError;

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

    /// Does a Run of fork code get the gate? `agent_started`: an app acting
    /// for someone, such as an agent, started the Run, as the server decides
    /// from the DelegationTrail it records as the Run's origin.
    pub fn gates(self, agent_started: bool) -> bool {
        match self {
            Self::Off => false,
            Self::AgentStarted => agent_started,
            Self::All => true,
        }
    }
}

/// The key an org's setting is stored under: the org name in ASCII lower
/// case. The settings API takes the org from its URL and Run creation from
/// the repository's owner as the forge spells it; GitHub and Forgejo org
/// names are case-insensitive, so `Acme` and `acme` are one org, and a
/// letter-case difference must never fall back to the default.
pub fn fork_run_approval_org_key(org: &str) -> String {
    org.to_ascii_lowercase()
}

/// Where each org's [`ForkRunApproval`] is kept, under
/// [`fork_run_approval_org_key`].
#[async_trait]
pub trait ForkRunApprovalStore: Send + Sync {
    /// The setting stored for `org`; `None` when it has none (the default,
    /// [`ForkRunApproval::AgentStarted`], applies).
    async fn fork_run_approval(&self, org: &str) -> Result<Option<ForkRunApproval>, IdentityError>;

    /// Store `new` for `org`, as changed by `updated_by` at `updated_at`
    /// (unix ms), only when `org`'s setting is still `expected` (the
    /// default when it has none): a compare-and-set, so a change decided
    /// (and confirmed) against one value never lands on another. `false`,
    /// with nothing written, when the setting is no longer `expected`.
    async fn put_fork_run_approval(
        &self,
        org: &str,
        expected: ForkRunApproval,
        new: ForkRunApproval,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<bool, IdentityError>;
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
        assert!(!Off.gates(true));
        assert!(All.gates(false));
        assert!(AgentStarted.gates(true));
        assert!(!AgentStarted.gates(false));
    }

    #[test]
    fn the_org_key_ignores_letter_case() {
        assert_eq!(
            fork_run_approval_org_key("Acme"),
            fork_run_approval_org_key("acme")
        );
    }
}
