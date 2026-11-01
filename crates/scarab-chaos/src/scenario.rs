//! The scenario file (ADR-0077 §3): the workload, the seed and the plan it
//! gave, the faults as they were fired, and the quiesce bound. A run writes
//! one; `scarab-chaos replay` reads one back and fires the same plan, without
//! drawing from the seed again.

use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

/// Format version of the scenario file.
pub const FORMAT: u32 = 1;

/// The scheduler lease (ADR-0011): a peer takes over at most this long after
/// a holder stops (`crates/scarab-engine/src/scheduler.rs`).
pub const LEASE_TTL_SECS: u64 = 30;

/// `RESULTS_DRAIN_SLACK_SECS` (`crates/scarab-executor-k8s/src/lib.rs`): the
/// slack past a Pod's `terminationGracePeriodSeconds` after which the executor
/// reads an egress sidecar still showing `running` as a lost node. The sidecar
/// reporting ceiling is the step container's `finishedAt` + the grace + this.
pub const RESULTS_DRAIN_SLACK_SECS: u64 = 300;

/// How much longer than the sidecar reporting ceiling `pause-node` keeps the
/// node paused, so the first poll after the resume reads the ceiling as
/// passed on the wall clock.
pub const PAUSE_MARGIN_SECS: u64 = 30;

/// The Kubernetes default `terminationGracePeriodSeconds`, for a Pod record
/// that does not carry one.
pub const DEFAULT_GRACE_SECS: u64 = 30;

/// How long `pause-node` pauses the node for a Pod whose
/// `terminationGracePeriodSeconds` is `grace_secs`: the sidecar reporting
/// ceiling (grace + [`RESULTS_DRAIN_SLACK_SECS`]) + [`PAUSE_MARGIN_SECS`].
/// The producer's Pod is a workspace Pod with a grace of 600 s, so 930 s.
pub fn pause_secs(grace_secs: Option<i64>) -> u64 {
    let grace = grace_secs.and_then(|g| u64::try_from(g).ok()).unwrap_or(DEFAULT_GRACE_SECS);
    grace + RESULTS_DRAIN_SLACK_SECS + PAUSE_MARGIN_SECS
}

/// `WS_DRAIN_ESCALATION_MS` (`crates/scarab-executor-k8s/src/lib.rs`): how long
/// the control plane keeps re-driving a failing drain before it gives up.
pub const DRAIN_ESCALATION_SECS: u64 = 300;

/// The scenarios. Rank 1 (ADR-0077 §3): one Depot Pod deleted with grace 0,
/// at two moments. Rank 2: two server replicas with no fault, and one server
/// SIGKILLed mid-drain. Rank 4: the scheduler leader SIGSTOPped past the
/// lease TTL, then resumed. The server signals are sent from the kind
/// node's PID namespace (`crate::node`): in its container the server is PID
/// 1 with no init and ignores them. `pause-node` is not one of ADR-0077's
/// ranks: it reproduces git-bug 070d042 (a host suspend judged a live node
/// lost).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// While a producer's workspace drain to the Depot is owed or running:
    /// its step container has exited 0 and the Pod has no
    /// `scarab.io/workspace-root` annotation yet.
    MidDrain,
    /// While a consumer's `scarab-workspace-init` fetcher is pulling its
    /// inputs from the Depot.
    MidFeed,
    /// Rank 2 with no fault: the workload at two server replicas. It counts
    /// only when both replicas claimed some of its launch intents.
    TwoReplicas,
    /// Rank 2: SIGKILL the server replica that holds the producer's launch
    /// claim (and so polls its Pod and drives its drain) while the drain is
    /// in flight. The kubelet restarts the container in the same Pod.
    KillServer,
    /// Rank 4: SIGSTOP the scheduler leader while a producer runs, keep it
    /// stopped until a peer takes the lease over (past the 30 s TTL), then
    /// SIGCONT it.
    StopLeader,
    /// git-bug 070d042: while a producer's workspace drain is owed or running
    /// (the `mid-drain` trigger), `docker pause` the whole kind node past the
    /// sidecar reporting ceiling (`pause_secs`), then `docker unpause` it.
    /// The kubelet, the egress mid-drain and both servers freeze together, as
    /// in a host suspend, and the wall clock jumps on the resume.
    PauseNode,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::MidDrain => "mid-drain",
            Kind::MidFeed => "mid-feed",
            Kind::TwoReplicas => "two-replicas",
            Kind::KillServer => "kill-server",
            Kind::StopLeader => "stop-leader",
            Kind::PauseNode => "pause-node",
        }
    }

    /// The step whose Pod the trigger watches.
    pub fn target_step(self) -> &'static str {
        match self {
            Kind::MidFeed => "consume",
            _ => "produce",
        }
    }

    /// The window on the target Pod the fault must land in, by its landing
    /// class; `None` for a scenario with no window.
    pub fn window(self) -> Option<Kind> {
        match self {
            Kind::MidDrain | Kind::KillServer | Kind::PauseNode => Some(Kind::MidDrain),
            Kind::MidFeed => Some(Kind::MidFeed),
            Kind::TwoReplicas | Kind::StopLeader => None,
        }
    }

    /// Whether the scenario fires a fault at all.
    pub fn has_fault(self) -> bool {
        self != Kind::TwoReplicas
    }

    /// Whether the victim is a server replica (else a Depot replica).
    pub fn targets_servers(self) -> bool {
        matches!(self, Kind::KillServer | Kind::StopLeader | Kind::TwoReplicas)
    }

    /// The seeded delay range after the trigger, in ms. Measured 2026-11-01
    /// at 2+2 on colima (2 vCPU) with 64 x 2 MiB per producer: the drain
    /// window (step exit to root annotation) was 63 s, the feed 8 s. The
    /// leader stop needs only Runs in flight, and the producer's step runs
    /// for several seconds.
    fn delay_range_ms(self) -> std::ops::RangeInclusive<u64> {
        match self {
            Kind::MidDrain | Kind::KillServer => 2_000..=10_000,
            Kind::MidFeed => 0..=2_000,
            Kind::StopLeader => 0..=3_000,
            // Early in the drain window: the pause must start before the
            // drain records its root.
            Kind::PauseNode => 0..=2_000,
            Kind::TwoReplicas => 0..=0,
        }
    }
}

