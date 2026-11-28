//! An in-memory [`ForkRunApprovalStore`].

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use scarab_identity::{
    fork_run_approval_org_key, ForkRunApproval, ForkRunApprovalStore, IdentityError,
};

/// Each org's Fork Run approval setting, in memory.
#[derive(Default)]
pub struct InMemoryForkRunApproval {
    settings: Mutex<BTreeMap<String, ForkRunApproval>>,
}

impl InMemoryForkRunApproval {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ForkRunApprovalStore for InMemoryForkRunApproval {
    async fn fork_run_approval(&self, org: &str) -> Result<Option<ForkRunApproval>, IdentityError> {
        Ok(self
            .settings
            .lock()
            .unwrap()
            .get(&fork_run_approval_org_key(org))
            .copied())
    }

    async fn put_fork_run_approval(
        &self,
        org: &str,
        expected: ForkRunApproval,
        new: ForkRunApproval,
        _updated_by: &str,
        _updated_at: i64,
    ) -> Result<bool, IdentityError> {
        let mut settings = self.settings.lock().unwrap();
        let key = fork_run_approval_org_key(org);
        if settings.get(&key).copied().unwrap_or_default() != expected {
            return Ok(false);
        }
        settings.insert(key, new);
        Ok(true)
    }
}
