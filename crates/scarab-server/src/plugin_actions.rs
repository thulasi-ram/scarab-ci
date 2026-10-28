//! Absent plugins (ADR-0074 §5): a `plugin_action` Step whose plugin
//! installation is killed (ADR-0072 §10) or uninstalled fails at once with
//! the failure class `plugin-unavailable`, never retried.
//!
//! The kill and uninstall handlers fail the installation's pending
//! invocations in the same request ([`fail_for_installation`]). The sweep
//! ([`sweep_unavailable`], run on an interval by [`spawn_sweeper`]) does the
//! same for every pending invocation whose installation is no longer
//! enabled, so a crash between a kill and that request's failing, or a Step
//! that started after the kill, is still failed within one interval.
//!
//! An installation that is enabled but not answering is not "absent": its
//! Step stays pending until its `timeout`, and the reason says when the
//! plugin last answered (the scheduler's deadline pass).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use scarab_engine::{fail_plugin_invocations, Clock, Db, PluginInvocation, SchedulerError};
use scarab_plugin::{InstallationState, PluginStore};

/// How often [`spawn_sweeper`] looks for pending invocations of plugins
/// that are gone.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Fail every pending invocation of installation `installation` in `org`,
/// because it `why` ("was killed", "was uninstalled"). Best-effort: an error
/// is logged and the sweep retries it.
pub async fn fail_for_installation(
    db: &dyn Db,
    clock: &dyn Clock,
    org: &str,
    installation: &str,
    why: &str,
) {
    let result = async {
        let pending: Vec<PluginInvocation> = db
            .open_plugin_invocations(Some(org))
            .await?
            .into_iter()
            .filter(|i| i.installation == installation)
            .collect();
        fail_plugin_invocations(db, clock, &pending, why).await
    }
    .await;
    match result {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            org,
            installation,
            failed = n,
            "plugin_action steps failed: the plugin installation {why}"
        ),
        Err(e) => tracing::warn!(
            org,
            installation,
            error = %e,
            "failing the pending plugin_action steps of an absent plugin failed; the sweep \
             retries"
        ),
    }
}

/// Fail every pending invocation whose installation is killed or no longer
/// installed. Returns how many Attempts it closed.
pub async fn sweep_unavailable(
    db: &dyn Db,
    clock: &dyn Clock,
    plugins: &dyn PluginStore,
) -> Result<usize, SchedulerError> {
    let mut by_installation: BTreeMap<(String, String), Vec<PluginInvocation>> = BTreeMap::new();
    for inv in db.open_plugin_invocations(None).await? {
        by_installation
            .entry((inv.org.clone(), inv.installation.clone()))
            .or_default()
            .push(inv);
    }
    let mut closed = 0;
    for ((org, name), pending) in by_installation {
        let why = match plugins.installation(&org, &name).await {
            Ok(None) => "is not installed",
            Ok(Some(i)) if i.state == InstallationState::Killed => "was killed",
            Ok(Some(_)) => continue,
            Err(e) => {
                tracing::warn!(org, installation = %name, error = %e, "plugin registry read failed");
                continue;
            }
        };
        closed += fail_plugin_invocations(db, clock, &pending, why).await?;
    }
    Ok(closed)
}

/// Run [`sweep_unavailable`] every [`SWEEP_INTERVAL`].
pub fn spawn_sweeper(
    db: Arc<dyn Db>,
    clock: Arc<dyn Clock>,
    plugins: Arc<dyn PluginStore>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match sweep_unavailable(&*db, &*clock, &*plugins).await {
                Ok(0) => {}
                Ok(n) => tracing::warn!(
                    failed = n,
                    "plugin_action steps failed: their plugin installation is gone"
                ),
                Err(e) => tracing::warn!(error = %e, "plugin_action sweep failed"),
            }
            tokio::time::sleep(SWEEP_INTERVAL).await;
        }
    })
}
