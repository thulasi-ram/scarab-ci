//! How long THIS executor has itself watched a Pod since a moment the Pod
//! reports — the second clock every sidecar ceiling must also pass
//! (git-bug 070d042).
//!
//! The ceilings in `lib.rs` (`sidecar_reporting_ceiling_passed`,
//! `results_drain_overdue`) compare the control plane's wall clock with a
//! `finishedAt` the node stamped. A lost node is the case they exist for: its
//! kubelet stops reporting while the control plane keeps running. A host
//! suspend (a laptop sleeping under kind or colima), a `docker pause` of the
//! node, a VM migration or a long stall of the control plane is a different
//! case: the kubelet, the egress mid-drain AND this executor are all frozen,
//! and the wall clock jumps forward on resume. The first poll after it saw
//! the ceiling long passed and judged a live node lost ("egress sidecar
//! died").
//!
//! So a ceiling also has to elapse in time this executor watched the Pod:
//! from the first poll that saw the anchor (the step's end, the results
//! drain's start), summing the gaps between consecutive polls, each capped at
//! [`GAP_CAP`]. A freeze shows up as one long gap between two polls, and
//! counts as at most the cap, whichever clock stopped and whichever did not:
//!
//! - `Instant` (CLOCK_MONOTONIC) measures the gaps, so a wall-clock step (NTP
//!   resync after a resume, an operator setting the clock) never counts.
//! - CLOCK_MONOTONIC alone is not enough. It stops during a Linux suspend and
//!   in a guest whose host slept, but it keeps running while the process is
//!   frozen by a cgroup freezer (`docker pause`) or descheduled, and when the
//!   executor runs outside the frozen cluster. Capping the gap covers those.
//!
//! In memory, keyed by Pod UID: a control-plane restart only restarts the
//! wait (a lost node then costs one extra ceiling, never a false verdict). An
//! entry is dropped [`PRUNE_AFTER`] after its last observation — a settled
//! unit stops being polled — rather than at the terminal verdict, because
//! `results_delivery_failure` reads the same clock after that verdict.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The most one gap between two polls of the same Pod counts for. The
/// scheduler polls a Running step every tick (500 ms); this leaves room for a
/// slow tick under load while keeping a freeze from counting as more than a
/// tick-sized wait.
pub(crate) const GAP_CAP: Duration = Duration::from_secs(30);

/// An entry not observed for this long belongs to a unit that settled (or a
/// Pod that is gone) and is dropped on the next observation of any Pod.
pub(crate) const PRUNE_AFTER: Duration = Duration::from_secs(60 * 60);

/// What a [`WatchClock`] entry measures from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Anchor {
    /// The step container's `finishedAt`: the sidecar reporting ceiling.
    StepEnd,
    /// The results sidecar's drain start (`results_drain_began_at`): the
    /// results drain bound.
    ResultsDrain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Watch {
    last: Instant,
    watched: Duration,
}

/// One more observation of an anchor at `now`: the first starts the watch at
/// zero; each later one adds the gap since the previous, capped at
/// [`GAP_CAP`]. Pure, so tests construct the freeze instead of sleeping.
pub(crate) fn advance(prev: Option<Watch>, now: Instant) -> Watch {
    match prev {
        None => Watch {
            last: now,
            watched: Duration::ZERO,
        },
        Some(w) => Watch {
            last: now.max(w.last),
            watched: w
                .watched
                .saturating_add(now.saturating_duration_since(w.last).min(GAP_CAP)),
        },
    }
}

/// The executor's per-Pod watch clocks (see the module docs).
#[derive(Debug, Default)]
pub(crate) struct WatchClock {
    entries: Mutex<HashMap<(String, Anchor), Watch>>,
}

impl WatchClock {
    /// Record an observation of `anchor` on the Pod `uid` at `now`, and return
    /// how long (whole seconds) this executor has watched it so far.
    pub(crate) fn observe(&self, uid: &str, anchor: Anchor, now: Instant) -> i64 {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.retain(|_, w| now.saturating_duration_since(w.last) <= PRUNE_AFTER);
        let key = (uid.to_string(), anchor);
        let next = advance(entries.get(&key).copied(), now);
        entries.insert(key, next);
        i64::try_from(next.watched.as_secs()).unwrap_or(i64::MAX)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polls_accrue_their_gaps_and_a_freeze_counts_as_one_capped_gap() {
        let t0 = Instant::now();
        let w = advance(None, t0);
        assert_eq!(w.watched, Duration::ZERO, "the first sight starts at zero");
        let w = advance(Some(w), t0 + Duration::from_secs(10));
        assert_eq!(w.watched, Duration::from_secs(10));
        // A 15-minute freeze between two polls (a host suspend, a paused
        // node): it counts as one capped gap.
        let w = advance(Some(w), t0 + Duration::from_secs(10 + 900));
        assert_eq!(w.watched, Duration::from_secs(10) + GAP_CAP);
        // Polling resumes and accrues normally from there.
        let w = advance(Some(w), t0 + Duration::from_secs(10 + 900 + 5));
        assert_eq!(w.watched, Duration::from_secs(15) + GAP_CAP);
    }

    #[test]
    fn each_pod_and_anchor_has_its_own_watch_and_stale_ones_are_pruned() {
        let clock = WatchClock::default();
        let t0 = Instant::now();
        assert_eq!(clock.observe("a", Anchor::StepEnd, t0), 0);
        assert_eq!(
            clock.observe("a", Anchor::StepEnd, t0 + Duration::from_secs(20)),
            20
        );
        assert_eq!(
            clock.observe("a", Anchor::ResultsDrain, t0 + Duration::from_secs(20)),
            0,
            "the drain's watch starts when its anchor is first seen"
        );
        assert_eq!(
            clock.observe("b", Anchor::StepEnd, t0 + Duration::from_secs(20)),
            0
        );
        assert_eq!(clock.len(), 3);
        // Pod `b` keeps being polled; `a` settled and is not. Past the prune
        // window `a`'s entries go.
        let later = t0 + Duration::from_secs(20) + PRUNE_AFTER + Duration::from_secs(1);
        clock.observe("b", Anchor::StepEnd, later);
        assert_eq!(clock.len(), 1);
    }
}