/// The workload: `runs` chained pipelines submitted together. Each producer
/// writes `files` files of `file_mib` MiB of random bytes and a SHA-256 list;
/// each consumer inherits the workspace and checks every file against it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workload {
    pub runs: u32,
    pub files: u32,
    pub file_mib: u32,
    pub image: String,
    /// Expected wall time of the whole workload without faults, the "step
    /// time" term of the quiesce bound.
    pub expected_secs: u64,
}

impl Default for Workload {
    fn default() -> Self {
        Self {
            runs: 3,
            files: 64,
            file_mib: 2,
            image: "scarab-clone:dev".into(),
            // One run took 151 s alone at 2+2 (d-probe-1.log); three at once
            // share 2 vCPU.
            expected_secs: 300,
        }
    }
}

impl Workload {
    /// Steps per Run in [`Workload::pipeline`]: produce and consume.
    pub const STEPS: usize = 2;

    /// The inline pipeline for one run (`POST /v1/runs` body).
    pub fn pipeline(&self) -> serde_json::Value {
        let bytes = u64::from(self.file_mib) * 1024 * 1024;
        let produce = format!(
            "set -e; mkdir -p /workspace/data; i=0; \
             while [ $i -lt {files} ]; do head -c {bytes} /dev/urandom > /workspace/data/f$i; i=$((i+1)); done; \
             cd /workspace/data; sha256sum f* > /workspace/SUMS; echo PRODUCED {files} files",
            files = self.files,
        );
        let consume = "cd /workspace/data && sha256sum -c -s /workspace/SUMS \
                       && echo CONSUMED-OK $(wc -l < /workspace/SUMS) files"
            .to_string();
        serde_json::json!({
            "pipeline": {
                "ir_version": 1,
                "steps": [
                    { "id": "produce", "image": self.image, "command": ["sh", "-c", produce] },
                    { "id": "consume", "image": self.image, "needs": ["produce"],
                      "command": ["sh", "-c", consume] }
                ]
            }
        })
    }
}

/// What the seed decides. Stored in the file, so a replay uses these values
/// and never the RNG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Index (submission order) of the run whose Pod triggers the fault.
    pub target_run: u32,
    /// How long after the trigger condition is first seen the fault fires.
    pub delay_ms: u64,
    /// Which replica is the victim. Rank 1: the Depot Pod
    /// `scarab-workspace-<ordinal>`. Ranks 2 and 4: the index into the
    /// server Pods sorted by name, used only when the claim or lease owner
    /// cannot be mapped to a Pod (the scenario records which it used).
    pub victim_ordinal: u32,
    /// Rank 4: how long the leader stays stopped after a peer has taken the
    /// lease over. Drawn last, so rank 1's plans did not change when it was
    /// added.
    #[serde(default)]
    pub hold_ms: u64,
}

