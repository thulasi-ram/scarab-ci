//! An in-memory [`LeakCheckLimitsStore`].

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use scarab_identity::{IdentityError, LeakCheckLimits, LeakCheckLimitsStore};

/// Each org's leak check limits, in memory.
#[derive(Default)]
pub struct InMemoryLeakCheckLimits {
    limits: Mutex<BTreeMap<String, LeakCheckLimits>>,
}

impl InMemoryLeakCheckLimits {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl LeakCheckLimitsStore for InMemoryLeakCheckLimits {
    async fn leak_check_limits(&self, org: &str) -> Result<Option<LeakCheckLimits>, IdentityError> {
        Ok(self.limits.lock().unwrap().get(org).copied())
    }

    async fn put_leak_check_limits(
        &self,
        org: &str,
        limits: LeakCheckLimits,
        _updated_by: &str,
        _updated_at: i64,
    ) -> Result<Option<LeakCheckLimits>, IdentityError> {
        Ok(self.limits.lock().unwrap().insert(org.to_string(), limits))
    }
}
