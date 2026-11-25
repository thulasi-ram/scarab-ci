//! An org's limits on leak checks (`secret.match`): how many checks one
//! person may run in the org per rolling hour and per rolling day. Stored
//! per org, changed only by an org Owner; an org with no stored limits has
//! the defaults. Either limit at 0 turns the check off for the org.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::IdentityError;

/// Checks per person per rolling hour, for an org with no stored limits.
pub const DEFAULT_LEAK_CHECKS_PER_HOUR: u32 = 20;
/// Checks per person per rolling day, for an org with no stored limits.
pub const DEFAULT_LEAK_CHECKS_PER_DAY: u32 = 40;
/// The highest hourly limit an Owner may set.
pub const MAX_LEAK_CHECKS_PER_HOUR: u32 = 100;
/// The highest daily limit an Owner may set.
pub const MAX_LEAK_CHECKS_PER_DAY: u32 = 1000;

/// One org's leak check limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeakCheckLimits {
    /// Checks one person may run in the org in any rolling hour.
    pub per_hour: u32,
    /// Checks one person may run in the org in any rolling day.
    pub per_day: u32,
}

impl Default for LeakCheckLimits {
    fn default() -> Self {
        Self {
            per_hour: DEFAULT_LEAK_CHECKS_PER_HOUR,
            per_day: DEFAULT_LEAK_CHECKS_PER_DAY,
        }
    }
}

impl LeakCheckLimits {
    /// Either limit is 0: the org runs no leak checks.
    pub fn off(&self) -> bool {
        self.per_hour == 0 || self.per_day == 0
    }

    /// Both limits are within [`MAX_LEAK_CHECKS_PER_HOUR`] and
    /// [`MAX_LEAK_CHECKS_PER_DAY`].
    pub fn within_bounds(&self) -> bool {
        self.per_hour <= MAX_LEAK_CHECKS_PER_HOUR && self.per_day <= MAX_LEAK_CHECKS_PER_DAY
    }
}

/// Where each org's [`LeakCheckLimits`] are kept.
#[async_trait]
pub trait LeakCheckLimitsStore: Send + Sync {
    /// The limits stored for `org`; `None` when it has none (the defaults
    /// apply).
    async fn leak_check_limits(&self, org: &str) -> Result<Option<LeakCheckLimits>, IdentityError>;

    /// Store `limits` for `org`, replacing any stored before, as changed by
    /// `updated_by` at `updated_at` (unix ms). Returns the limits it
    /// replaced (`None`: the org had none), read in the same transaction as
    /// the write, so of two concurrent changes the second returns the
    /// first's limits.
    async fn put_leak_check_limits(
        &self,
        org: &str,
        limits: LeakCheckLimits,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<Option<LeakCheckLimits>, IdentityError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_on_and_within_bounds() {
        let d = LeakCheckLimits::default();
        assert_eq!((d.per_hour, d.per_day), (20, 40));
        assert!(!d.off());
        assert!(d.within_bounds());
    }

    #[test]
    fn either_limit_at_zero_turns_the_check_off() {
        assert!(LeakCheckLimits {
            per_hour: 0,
            per_day: 40
        }
        .off());
        assert!(LeakCheckLimits {
            per_hour: 20,
            per_day: 0
        }
        .off());
    }

    #[test]
    fn the_bounds_are_inclusive() {
        let at = |per_hour, per_day| LeakCheckLimits { per_hour, per_day }.within_bounds();
        assert!(at(100, 1000));
        assert!(!at(101, 1000));
        assert!(!at(100, 1001));
    }
}
