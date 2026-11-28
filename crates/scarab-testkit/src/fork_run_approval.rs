//! An in-memory [`ForkRunApprovalStore`].

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use scarab_identity::{ForkRunApproval, ForkRunApprovalStore, IdentityError};

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
        Ok(self.settings.lock().unwrap().get(org).copied())
    }

    async fn put_fork_run_approval(
        &self,
        org: &str,
        setting: ForkRunApproval,
        _updated_by: &str,
        _updated_at: i64,
    ) -> Result<Option<ForkRunApproval>, IdentityError> {
        Ok(self
            .settings
            .lock()
            .unwrap()
            .insert(org.to_string(), setting))
    }
}