impl Plan {
    pub fn from_seed(kind: Kind, seed: u64, workload: &Workload, server_replicas: u32, depot_replicas: u32) -> Plan {
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let victims = if kind.targets_servers() { server_replicas } else { depot_replicas };
        Plan {
            target_run: rng.gen_range(0..workload.runs.max(1)),
            delay_ms: rng.gen_range(kind.delay_range_ms()),
            victim_ordinal: rng.gen_range(0..victims.max(1)),
            hold_ms: rng.gen_range(0..=HOLD_MAX_MS),
        }
    }
}

/// Rank 4's longest hold after the takeover. Short on purpose: the chart's
/// liveness probe (`/healthz` every 15 s, three failures) restarts a stopped
/// server 30 to 45 s after it stops answering, and the lease TTL already
/// takes 30 s of that.
pub const HOLD_MAX_MS: u64 = 2_000;

/// The trigger as the nemesis saw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    pub run: String,
    pub step: String,
    pub attempt: Option<String>,
    pub pod: String,
    pub pod_uid: String,
    /// The condition that held (human-readable).
    pub condition: String,
    pub observed_at_ms: i64,
}

/// Where the fault landed, worked out after the run from the Pod record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Landing {
    /// `mid-drain` / `mid-feed` when the fault fired inside the window, else
    /// `before-window` or `after-window`.
    pub class: String,
    pub window_start_ms: Option<i64>,
    /// `None`: the window never closed on the target Pod (the drain record or
    /// the fetcher's exit was never seen there).
    pub window_end_ms: Option<i64>,
}

/// A signal sent to a server process from the kind node's PID namespace.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeSignal {
    /// `KILL` or `STOP` (a `STOP` is always followed by a `CONT`).
    pub signal: String,
    pub node: String,
    /// The `scarab-server` container's id (no `containerd://` prefix).
    pub container_id: String,
    /// The process's PID in the node's PID namespace (`crictl inspect`).
    pub pid: i64,
    /// Whether the signal took effect: for `KILL`, the kubelet restarted the
    /// container in the same Pod after a termination by signal 9 (exit
    /// 137); for `STOP`, `/proc/<pid>/status` said `T` (stopped).
    pub delivered: bool,
    /// What showed it (the termination record, or the `State:` line).
    pub evidence: String,
}

/// `pause-node`: the node container paused and unpaused from the host.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodePause {
    /// The kind node container (`SCARAB_CHAOS_NODE`).
    pub node: String,
    /// The target Pod's sidecar reporting ceiling: its grace +
    /// [`RESULTS_DRAIN_SLACK_SECS`].
    pub ceiling_secs: u64,
    /// How long the pause was planned for ([`pause_secs`]).
    pub planned_secs: u64,
    /// `docker inspect` said `State.Paused = true` after the pause.
    pub paused: bool,
    /// `docker inspect` said `State.Paused = false` after the unpause.
    pub unpaused: bool,
    /// What went wrong, if anything (a failed inspect, an early unpause on a
    /// signal).
    pub error: Option<String>,
}

/// Rank 2: the producer's launch claim, from `outbox`, and whether its owner
/// kept writing after the kill.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClaimObservation {
    /// `outbox.claimed_by` of the launch intent when the kill fired (only a
    /// claim that had not lapsed and an intent not yet dispatched).
    pub owner: Option<String>,
    pub claimed_until: Option<i64>,
    /// The latest `claimed_until` or lease `expires_at` the owner had
    /// written, 5 s after the kill (writes in flight at the kill have landed
    /// by then) and 20 s after it. Equal: the owner stopped writing, so it
    /// was the process killed. Grown: it is still alive.
    pub owner_written_until_5s: Option<i64>,
    pub owner_written_until_20s: Option<i64>,
}

/// Rank 4: the `leases` row `resource = 'scheduler'` around the stop. `term`
/// is read only when the column exists (ADR-0077 Amendment, grill, item 2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LeaseObservation {
    pub owner_before: String,
    pub expires_before: i64,
    pub term_before: Option<i64>,
    /// The first different owner seen while the old one was stopped.
    pub owner_after: Option<String>,
    pub term_after: Option<i64>,
    pub takeover_at_ms: Option<i64>,
    /// The row 10 s after the resume: the resumed process must not hold it.
    pub owner_after_resume: Option<String>,
    pub term_after_resume: Option<i64>,
    /// Replicas stopped and resumed first because the lease kept being
    /// renewed while they were stopped (they were not the leader).
    pub probes: Vec<String>,
}

