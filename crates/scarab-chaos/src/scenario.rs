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

/// `WS_DRAIN_ESCALATION_MS` (`crates/scarab-executor-k8s/src/lib.rs`): how long
/// the control plane keeps re-driving a failing drain before it gives up.
pub const DRAIN_ESCALATION_SECS: u64 = 300;

/// Rank 1's two scenarios: the same fault (one Depot Pod deleted with grace
/// 0) at two moments.
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
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::MidDrain => "mid-drain",
            Kind::MidFeed => "mid-feed",
        }
    }

    /// The step whose Pod the trigger watches.
    pub fn target_step(self) -> &'static str {
        match self {
            Kind::MidDrain => "produce",
            Kind::MidFeed => "consume",
        }
    }

    /// The seeded delay range after the trigger, in ms. Measured 2026-11-01
    /// at 2+2 on colima (2 vCPU) with 64 x 2 MiB per producer: the drain
    /// window (step exit to root annotation) was 63 s, the feed 8 s.
    fn delay_range_ms(self) -> std::ops::RangeInclusive<u64> {
        match self {
            Kind::MidDrain => 2_000..=10_000,
            Kind::MidFeed => 0..=2_000,
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
    /// Which Depot replica is deleted: `scarab-workspace-<ordinal>`.
    pub victim_ordinal: u32,
}

impl Plan {
    pub fn from_seed(kind: Kind, seed: u64, workload: &Workload, depot_replicas: u32) -> Plan {
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        Plan {
            target_run: rng.gen_range(0..workload.runs.max(1)),
            delay_ms: rng.gen_range(kind.delay_range_ms()),
            victim_ordinal: rng.gen_range(0..depot_replicas.max(1)),
        }
    }
}

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

/// One fault, as fired.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fault {
    /// `delete-pod`: a Pod delete with `gracePeriodSeconds: 0`.
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
}

impl Scenario {
    pub fn new(kind: Kind, seed: u64, workload: Workload, server_replicas: u32, depot_replicas: u32) -> Self {
        let plan = Plan::from_seed(kind, seed, &workload, depot_replicas);
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

    /// The deadline every Run must be terminal by: the bound, from the fault
    /// (or from the start when nothing fired).
    pub fn quiesce_deadline_ms(&self) -> i64 {
        let from = self
            .faults
            .iter()
            .map(|f| f.fired_at_ms)
            .max()
            .unwrap_or(self.started_at_ms);
        from + (self.quiesce_bound_secs as i64) * 1000
    }
}

/// ADR-0077 §2: lease TTL + the workload's step time + the drain escalation,
/// for any scenario that faults the Depot.
pub fn quiesce_bound_secs(workload: &Workload) -> u64 {
    LEASE_TTL_SECS + workload.expected_secs + DRAIN_ESCALATION_SECS
}

/// Where a fault fired relative to a window.
pub fn classify(kind: Kind, fired_at_ms: i64, start: Option<i64>, end: Option<i64>) -> Landing {
    let class = match (start, end) {
        (Some(s), _) if fired_at_ms < s => "before-window".to_string(),
        (Some(_), Some(e)) if fired_at_ms >= e => "after-window".to_string(),
        (Some(_), _) => kind.as_str().to_string(),
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
            let a = Plan::from_seed(Kind::MidDrain, seed, &w, 2);
            assert_eq!(a, Plan::from_seed(Kind::MidDrain, seed, &w, 2));
            assert!(a.target_run < w.runs && a.victim_ordinal < 2);
            assert!((2_000..=10_000).contains(&a.delay_ms));
            let f = Plan::from_seed(Kind::MidFeed, seed, &w, 2);
            assert!(f.delay_ms <= 2_000);
        }
        // Different seeds explore different plans.
        let plans: std::collections::HashSet<_> = (0..50)
            .map(|s| {
                let p = Plan::from_seed(Kind::MidDrain, s, &w, 2);
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
        assert!(r.runs.is_empty() && r.replay_of.is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_bound_is_the_adr_sum() {
        assert_eq!(quiesce_bound_secs(&Workload::default()), 30 + 300 + 300);
    }

    #[test]
    fn landing_classes() {
        let k = Kind::MidDrain;
        assert_eq!(classify(k, 5, Some(10), Some(20)).class, "before-window");
        assert_eq!(classify(k, 15, Some(10), Some(20)).class, "mid-drain");
        assert_eq!(classify(k, 15, Some(10), None).class, "mid-drain");
        assert_eq!(classify(k, 25, Some(10), Some(20)).class, "after-window");
    }
}