/// One fault, as fired.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Fault {
    /// `delete-pod`: a Pod delete with `gracePeriodSeconds: 0`. `kill-server`:
    /// SIGKILL from the kind node. `stop-leader`: SIGSTOP, then SIGCONT, from
    /// the kind node. `pause-node`: `docker pause`, then `docker unpause`, of
    /// the kind node container.
    pub action: String,
    pub pod: String,
    pub pod_uid: Option<String>,
    pub grace_seconds: i64,
    pub trigger: Option<Trigger>,
    /// Whether the trigger condition still held when the fault fired.
    pub condition_held_at_fire: bool,
    /// Depot pack rows already staged for the target fence when it fired
    /// (`depot_packs`): a drain that has sealed a pack has started.
    pub fence_pack_rows_at_fire: Option<i64>,
    pub fired_at_ms: i64,
    pub landing: Option<Landing>,
    /// Ranks 2 and 4: the signal as sent and whether it took effect.
    #[serde(default)]
    pub signal: Option<NodeSignal>,
    /// Ranks 2 and 4: how the victim was chosen (`owner-names-pod`,
    /// `owner-in-pod-log`, `seed`, `lease-renewal`).
    #[serde(default)]
    pub target_basis: Option<String>,
    /// Ranks 2 and 4: what the victim turned out to be. Rank 2:
    /// `claim-holder`, `peer` or `unknown`. Rank 4: `leader`,
    /// `leader-no-takeover` or `not-leader`; `restarted-while-stopped` when
    /// the kubelet restarted it before the SIGCONT.
    #[serde(default)]
    pub victim_role: Option<String>,
    /// Rank 4: when the SIGCONT was sent. `pause-node`: when the unpause
    /// returned.
    #[serde(default)]
    pub resumed_at_ms: Option<i64>,
    #[serde(default)]
    pub claim: Option<ClaimObservation>,
    #[serde(default)]
    pub lease: Option<LeaseObservation>,
    /// `pause-node`: the pause as done. Its end is `resumed_at_ms`.
    #[serde(default)]
    pub pause: Option<NodePause>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scenario {
    pub format: u32,
    pub kind: Kind,
    pub seed: u64,
    /// Path of the scenario file this run replays, if it is a replay.
    pub replay_of: Option<String>,
    pub workload: Workload,
    pub plan: Plan,
    pub server_replicas: u32,
    pub depot_replicas: u32,
    /// LEASE_TTL + workload.expected_secs + DRAIN_ESCALATION (ADR-0077 §2),
    /// counted from the fault.
    pub quiesce_bound_secs: u64,
    pub started_at_ms: i64,
    pub runs: Vec<String>,
    pub faults: Vec<Fault>,
    /// When every Run was terminal, or the bound ran out.
    pub quiesced_at_ms: Option<i64>,
    pub notes: Vec<String>,
    /// The Cache probe (`crate::cache`): informational, its Runs are not in
    /// `runs`. `None` in a file written before it existed; a replay of such a
    /// file runs no probe.
    #[serde(default)]
    pub cache: Option<crate::cache::CacheProbe>,
}

impl Scenario {
    pub fn new(kind: Kind, seed: u64, workload: Workload, server_replicas: u32, depot_replicas: u32) -> Self {
        let plan = Plan::from_seed(kind, seed, &workload, server_replicas, depot_replicas);
        Self::with_plan(kind, seed, workload, plan, server_replicas, depot_replicas)
    }

    pub fn with_plan(
        kind: Kind,
        seed: u64,
        workload: Workload,
        plan: Plan,
        server_replicas: u32,
        depot_replicas: u32,
    ) -> Self {
        let quiesce_bound_secs = quiesce_bound_secs(&workload);
        let cache = Some(crate::cache::CacheProbe::new(&workload.image));
        Self {
            format: FORMAT,
            kind,
            seed,
            replay_of: None,
            workload,
            plan,
            server_replicas,
            depot_replicas,
            quiesce_bound_secs,
            started_at_ms: 0,
            runs: Vec::new(),
            faults: Vec::new(),
            quiesced_at_ms: None,
            notes: Vec::new(),
            cache,
        }
    }

    /// A fresh scenario that fires the same plan as `self` (a replay).
    pub fn replay(&self, path: &str) -> Self {
        let mut s = Self::with_plan(
            self.kind,
            self.seed,
            self.workload.clone(),
            self.plan.clone(),
            self.server_replicas,
            self.depot_replicas,
        );
        s.replay_of = Some(path.to_string());
        // The recorded pipeline, not this build's: the replay writes what the
        // original wrote.
        s.cache = self.cache.as_ref().map(crate::cache::CacheProbe::replay);
        s
    }

    pub fn read(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let s: Scenario =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if s.format != FORMAT {
            return Err(format!("{}: format {} (expected {FORMAT})", path.display(), s.format));
        }
        Ok(s)
    }

    pub fn write(&self, path: &std::path::Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text + "\n").map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The deadline every Run must be terminal by: the bound, from the end
    /// of the fault (its resume, for a stop), or from the start when nothing
    /// fired.
    pub fn quiesce_deadline_ms(&self) -> i64 {
        let from = self
            .faults
            .iter()
            .map(|f| f.resumed_at_ms.unwrap_or(f.fired_at_ms).max(f.fired_at_ms))
            .max()
            .unwrap_or(self.started_at_ms);
        from + (self.quiesce_bound_secs as i64) * 1000
    }
}

/// ADR-0077 §2: lease TTL + the workload's step time + the drain escalation.
/// The escalation term is for scenarios that fault the Depot; the server
/// scenarios keep it too, because a killed or stopped replica can leave a
/// drain to be driven again.
pub fn quiesce_bound_secs(workload: &Workload) -> u64 {
    LEASE_TTL_SECS + workload.expected_secs + DRAIN_ESCALATION_SECS
}

/// Where a fault fired relative to a window. Inside it, the class is the
/// window's name (`mid-drain`, `mid-feed`).
pub fn classify(window: Kind, fired_at_ms: i64, start: Option<i64>, end: Option<i64>) -> Landing {
    let class = match (start, end) {
        (Some(s), _) if fired_at_ms < s => "before-window".to_string(),
        (Some(_), Some(e)) if fired_at_ms >= e => "after-window".to_string(),
        (Some(_), _) => window.as_str().to_string(),
        (None, _) => "before-window".to_string(),
    };
    Landing {
        class,
        window_start_ms: start,
        window_end_ms: end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_gives_the_same_plan_and_stays_in_range() {
        let w = Workload::default();
        for seed in 0..200 {
            let a = Plan::from_seed(Kind::MidDrain, seed, &w, 2, 2);
            assert_eq!(a, Plan::from_seed(Kind::MidDrain, seed, &w, 2, 2));
            assert!(a.target_run < w.runs && a.victim_ordinal < 2);
            assert!((2_000..=10_000).contains(&a.delay_ms));
            let f = Plan::from_seed(Kind::MidFeed, seed, &w, 2, 2);
            assert!(f.delay_ms <= 2_000);
            // The victim ordinal ranges over the replicas the scenario faults.
            let k = Plan::from_seed(Kind::KillServer, seed, &w, 3, 1);
            assert!(k.victim_ordinal < 3 && k.hold_ms <= HOLD_MAX_MS);
            assert_eq!(Plan::from_seed(Kind::MidFeed, seed, &w, 3, 1).victim_ordinal, 0);
        }
        // Different seeds explore different plans.
        let plans: std::collections::HashSet<_> = (0..50)
            .map(|s| {
                let p = Plan::from_seed(Kind::MidDrain, s, &w, 2, 2);
                (p.target_run, p.victim_ordinal, p.delay_ms)
            })
            .collect();
        assert!(plans.len() > 40);
    }

    #[test]
    fn a_replay_keeps_the_plan_and_the_bound() {
        let mut s = Scenario::new(Kind::MidFeed, 7, Workload::default(), 2, 2);
        s.runs = vec!["r1".into()];
        let dir = std::env::temp_dir().join(format!("scarab-chaos-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scenario.json");
        s.write(&path).unwrap();
        let back = Scenario::read(&path).unwrap();
        assert_eq!(back, s);
        let r = back.replay(path.to_str().unwrap());
        assert_eq!((r.plan.clone(), r.quiesce_bound_secs), (s.plan.clone(), s.quiesce_bound_secs));
        // The replay writes the recorded probe pipeline, with nothing observed.
        assert_eq!(
            r.cache.as_ref().map(|c| c.pipeline_yaml.as_str()),
            s.cache.as_ref().map(|c| c.pipeline_yaml.as_str())
        );
        assert_eq!(r.cache.as_ref().and_then(|c| c.seed_run.clone()), None);
        assert!(r.runs.is_empty() && r.replay_of.is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A scenario file written before the rank 2 and 4 fields existed still
    /// reads, and replays with the same plan.
    #[test]
    fn an_old_scenario_file_still_reads() {
        let mut v = serde_json::to_value(Scenario::new(Kind::MidDrain, 3, Workload::default(), 2, 2)).unwrap();
        v["plan"].as_object_mut().unwrap().remove("hold_ms");
        v["faults"] = serde_json::json!([{
            "action": "delete-pod", "pod": "scarab-workspace-0", "pod_uid": null, "grace_seconds": 0,
            "trigger": null, "condition_held_at_fire": true, "fence_pack_rows_at_fire": null,
            "fired_at_ms": 5, "landing": null
        }]);
        v.as_object_mut().unwrap().remove("cache");
        let s: Scenario = serde_json::from_value(v).unwrap();
        assert_eq!(s.plan.hold_ms, 0);
        assert_eq!(s.faults[0].signal, None);
        assert_eq!(s.cache, None, "a file from before the Cache probe has none");
        assert_eq!(s.replay("old.json").cache, None, "and its replay runs none");
    }

    /// The bound counts from a stopped leader's resume, not from its stop.
    #[test]
    fn the_quiesce_deadline_counts_from_the_resume() {
        let mut s = Scenario::new(Kind::StopLeader, 1, Workload::default(), 2, 2);
        s.faults.push(Fault {
            fired_at_ms: 1_000,
            resumed_at_ms: Some(40_000),
            ..Default::default()
        });
        assert_eq!(s.quiesce_deadline_ms(), 40_000 + (s.quiesce_bound_secs as i64) * 1000);
    }

    #[test]
    fn the_bound_is_the_adr_sum() {
        assert_eq!(quiesce_bound_secs(&Workload::default()), 30 + 300 + 300);
    }

    /// `pause-node` parses from the recipe's argument and the file, keeps
    /// the mid-drain trigger and window, and pauses past the workspace Pod's
    /// sidecar reporting ceiling (600 s grace + 300 s slack).
    #[test]
    fn pause_node_parses_and_pauses_past_the_ceiling() {
        use clap::ValueEnum as _;
        assert_eq!(Kind::from_str("pause-node", false), Ok(Kind::PauseNode));
        assert_eq!(serde_json::to_value(Kind::PauseNode).unwrap(), "pause-node");
        assert_eq!(Kind::PauseNode.as_str(), "pause-node");
        assert_eq!(Kind::PauseNode.window(), Some(Kind::MidDrain));
        assert_eq!(Kind::PauseNode.target_step(), "produce");
        assert!(Kind::PauseNode.has_fault() && !Kind::PauseNode.targets_servers());
        assert_eq!(pause_secs(Some(600)), 930);
        assert_eq!(pause_secs(Some(30)), 360);
        assert_eq!(pause_secs(None), 360, "a record without the grace uses the Kubernetes default");
        let w = Workload::default();
        for seed in 0..50 {
            assert!(Plan::from_seed(Kind::PauseNode, seed, &w, 2, 2).delay_ms <= 2_000);
        }
        // A scenario file round-trips with its pause, and the quiesce bound
        // counts from the unpause.
        let mut s = Scenario::new(Kind::PauseNode, 1, w, 2, 2);
        s.faults.push(Fault {
            action: "pause-node".into(),
            fired_at_ms: 1_000,
            resumed_at_ms: Some(931_000),
            pause: Some(NodePause {
                node: "scarab-dev-control-plane".into(),
                ceiling_secs: 900,
                planned_secs: 930,
                paused: true,
                unpaused: true,
                error: None,
            }),
            ..Default::default()
        });
        let back: Scenario = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert_eq!(s.quiesce_deadline_ms(), 931_000 + (s.quiesce_bound_secs as i64) * 1000);
    }

    #[test]
    fn landing_classes() {
        let k = Kind::KillServer.window().unwrap();
        assert_eq!(classify(k, 5, Some(10), Some(20)).class, "before-window");
        assert_eq!(classify(k, 15, Some(10), Some(20)).class, "mid-drain");
        assert_eq!(classify(k, 15, Some(10), None).class, "mid-drain");
        assert_eq!(classify(k, 25, Some(10), Some(20)).class, "after-window");
    }
}
