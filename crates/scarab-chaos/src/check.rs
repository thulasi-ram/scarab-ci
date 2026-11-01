//! The invariant checker (ADR-0077 §2), for the invariants ranks 1, 2 and 4
//! exercise.
//!
//! [`gather`] reads the recorded state: Postgres (`runs`, `step_runs`,
//! `attempts`, `events`, the Depot tables), the bucket (HEAD and ranged GET
//! of every pack member a green Attempt's snapshot reaches), the kube API
//! (what still runs after quiesce) and the watcher's `pods.jsonl`.
//! [`evaluate`] is a pure function of those facts. The checker never takes
//! the API's or a step's word for success: a green Attempt passes I4 only when
//! its bytes read back from the bucket and hash to their addresses.
//!
//! | id   | invariant (ADR-0077 §2) |
//! |------|-------------------------|
//! | I1a  | at most one Pod UID per Attempt |
//! | I1b  | Attempt N+1's step container starts after Attempt N's ended, or after N was journalled finished or superseded or N's Pod was gone (only the journal counts for an N whose step container was never seen); an overlap after that is allowed until N's container has run the Pod's `terminationGracePeriodSeconds` + 30 s past its Pod's removal, and fails past it |
//! | I1c  | no double execution: per Step at most one succeeded Attempt, one `AttemptStarted` and one `AttemptFinished` per journalled Attempt (with an `attempts` row or not), and a Succeeded Step's evidence Attempt is that one |
//! | T1   | exactly one terminal `RunTransitioned` per Run, matching `runs.status` |
//! | I5   | folding each Run's journal (its last `RunTransitioned`, and per Step its last `StepTransitioned` or `StepSkipped`) gives `runs.status` and `step_runs.status` |
//! | I3   | every scenario Run terminal within the quiesce bound, and none left with `creating = TRUE` |
//! | I4   | every green Attempt's output snapshot closure is in committed packs whose ranges read back and hash to their addresses; a member in another fence's pack needs a `depot_fence_borrows` edge |
//! | I8   | after quiesce no Pod of a scenario Run is still pending or running, and no per-Pod Secret outlives its Pod |
//! | I10  | no `committed = FALSE` pack row older than the linger for a fence whose drain has a record; every committed pack's object exists at its recorded size |
//! | L1   | rank 4: a takeover of the scheduler lease increments `leases.term`, and the term never goes back (only where the column exists) |
//! | L2   | rank 4: a peer takes the stopped leader's lease over within the TTL + 30 s, wherever the stop landed (inside a fenced write too: the fence's stall bound releases the lease row) |
//! | W1   | the workload's own outcome: every scenario Run that ended, ended `succeeded`, and every green Attempt has an output snapshot |
//!
//! W1 is not one of ADR-0077 §2's invariants. As written there, I3 accepts a
//! dead-lettered Run and I4 passes a green Attempt whose snapshot is NULL, so
//! a Depot outage that dead-letters Runs (git-bug e140121) or a lost
//! workspace would pass them. The workload (`scenario::Workload`) is built so
//! that every Run succeeds and every step writes to `/workspace`: a consumer
//! that finds a missing or corrupt file fails its `sha256sum -c`.
//!
//! Outside the invariants, a scenario is INCONCLUSIVE, not PASS, when its
//! fault did not exercise what it names ([`exercised`]): no fault fired, the
//! fault landed outside its window, the trigger condition no longer held
//! when it fired, a server signal did not take effect, or it hit a replica
//! other than the one the scenario names. `two-replicas` is INCONCLUSIVE
//! when one replica drove every step. It is also INCONCLUSIVE when an
//! invariant looked at fewer things than the workload must produce
//! ([`minimums`]). A FAIL finding still makes the verdict FAIL.
//!
//! I5 folds the transition events in the checker. `scarab-engine` exports no
//! fold, and the checker does not link the engine it checks. A mismatch
//! between two non-terminal statuses (`pending` against `ready`) is reported
//! as a warning: the checker does not assume every non-terminal step is
//! journalled, and after quiesce every row should be terminal.
//!
//! I1b compares consecutive Attempts of one Step. None of the workloads here
//! is built to cause a retry (a Depot fault or a server kill is meant to be
//! absorbed), so I1b often has no pair: it is reported as not exercised and
//! has no minimum.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use futures::StreamExt as _;
use k8s_openapi::api::core::v1::{Pod, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{Api as KubeApi, ListParams};
use object_store::{path::Path as ObjPath, ObjectStoreExt as _};
use serde::{Deserialize, Serialize};

use crate::pods::{self, PodRecord};
use crate::scenario::{Fault, Kind, Scenario};
use crate::time::now_ms;

/// How old an uncommitted pack row of a recorded fence may be: the Depot's
/// 2 s idle-pack linger plus the drain client's 10 s record retry, with room
/// for a slow seal on a 2-vCPU VM.
pub const LINGER_BOUND_MS: i64 = 60_000;

/// A step container's (start, finish), unix ms.
type Interval = (Option<i64>, Option<i64>);
/// A pack member's byte range: (pack key, offset, length).
type RangeKey = (String, i64, i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The invariant does not hold: the verdict is FAIL.
    Fail,
    /// Reported, not counted: a known slow path or an ADR open question.
    Warn,
    Info,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub invariant: String,
    pub severity: Severity,
    pub detail: String,
}

/// The verdict's outcome. `just test chaos` exits 0, 1 and 3 for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Pass,
    /// At least one invariant does not hold.
    Fail,
    /// No invariant failed, but the scenario did not test what it names.
    Inconclusive,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "PASS",
            Outcome::Fail => "FAIL",
            Outcome::Inconclusive => "INCONCLUSIVE",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    /// `outcome == Pass`.
    pub pass: bool,
    pub outcome: Outcome,
    /// Why the scenario does not count as a test, when it does not.
    pub inconclusive: Vec<String>,
    pub checked_at_ms: i64,
    /// Every invariant that was evaluated, with the number of things it
    /// looked at (so "PASS" over nothing is visible).
    pub checked: BTreeMap<String, usize>,
    pub findings: Vec<Finding>,
    /// What I4 read back: per green Attempt, the objects in its snapshot
    /// closure and the bytes read from the bucket and hashed.
    pub snapshots: Vec<SnapshotSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub run: String,
    pub step: String,
    pub attempt: String,
    pub root: String,
    pub objects: usize,
    pub bytes_read: u64,
}

// --- facts --------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct RunRow {
    pub id: String,
    pub status: String,
    pub creating: bool,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub run: String,
    pub seq: i64,
    pub at: i64,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct AttemptRow {
    pub run: String,
    pub step: String,
    pub attempt: String,
    pub outcome: Option<String>,
    pub output_snapshot: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StepRow {
    pub run: String,
    pub step: String,
    pub status: String,
    pub output_snapshot: Option<String>,
    pub evidence_attempt: Option<String>,
}

/// The result of walking one green Attempt's snapshot closure.
#[derive(Debug, Clone, Default)]
pub struct SnapshotCheck {
    pub run: String,
    pub step: String,
    pub attempt: String,
    pub root: String,
    pub objects: usize,
    pub bytes: u64,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PackRow {
    pub pack_key: String,
    pub fence_key: String,
    pub committed: bool,
    /// Unix seconds (`depot_packs.created_at`).
    pub created_at: i64,
    pub bytes: i64,
    /// HEAD result: `Ok(size)` or the error.
    pub object: Result<u64, String>,
}

#[derive(Debug, Clone, Default)]
pub struct LiveObject {
    pub kind: String,
    pub name: String,
    pub run: Option<String>,
    pub phase: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub runs: Vec<RunRow>,
    pub events: Vec<EventRow>,
    pub attempts: Vec<AttemptRow>,
    pub steps: Vec<StepRow>,
    pub snapshots: Vec<SnapshotCheck>,
    pub packs: Vec<PackRow>,
    /// Fences with a `depot_drain_records` row.
    pub recorded_fences: HashSet<String>,
    pub pod_records: Vec<PodRecord>,
    pub live: Vec<LiveObject>,
    /// Pod names that still exist (any run), for the per-Pod Secret check.
    pub live_pod_names: HashSet<String>,
    pub quiesce_deadline_ms: i64,
    pub now_ms: i64,
    /// The scenario's kind; `gather` always sets it. `None` evaluates the
    /// invariants alone, with no "exercised" check (the unit tests of one
    /// invariant).
    pub kind: Option<Kind>,
    /// The faults as fired (scenario.json).
    pub faults: Vec<Fault>,
    /// How many Runs the workload submits (`workload.runs`).
    pub expected_runs: usize,
    /// Distinct `outbox.claimed_by` of the scenario's launch intents: the
    /// replicas that drove a step to its end (`two-replicas`).
    pub launch_claimers: BTreeSet<String>,
}

// --- evaluate (pure) -------------------------------------------------------------

fn fail(inv: &str, detail: String) -> Finding {
    Finding {
        invariant: inv.into(),
        severity: Severity::Fail,
        detail,
    }
}

fn warn(inv: &str, detail: String) -> Finding {
    Finding {
        invariant: inv.into(),
        severity: Severity::Warn,
        detail,
    }
}

fn event_kind(p: &serde_json::Value) -> &str {
    match p {
        serde_json::Value::String(s) => s,
        serde_json::Value::Object(m) => m.keys().next().map(String::as_str).unwrap_or(""),
        _ => "",
    }
}

fn event_field<'a>(p: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    p.as_object()?.values().next()?.get(field)?.as_str()
}

/// `a12` -> 12; anything else sorts last.
fn attempt_number(a: &str) -> u64 {
    a.trim_start_matches('a').parse().unwrap_or(u64::MAX)
}

const TERMINAL: &[&str] = &["Succeeded", "Failed", "Cancelled", "DeadLettered"];

/// Terminal `step_runs.status` values.
const STEP_TERMINAL: &[&str] = &["succeeded", "failed", "skipped", "cancelled"];

fn status_token(to: &str) -> String {
    match to {
        "DeadLettered" => "dead_lettered".into(),
        other => other.to_ascii_lowercase(),
    }
}

/// I1b's slack (ADR-0077 open question 1, answered 2026-11-01): Attempt N's
/// step container may still run this long past its Pod's
/// `terminationGracePeriodSeconds`, counted from the Pod's removal, while
/// Attempt N+1 runs.
pub const I1B_SLACK_SECS: i64 = 30;

/// The Kubernetes default `terminationGracePeriodSeconds`, for a Pod record
/// written before the watcher recorded it.
pub const DEFAULT_GRACE_SECS: i64 = 30;

/// What the watcher saw of one Attempt's Pod object going away.
#[derive(Debug, Clone, Default)]
struct PodLife {
    /// First record with a `deletionTimestamp`, or of the object gone.
    removal_ms: Option<i64>,
    /// First record of the object gone (`deleted`).
    gone_ms: Option<i64>,
    grace_secs: Option<i64>,
}

/// I1b's bound for an Attempt N whose step container overlapped N+1's:
/// `Some((until, anchor, bound_ms))` when N's container was seen running
/// later than `anchor + bound_ms`, else `None`.
///
/// The owner's rule counts the bound from N's Pod object being gone. The
/// watcher sees a container only through its Pod object, so once the object
/// is gone nothing more is seen and a bound counted from there could never
/// fail. The anchor is therefore the first sign of removal (the
/// `deletionTimestamp`, or the object gone if no record carried one), which
/// is never later than the object gone; a Pod that was never removed is
/// counted from N+1's start. `until` is N's container finish, else the
/// moment its object was gone (the last moment it could be seen), else the
/// last record of the run (it was still running when recording stopped).
fn i1b_overrun(life: &PodLife, end: Option<i64>, start_b: i64, last_record_ms: i64) -> Option<(i64, i64, i64)> {
    let grace = life.grace_secs.unwrap_or(DEFAULT_GRACE_SECS);
    let bound_ms = (grace + I1B_SLACK_SECS) * 1000;
    let anchor = life.removal_ms.unwrap_or(start_b);
    let until = end.or(life.gone_ms).unwrap_or(last_record_ms);
    (until > anchor + bound_ms).then_some((until, anchor, bound_ms))
}

/// Why `faults` did not exercise `kind`; empty when they did. Each fault must
/// have fired with its trigger condition still holding and landed inside the
/// window it names. A server signal must also have taken effect and hit the
/// replica the scenario names: rank 2 the producer's claim holder, rank 4
/// the leader, stopped past its TTL, then resumed. A leader whose lease no
/// peer took over was exercised: that is a liveness finding (L2), not a
/// missed window. `two-replicas` fires nothing.
pub fn exercised(kind: Kind, faults: &[Fault]) -> Vec<String> {
    if !kind.has_fault() {
        return faults
            .iter()
            .map(|x| format!("{} {} fired in a scenario that has no fault", x.action, x.pod))
            .collect();
    }
    if faults.is_empty() {
        return vec!["no fault fired (the trigger never held, or every Run ended first)".into()];
    }
    let mut why = Vec::new();
    for x in faults {
        if !x.condition_held_at_fire {
            why.push(format!("{} {}: the trigger condition no longer held when it fired", x.action, x.pod));
        }
        if let Some(w) = kind.window() {
            match &x.landing {
                None => why.push(format!("{} {}: where it landed is unknown", x.action, x.pod)),
                Some(l) if l.class != w.as_str() => why.push(format!(
                    "{} {}: landed {}, outside the {} window",
                    x.action,
                    x.pod,
                    l.class,
                    w.as_str()
                )),
                Some(_) => {}
            }
        }
        if kind.targets_servers() {
            match &x.signal {
                Some(sig) if sig.delivered => {}
                Some(sig) => why.push(format!(
                    "{} {}: SIG{} to PID {} did not take effect ({})",
                    x.action, x.pod, sig.signal, sig.pid, sig.evidence
                )),
                None => why.push(format!("{} {}: no signal was recorded", x.action, x.pod)),
            }
        }
        let role = x.victim_role.as_deref().unwrap_or("unknown");
        match kind {
            Kind::KillServer if role != "claim-holder" => why.push(format!(
                "{} {}: the victim was the {role}, not the replica holding the producer's launch claim \
                 (chosen by {})",
                x.action,
                x.pod,
                x.target_basis.as_deref().unwrap_or("?")
            )),
            Kind::StopLeader => {
                // `leader-no-takeover` is exercised and judged by L2. Before
                // the fence's stall bound (git-bug aaf4e3e) a stop that landed
                // inside a fenced write held the lease row until the resume,
                // and this was the escape for it; now it is a finding.
                if role != "leader" && role != "leader-no-takeover" {
                    why.push(format!(
                        "{} {}: {role} (it was not the leader, or it did not stay stopped until its resume)",
                        x.action, x.pod
                    ));
                }
                match x.resumed_at_ms {
                    None => why.push(format!("{} {}: never resumed", x.action, x.pod)),
                    Some(r) if r - x.fired_at_ms <= crate::scenario::LEASE_TTL_SECS as i64 * 1000 => why.push(format!(
                        "{} {}: stopped {} ms, not past the {} s lease TTL",
                        x.action,
                        x.pod,
                        r - x.fired_at_ms,
                        crate::scenario::LEASE_TTL_SECS
                    )),
                    Some(_) => {}
                }
            }
            _ => {}
        }
    }
    why
}

/// The least each invariant must have looked at for `runs` Runs of the
/// workload, and why a count below it makes the scenario inconclusive. I1b,
/// I8 and I10 have none (I1b: no scenario here is built to cause a retry).
pub fn minimums(runs: usize, checked: &BTreeMap<String, usize>) -> Vec<String> {
    let attempts = runs * crate::scenario::Workload::STEPS;
    let want = [
        ("T1", runs, "one per scenario Run"),
        ("I3", runs, "one per scenario Run"),
        ("I1a", attempts, "the Pod of every Attempt, as the watcher saw it"),
        ("I1c", attempts, "every Step of every Run"),
        ("I5", runs + attempts, "every Run row and Step row"),
        ("I4", attempts, "every green Attempt's snapshot, read back"),
    ];
    want.iter()
        .filter_map(|(inv, min, what)| {
            let n = checked.get(*inv).copied().unwrap_or(0);
            (n < *min).then(|| format!("{inv} looked at {n}, below its minimum of {min} ({what})"))
        })
        .collect()
}

pub fn evaluate(f: &Facts) -> Verdict {
    let mut findings = Vec::new();
    let mut checked: BTreeMap<String, usize> = BTreeMap::new();

    // I1a: one Pod UID per Attempt.
    let mut uids: BTreeMap<(String, String, String), HashSet<String>> = BTreeMap::new();
    for r in &f.pod_records {
        if let (Some(run), Some(step), Some(att)) = (&r.pod.run, &r.pod.step, &r.pod.attempt) {
            uids.entry((run.clone(), step.clone(), att.clone()))
                .or_default()
                .insert(r.pod.uid.clone());
        }
    }
    checked.insert("I1a".into(), uids.len());
    for ((run, step, att), set) in &uids {
        if set.len() > 1 {
            findings.push(fail(
                "I1a",
                format!("run {run} step {step} attempt {att} had {} Pod UIDs: {set:?}", set.len()),
            ));
        }
    }

    // I1b: step container intervals of consecutive Attempts do not overlap,
    // unless the earlier one was journalled finished or superseded, or its
    // Pod was gone, before the later one started; and then only within the
    // bound (`I1B_SLACK_SECS`).
    let mut intervals: BTreeMap<(String, String), BTreeMap<String, Interval>> =
        BTreeMap::new();
    let mut lives: HashMap<(String, String, String), PodLife> = HashMap::new();
    let last_record_ms = f.pod_records.iter().map(|r| r.at_ms).max().unwrap_or(0);
    for r in &f.pod_records {
        let (Some(run), Some(step), Some(att)) = (&r.pod.run, &r.pod.step, &r.pod.attempt) else {
            continue;
        };
        let life = lives.entry((run.clone(), step.clone(), att.clone())).or_default();
        let gone = r.event == "deleted";
        if gone {
            life.gone_ms = Some(life.gone_ms.map_or(r.at_ms, |x| x.min(r.at_ms)));
        }
        if gone || r.pod.deleting {
            life.removal_ms = Some(life.removal_ms.map_or(r.at_ms, |x| x.min(r.at_ms)));
        }
        if r.pod.grace_secs.is_some() {
            life.grace_secs = r.pod.grace_secs;
        }
        let Some(c) = r.pod.container(pods::STEP_CONTAINER) else { continue };
        let e = intervals
            .entry((run.clone(), step.clone()))
            .or_default()
            .entry(att.clone())
            .or_insert((None, None));
        if let Some(s) = c.started_at_ms {
            e.0 = Some(e.0.map_or(s, |x: i64| x.min(s)));
        }
        if let Some(t) = c.finished_at_ms {
            e.1 = Some(e.1.map_or(t, |x: i64| x.max(t)));
        }
    }
    // An Attempt the journal started but whose step container the watcher
    // never saw (its Pod deleted before a status was recorded, or a watch
    // gap) is still paired: with no interval, only the journal can show it
    // ended before the next one started.
    for e in &f.events {
        if event_kind(&e.payload) != "AttemptStarted" {
            continue;
        }
        if let (Some(step), Some(att)) = (event_field(&e.payload, "step"), event_field(&e.payload, "attempt")) {
            intervals
                .entry((e.run.clone(), step.into()))
                .or_default()
                .entry(att.into())
                .or_insert((None, None));
        }
    }
    let seq_of = |run: &str, kind: &str, step: &str, att: &str| -> Option<i64> {
        f.events
            .iter()
            .filter(|e| e.run == run && event_kind(&e.payload) == kind)
            .filter(|e| event_field(&e.payload, "step") == Some(step))
            .filter(|e| event_field(&e.payload, "attempt") == Some(att))
            .map(|e| e.seq)
            .min()
    };
    let mut pairs = 0;
    for ((run, step), by_att) in &intervals {
        let mut atts: Vec<(&String, &Interval)> = by_att.iter().collect();
        atts.sort_by_key(|(a, _)| attempt_number(a));
        for w in atts.windows(2) {
            pairs += 1;
            let ((a, (start_a, end_a)), (b, (start_b, _))) = (w[0], w[1]);
            let Some(start_b) = *start_b else { continue };
            let started_b = seq_of(run, "AttemptStarted", step, b);
            let ended_a = [
                seq_of(run, "AttemptFinished", step, a),
                seq_of(run, "AttemptSuperseded", step, a),
            ]
            .into_iter()
            .flatten()
            .min();
            let journalled_ended = matches!((ended_a, started_b), (Some(e), Some(s)) if e < s);
            if start_a.is_none() && end_a.is_none() {
                // a's step container was never seen: no interval to compare.
                if !journalled_ended {
                    findings.push(fail(
                        "I1b",
                        format!(
                            "run {run} step {step}: {b}'s step container started at {start_b}, {a}'s was never \
                             seen, and {a} was not journalled ended before {b} started"
                        ),
                    ));
                }
                continue;
            }
            let overlaps = match end_a {
                Some(end) => start_b < *end,
                // Attempt a's container started and never finished.
                None => start_a.is_some(),
            };
            if !overlaps {
                continue;
            }
            let life = lives
                .get(&(run.clone(), step.clone(), a.clone()))
                .cloned()
                .unwrap_or_default();
            let gone_first = life.gone_ms.is_some_and(|g| g <= start_b);
            if !journalled_ended && !gone_first {
                findings.push(fail(
                    "I1b",
                    format!(
                        "run {run} step {step}: {b}'s step container started at {start_b} before {a}'s \
                         ended ({end_a:?}), before {a} was journalled ended and before {a}'s Pod was gone"
                    ),
                ));
                continue;
            }
            match i1b_overrun(&life, *end_a, start_b, last_record_ms) {
                None => findings.push(warn(
                    "I1b",
                    format!(
                        "run {run} step {step}: {b}'s container started while {a}'s was still running, after \
                         {a} was journalled ended or its Pod was gone (allowed within the Pod's grace period \
                         + {I1B_SLACK_SECS} s)"
                    ),
                )),
                Some((until, anchor, bound_ms)) => findings.push(fail(
                    "I1b",
                    format!(
                        "run {run} step {step}: {a}'s step container was seen running until {until}, {} ms \
                         after its Pod's removal at {anchor}, past the bound of {bound_ms} ms (grace period \
                         + {I1B_SLACK_SECS} s), while {b} ran",
                        until - anchor
                    ),
                )),
            }
        }
    }
    checked.insert("I1b".into(), pairs);

    // I1c: no double execution.
    let mut steps_seen = 0;
    let mut by_step: BTreeMap<(String, String), Vec<&AttemptRow>> = BTreeMap::new();
    for a in &f.attempts {
        by_step.entry((a.run.clone(), a.step.clone())).or_default().push(a);
    }
    for ((run, step), atts) in &by_step {
        steps_seen += 1;
        let green: Vec<&str> = atts
            .iter()
            .filter(|a| a.outcome.as_deref() == Some("succeeded"))
            .map(|a| a.attempt.as_str())
            .collect();
        if green.len() > 1 {
            findings.push(fail("I1c", format!("run {run} step {step}: {} succeeded Attempts {green:?}", green.len())));
        }
        if let Some(s) = f.steps.iter().find(|s| s.run == *run && s.step == *step) {
            if s.status == "succeeded" {
                match (&s.evidence_attempt, green.as_slice()) {
                    (Some(ev), [g]) if ev == g => {}
                    (ev, g) => findings.push(fail(
                        "I1c",
                        format!("run {run} step {step} is succeeded with evidence Attempt {ev:?} but succeeded Attempts {g:?}"),
                    )),
                }
            }
        }
    }
    // One AttemptStarted and one AttemptFinished per Attempt, counted over
    // the journal itself: an Attempt with no `attempts` row is counted too.
    let mut per_attempt: BTreeMap<(String, String, String, &str), usize> = BTreeMap::new();
    for e in &f.events {
        let kind = event_kind(&e.payload);
        if kind != "AttemptStarted" && kind != "AttemptFinished" {
            continue;
        }
        if let (Some(step), Some(att)) = (event_field(&e.payload, "step"), event_field(&e.payload, "attempt")) {
            *per_attempt.entry((e.run.clone(), step.into(), att.into(), kind)).or_default() += 1;
        }
    }
    for ((run, step, att, kind), n) in &per_attempt {
        if *n > 1 {
            findings.push(fail("I1c", format!("run {run} step {step} attempt {att}: {n} {kind} events")));
        }
    }
    checked.insert("I1c".into(), steps_seen);

    // T1: exactly one terminal transition per Run.
    for r in &f.runs {
        let terminal: Vec<&EventRow> = f
            .events
            .iter()
            .filter(|e| e.run == r.id && event_kind(&e.payload) == "RunTransitioned")
            .filter(|e| event_field(&e.payload, "to").is_some_and(|t| TERMINAL.contains(&t)))
            .collect();
        let is_terminal = crate::env::TERMINAL_RUN_STATUSES.contains(&r.status.as_str());
        match (terminal.as_slice(), is_terminal) {
            ([], false) => {} // I3 reports it
            ([one], true) => {
                let to = event_field(&one.payload, "to").unwrap_or_default();
                if status_token(to) != r.status {
                    findings.push(fail("T1", format!("run {}: journal says {to}, runs.status says {}", r.id, r.status)));
                }
            }
            (many, _) => findings.push(fail(
                "T1",
                format!(
                    "run {}: {} terminal RunTransitioned events ({:?}), runs.status {}",
                    r.id,
                    many.len(),
                    many.iter().map(|e| event_field(&e.payload, "to")).collect::<Vec<_>>(),
                    r.status
                ),
            )),
        }
    }
    checked.insert("T1".into(), f.runs.len());

    // I5: the journal folds to the projected rows.
    let mut i5 = 0;
    for r in &f.runs {
        if r.status == "missing" {
            continue; // I3 and the minimums report a Run with no row
        }
        i5 += 1;
        let folded = f
            .events
            .iter()
            .filter(|e| e.run == r.id && event_kind(&e.payload) == "RunTransitioned")
            .max_by_key(|e| e.seq)
            .and_then(|e| event_field(&e.payload, "to"))
            .map(status_token)
            .unwrap_or_else(|| "pending".into());
        if folded != r.status {
            let terminal = |s: &str| crate::env::TERMINAL_RUN_STATUSES.contains(&s);
            let detail = format!("run {}: the journal folds to {folded}, runs.status is {}", r.id, r.status);
            findings.push(if terminal(&folded) || terminal(&r.status) {
                fail("I5", detail)
            } else {
                warn("I5", detail)
            });
        }
    }
    for s in &f.steps {
        i5 += 1;
        let folded = f
            .events
            .iter()
            .filter(|e| e.run == s.run && event_field(&e.payload, "step") == Some(s.step.as_str()))
            .filter(|e| matches!(event_kind(&e.payload), "StepTransitioned" | "StepSkipped"))
            .max_by_key(|e| e.seq)
            .map(|e| match event_kind(&e.payload) {
                "StepSkipped" => "skipped".to_string(),
                _ => event_field(&e.payload, "to").unwrap_or_default().to_ascii_lowercase(),
            })
            .unwrap_or_else(|| "pending".into());
        if folded != s.status {
            let terminal = |x: &str| STEP_TERMINAL.contains(&x);
            let detail = format!(
                "run {} step {}: the journal folds to {folded}, step_runs.status is {}",
                s.run, s.step, s.status
            );
            findings.push(if terminal(&folded) || terminal(&s.status) {
                fail("I5", detail)
            } else {
                warn("I5", detail)
            });
        }
    }
    checked.insert("I5".into(), i5);

    // I3: terminal within the bound.
    for r in &f.runs {
        if !crate::env::TERMINAL_RUN_STATUSES.contains(&r.status.as_str()) {
            findings.push(fail("I3", format!("run {} is still {} after the quiesce bound", r.id, r.status)));
            continue;
        }
        let at = f
            .events
            .iter()
            .filter(|e| e.run == r.id && event_kind(&e.payload) == "RunTransitioned")
            .filter(|e| event_field(&e.payload, "to").is_some_and(|t| TERMINAL.contains(&t)))
            .map(|e| e.at)
            .min();
        if let Some(at) = at {
            if at > f.quiesce_deadline_ms {
                findings.push(fail(
                    "I3",
                    format!("run {} became terminal {} ms after the quiesce bound", r.id, at - f.quiesce_deadline_ms),
                ));
            }
        }
    }
    for r in f.runs.iter().filter(|r| r.creating) {
        findings.push(fail("I3", format!("run {} still has creating = TRUE", r.id)));
    }
    checked.insert("I3".into(), f.runs.len());

    // W1: the workload's outcome. A Run that is not terminal is I3's finding.
    let mut w1 = 0;
    for r in &f.runs {
        if crate::env::TERMINAL_RUN_STATUSES.contains(&r.status.as_str()) {
            w1 += 1;
            if r.status != "succeeded" {
                findings.push(fail("W1", format!("run {} ended {}, not succeeded", r.id, r.status)));
            }
        }
    }
    for a in &f.attempts {
        if a.outcome.as_deref() == Some("succeeded") {
            w1 += 1;
            if a.output_snapshot.is_none() {
                findings.push(fail(
                    "W1",
                    format!(
                        "run {} step {} attempt {} is succeeded with no output snapshot (its workspace was not recorded)",
                        a.run, a.step, a.attempt
                    ),
                ));
            }
        }
    }
    checked.insert("W1".into(), w1);

    // I4: green snapshots are durable and read back.
    for s in &f.snapshots {
        for p in &s.problems {
            findings.push(fail(
                "I4",
                format!("run {} step {} attempt {} root {}: {p}", s.run, s.step, s.attempt, s.root),
            ));
        }
    }
    checked.insert("I4".into(), f.snapshots.len());

    // I8: nothing of a terminal scenario Run still runs.
    for o in &f.live {
        match o.kind.as_str() {
            "Pod" => {
                let phase = o.phase.as_deref().unwrap_or("Unknown");
                if phase != "Succeeded" && phase != "Failed" {
                    findings.push(fail("I8", format!("Pod {} of run {:?} is {phase} after quiesce", o.name, o.run)));
                }
            }
            "Secret" => findings.push(fail("I8", format!("Secret {} outlives its Pod", o.name))),
            kind => findings.push(fail("I8", format!("{kind} {} of run {:?} remains after quiesce", o.name, o.run))),
        }
    }
    // What I8 looked at is every scenario Run's leftovers; finding none is
    // the pass, so the count is the Runs, not the objects found.
    checked.insert("I8".into(), f.runs.len());

    // I10: no stale uncommitted pack rows; committed packs have their object.
    let cutoff_s = (f.now_ms - LINGER_BOUND_MS) / 1000;
    for p in &f.packs {
        if !p.committed && p.created_at < cutoff_s {
            let age = f.now_ms / 1000 - p.created_at;
            if f.recorded_fences.contains(&p.fence_key) {
                findings.push(fail(
                    "I10",
                    format!("pack {} is uncommitted {age} s after staging, but its fence has a drain record", p.pack_key),
                ));
            } else {
                findings.push(warn(
                    "I10",
                    format!(
                        "pack {} of an unrecorded fence is uncommitted {age} s after staging (an abandoned drain; \
                         the pack reclaimer removes it later, git-bug ad79c90)",
                        p.pack_key
                    ),
                ));
            }
        }
        if p.committed {
            match &p.object {
                Ok(size) if *size == p.bytes as u64 => {}
                Ok(size) => findings.push(fail(
                    "I10",
                    format!("pack {}: object is {size} bytes, row says {}", p.pack_key, p.bytes),
                )),
                Err(e) => findings.push(fail("I10", format!("committed pack {} has no object: {e}", p.pack_key))),
            }
        }
    }
    checked.insert("I10".into(), f.packs.len());

    // L1: a takeover of the scheduler lease increments its term (ADR-0077
    // Amendment, grill, item 2), and the term never goes back. Read only
    // where the `leases.term` column exists.
    let mut l1 = 0;
    for x in &f.faults {
        let Some(l) = &x.lease else { continue };
        if let (Some(after), Some(tb), Some(ta)) = (&l.owner_after, l.term_before, l.term_after) {
            l1 += 1;
            if *after != l.owner_before && ta <= tb {
                findings.push(fail(
                    "L1",
                    format!("the scheduler lease passed from {} to {after} with term {tb} -> {ta}", l.owner_before),
                ));
            }
        }
        if let (Some(ta), Some(tr)) = (l.term_after, l.term_after_resume) {
            if tr < ta {
                findings.push(fail("L1", format!("the scheduler lease's term went back from {ta} to {tr} after the resume")));
            }
        }
        if l.owner_after.is_some() && l.owner_after_resume.as_deref() == Some(l.owner_before.as_str()) {
            findings.push(warn(
                "L1",
                format!("the resumed replica {} holds the scheduler lease again 10 s after its resume", l.owner_before),
            ));
        }
    }
    if f.faults.iter().any(|x| x.lease.is_some()) {
        checked.insert("L1".into(), l1);
    }

    // L2: the stopped leader lost its lease. The harness waits the TTL + 30 s
    // for a takeover; the scheduler lease expires 30 s after the last renewal
    // and a fence the stopped leader held ends after its 10 s stall bound.
    let mut l2 = 0;
    for x in &f.faults {
        match x.victim_role.as_deref() {
            Some("leader") => l2 += 1,
            Some("leader-no-takeover") => {
                l2 += 1;
                findings.push(fail(
                    "L2",
                    format!(
                        "{} {}: the scheduler lease was neither renewed nor taken over within the {} s TTL + 30 s \
                         while its leader was stopped (a fence held by the stopped leader should end after its \
                         stall bound)",
                        x.action,
                        x.pod,
                        crate::scenario::LEASE_TTL_SECS
                    ),
                ));
            }
            _ => {}
        }
    }
    if f.kind == Some(Kind::StopLeader) {
        checked.insert("L2".into(), l2);
    }

    let inconclusive = match f.kind {
        Some(kind) => {
            let mut why = exercised(kind, &f.faults);
            why.extend(minimums(f.expected_runs, &checked));
            if kind == Kind::TwoReplicas && f.launch_claimers.len() < 2 {
                why.push(format!(
                    "{} replica(s) drove the scenario's steps ({:?}); two replicas did not share the work",
                    f.launch_claimers.len(),
                    f.launch_claimers
                ));
            }
            why
        }
        None => Vec::new(),
    };
    let outcome = if findings.iter().any(|x| x.severity == Severity::Fail) {
        Outcome::Fail
    } else if !inconclusive.is_empty() {
        Outcome::Inconclusive
    } else {
        Outcome::Pass
    };
    Verdict {
        pass: outcome == Outcome::Pass,
        outcome,
        inconclusive,
        checked_at_ms: f.now_ms,
        checked,
        findings,
        snapshots: f
            .snapshots
            .iter()
            .map(|s| SnapshotSummary {
                run: s.run.clone(),
                step: s.step.clone(),
                attempt: s.attempt.clone(),
                root: s.root.clone(),
                objects: s.objects,
                bytes_read: s.bytes,
            })
            .collect(),
    }
}

// --- gather --------------------------------------------------------------------------

pub struct Sources<'a> {
    pub pg: &'a sqlx::PgPool,
    pub bucket: &'a object_store::aws::AmazonS3,
    pub kube: kube::Client,
    pub namespace: &'a str,
}

fn db_err(e: sqlx::Error) -> String {
    format!("postgres: {e}")
}

pub async fn gather(src: &Sources<'_>, scenario: &Scenario, pod_records: Vec<PodRecord>) -> Result<Facts, String> {
    let runs = &scenario.runs;
    // Only the scenario's own step Pods: the namespace also holds Pods of
    // earlier Runs and other modes.
    let pod_records = pod_records
        .into_iter()
        .filter(|r| r.pod.run.as_ref().is_none_or(|x| runs.contains(x)))
        .collect();
    let mut f = Facts {
        pod_records,
        quiesce_deadline_ms: scenario.quiesce_deadline_ms(),
        kind: Some(scenario.kind),
        faults: scenario.faults.clone(),
        expected_runs: scenario.workload.runs as usize,
        ..Default::default()
    };

    for (id, status, creating) in sqlx::query_as::<_, (String, String, bool)>(
        "SELECT id, status, creating FROM runs WHERE id = ANY($1)",
    )
    .bind(runs)
    .fetch_all(src.pg)
    .await
    .map_err(db_err)?
    {
        f.runs.push(RunRow { id, status, creating });
    }
    for id in runs {
        if !f.runs.iter().any(|r| &r.id == id) {
            f.runs.push(RunRow {
                id: id.clone(),
                status: "missing".into(),
                creating: false,
            });
        }
    }
    for (run, seq, at, payload) in sqlx::query_as::<_, (String, i64, i64, serde_json::Value)>(
        "SELECT run_id, seq, at, payload FROM events WHERE run_id = ANY($1) ORDER BY run_id, seq",
    )
    .bind(runs)
    .fetch_all(src.pg)
    .await
    .map_err(db_err)?
    {
        f.events.push(EventRow { run, seq, at, payload });
    }
    for (run, step, attempt, outcome, output_snapshot) in
        sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>)>(
            "SELECT run_id, step_id, attempt_id, outcome, output_snapshot FROM attempts WHERE run_id = ANY($1)",
        )
        .bind(runs)
        .fetch_all(src.pg)
        .await
        .map_err(db_err)?
    {
        f.attempts.push(AttemptRow {
            run,
            step,
            attempt,
            outcome,
            output_snapshot,
        });
    }
    for (run, step, status, output_snapshot, evidence_attempt) in
        sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>)>(
            "SELECT run_id, step_id, status, output_snapshot, evidence_attempt FROM step_runs WHERE run_id = ANY($1)",
        )
        .bind(runs)
        .fetch_all(src.pg)
        .await
        .map_err(db_err)?
    {
        f.steps.push(StepRow {
            run,
            step,
            status,
            output_snapshot,
            evidence_attempt,
        });
    }

    f.launch_claimers = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT claimed_by FROM outbox WHERE run_id = ANY($1) AND kind = $2 AND claimed_by IS NOT NULL",
    )
    .bind(runs)
    .bind(crate::servers::LAUNCH_STEP)
    .fetch_all(src.pg)
    .await
    .map_err(db_err)?
    .into_iter()
    .collect();

    // Every fence the scenario's Attempts could have drained into.
    let fences: Vec<String> = f
        .attempts
        .iter()
        .map(|a| scarab_workspace_client::drain_fence_key(&a.run, &a.step, &a.attempt))
        .collect();
    f.recorded_fences = sqlx::query_scalar::<_, String>(
        "SELECT fence_key FROM depot_drain_records WHERE fence_key = ANY($1)",
    )
    .bind(&fences)
    .fetch_all(src.pg)
    .await
    .map_err(db_err)?
    .into_iter()
    .collect();
    for (pack_key, fence_key, committed, created_at, bytes) in
        sqlx::query_as::<_, (String, String, bool, i64, i64)>(
            "SELECT pack_key, fence_key, committed, created_at, bytes FROM depot_packs WHERE fence_key = ANY($1)",
        )
        .bind(&fences)
        .fetch_all(src.pg)
        .await
        .map_err(db_err)?
    {
        let object = match src.bucket.head(&ObjPath::from(pack_key.as_str())).await {
            Ok(meta) => Ok(meta.size),
            Err(e) => Err(e.to_string()),
        };
        f.packs.push(PackRow {
            pack_key,
            fence_key,
            committed,
            created_at,
            bytes,
            object,
        });
    }

    // I4: green Attempts' snapshots (the Step's evidence included).
    let mut roots: Vec<(String, String, String, String)> = Vec::new();
    for a in &f.attempts {
        if a.outcome.as_deref() == Some("succeeded") {
            if let Some(root) = &a.output_snapshot {
                roots.push((a.run.clone(), a.step.clone(), a.attempt.clone(), root.clone()));
            }
        }
    }
    for s in &f.steps {
        if let (Some(root), Some(att)) = (&s.output_snapshot, &s.evidence_attempt) {
            let key = (s.run.clone(), s.step.clone(), att.clone(), root.clone());
            if !roots.contains(&key) {
                roots.push(key);
            }
        }
    }
    let mut verified: HashMap<RangeKey, Result<(), String>> = HashMap::new();
    for (run, step, attempt, root) in roots {
        let check = walk_snapshot(src, &run, &step, &attempt, &root, &mut verified).await?;
        f.snapshots.push(check);
    }

    // I8: what still exists for the scenario's Runs.
    let pods_api: KubeApi<Pod> = KubeApi::namespaced(src.kube.clone(), src.namespace);
    let all_pods = pods_api.list(&ListParams::default()).await.map_err(|e| format!("list pods: {e}"))?;
    let scenario_runs: HashSet<&str> = runs.iter().map(String::as_str).collect();
    let mut scenario_pod_names = HashSet::new();
    for r in &f.pod_records {
        if r.pod.run.as_deref().is_some_and(|x| scenario_runs.contains(x)) {
            scenario_pod_names.insert(r.pod.name.clone());
        }
    }
    for p in &all_pods.items {
        let name = p.metadata.name.clone().unwrap_or_default();
        f.live_pod_names.insert(name.clone());
        let run = p.metadata.labels.as_ref().and_then(|l| l.get("scarab.io/run")).cloned();
        if run.as_deref().is_some_and(|x| scenario_runs.contains(x)) {
            f.live.push(LiveObject {
                kind: "Pod".into(),
                name,
                run,
                phase: p.status.as_ref().and_then(|s| s.phase.clone()),
            });
        }
    }
    let lp = ListParams::default().labels("scarab.io/run");
    let svcs: KubeApi<Service> = KubeApi::namespaced(src.kube.clone(), src.namespace);
    for s in svcs.list(&lp).await.map_err(|e| format!("list services: {e}"))?.items {
        let run = s.metadata.labels.as_ref().and_then(|l| l.get("scarab.io/run")).cloned();
        if run.as_deref().is_some_and(|x| scenario_runs.contains(x)) {
            f.live.push(LiveObject {
                kind: "Service".into(),
                name: s.metadata.name.unwrap_or_default(),
                run,
                phase: None,
            });
        }
    }
    let nps: KubeApi<NetworkPolicy> = KubeApi::namespaced(src.kube.clone(), src.namespace);
    for n in nps.list(&lp).await.map_err(|e| format!("list networkpolicies: {e}"))?.items {
        let run = n.metadata.labels.as_ref().and_then(|l| l.get("scarab.io/run")).cloned();
        if run.as_deref().is_some_and(|x| scenario_runs.contains(x)) {
            f.live.push(LiveObject {
                kind: "NetworkPolicy".into(),
                name: n.metadata.name.unwrap_or_default(),
                run,
                phase: None,
            });
        }
    }
    // Per-Pod workspace Secrets (`<pod>-workspace`, owned by the Pod): one
    // whose Pod is gone has outlived it.
    let secrets: KubeApi<Secret> = KubeApi::namespaced(src.kube.clone(), src.namespace);
    for s in secrets.list(&ListParams::default()).await.map_err(|e| format!("list secrets: {e}"))?.items {
        let name = s.metadata.name.unwrap_or_default();
        if let Some(pod) = name.strip_suffix("-workspace") {
            if scenario_pod_names.contains(pod) && !f.live_pod_names.contains(pod) {
                f.live.push(LiveObject {
                    kind: "Secret".into(),
                    name,
                    run: None,
                    phase: None,
                });
            }
        }
    }
    f.now_ms = now_ms();
    Ok(f)
}

#[derive(Debug, Clone)]
struct MemberRow {
    pack_key: String,
    offset: i64,
    len: i64,
    fence_key: String,
    committed: bool,
}

async fn members(pg: &sqlx::PgPool, kind: &str, hex: &str) -> Result<Vec<MemberRow>, String> {
    let rows = sqlx::query_as::<_, (String, i64, i64, String, bool)>(
        "SELECT m.pack_key, m.byte_offset, m.byte_len, p.fence_key, p.committed \
         FROM depot_pack_members m JOIN depot_packs p ON p.pack_key = m.pack_key \
         WHERE m.address = $1 AND m.kind = $2",
    )
    .bind(scarab_storage::tagged_address(scarab_storage::HashAlgo::Sha256, hex))
    .bind(kind)
    .fetch_all(pg)
    .await
    .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .map(|(pack_key, offset, len, fence_key, committed)| MemberRow {
            pack_key,
            offset,
            len,
            fence_key,
            committed,
        })
        .collect())
}

async fn read_member(bucket: &object_store::aws::AmazonS3, m: &MemberRow) -> Result<Vec<u8>, String> {
    let (off, len) = (u64::try_from(m.offset).map_err(|e| e.to_string())?, u64::try_from(m.len).map_err(|e| e.to_string())?);
    bucket
        .get_range(&ObjPath::from(m.pack_key.as_str()), off..off + len)
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("GET {}[{off}..+{len}]: {e}", m.pack_key))
}

/// Pick the committed member this snapshot may read: one in its own fence's
/// pack, or in a fence it has a borrow edge to (git-bug ec294b7).
fn pick<'m>(rows: &'m [MemberRow], own: &str, borrows: &HashSet<String>) -> Result<&'m MemberRow, String> {
    let committed: Vec<&MemberRow> = rows.iter().filter(|m| m.committed).collect();
    if committed.is_empty() {
        return Err(if rows.is_empty() {
            "in no pack".into()
        } else {
            format!("only in uncommitted packs {:?}", rows.iter().map(|m| &m.pack_key).collect::<Vec<_>>())
        });
    }
    committed
        .iter()
        .find(|m| m.fence_key == own)
        .or_else(|| committed.iter().find(|m| borrows.contains(&m.fence_key)))
        .copied()
        .ok_or_else(|| {
            format!(
                "only in other fences' packs {:?} with no depot_fence_borrows edge from this fence",
                committed.iter().map(|m| &m.fence_key).collect::<Vec<_>>()
            )
        })
}

async fn walk_snapshot(
    src: &Sources<'_>,
    run: &str,
    step: &str,
    attempt: &str,
    root: &str,
    verified: &mut HashMap<RangeKey, Result<(), String>>,
) -> Result<SnapshotCheck, String> {
    let mut out = SnapshotCheck {
        run: run.into(),
        step: step.into(),
        attempt: attempt.into(),
        root: root.into(),
        ..Default::default()
    };
    let own = scarab_workspace_client::drain_fence_key(run, step, attempt);
    let borrows: HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT owner_fence FROM depot_fence_borrows WHERE borrower_fence = $1",
    )
    .bind(&own)
    .fetch_all(src.pg)
    .await
    .map_err(db_err)?
    .into_iter()
    .collect();
    let root_hex = scarab_storage::parse_address(root).map(|(_, h)| h.to_string()).unwrap_or_else(|_| root.to_string());

    // Trees first, one level at a time; blobs collected and read after.
    let mut trees = vec![root_hex];
    let mut seen = HashSet::new();
    let mut blobs: Vec<String> = Vec::new();
    while let Some(hex) = trees.pop() {
        if !seen.insert(format!("t{hex}")) {
            continue;
        }
        out.objects += 1;
        let rows = members(src.pg, "tree", &hex).await?;
        let m = match pick(&rows, &own, &borrows) {
            Ok(m) => m.clone(),
            Err(e) => {
                out.problems.push(format!("tree {hex} is {e}"));
                continue;
            }
        };
        let bytes = match read_member(src.bucket, &m).await {
            Ok(b) => b,
            Err(e) => {
                out.problems.push(format!("tree {hex}: {e}"));
                continue;
            }
        };
        if scarab_storage::sha256_hex(&bytes) != hex {
            out.problems.push(format!("tree {hex}: bytes at {}[{}..+{}] hash to something else", m.pack_key, m.offset, m.len));
            continue;
        }
        out.bytes += bytes.len() as u64;
        let entries: Vec<scarab_storage::TreeEntry> = match serde_json::from_slice(&bytes) {
            Ok(e) => e,
            Err(e) => {
                out.problems.push(format!("tree {hex} does not parse: {e}"));
                continue;
            }
        };
        for e in entries {
            match e.target {
                scarab_storage::TreeTarget::Tree(t) => trees.push(t.0),
                scarab_storage::TreeTarget::Blob(b) => {
                    if seen.insert(format!("b{}", b.0)) {
                        blobs.push(b.0)
                    }
                }
            }
        }
    }
    let mut picked = Vec::new();
    for hex in blobs {
        out.objects += 1;
        let rows = members(src.pg, "blob", &hex).await?;
        match pick(&rows, &own, &borrows) {
            Ok(m) => picked.push((hex, m.clone())),
            Err(e) => out.problems.push(format!("blob {hex} is {e}")),
        }
    }
    // Read every blob range back and hash it, eight at a time; a range read
    // for an earlier snapshot is not read again.
    let todo: Vec<(String, MemberRow)> = picked
        .iter()
        .filter(|(_, m)| !verified.contains_key(&(m.pack_key.clone(), m.offset, m.len)))
        .cloned()
        .collect();
    let results: Vec<(RangeKey, Result<(), String>, u64)> = futures::stream::iter(todo)
        .map(|(hex, m)| async move {
            let key = (m.pack_key.clone(), m.offset, m.len);
            match read_member(src.bucket, &m).await {
                Ok(b) if scarab_storage::sha256_hex(&b) == hex => (key, Ok(()), b.len() as u64),
                Ok(_) => (key, Err(format!("blob {hex}: bytes at {}[{}..+{}] hash to something else", m.pack_key, m.offset, m.len)), 0),
                Err(e) => (key, Err(format!("blob {hex}: {e}")), 0),
            }
        })
        .buffer_unordered(8)
        .collect()
        .await;
    for (key, res, n) in results {
        out.bytes += n;
        verified.insert(key, res);
    }
    for (_, m) in &picked {
        if let Some(Err(e)) = verified.get(&(m.pack_key.clone(), m.offset, m.len)) {
            out.problems.push(e.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pods::{ContainerView, PodView};

    fn ev(run: &str, seq: i64, payload: serde_json::Value) -> EventRow {
        EventRow {
            run: run.into(),
            seq,
            at: seq,
            payload,
        }
    }

    fn pod_rec(uid: &str, att: &str, start: i64, end: Option<i64>) -> PodRecord {
        PodRecord {
            at_ms: start,
            event: "modified".into(),
            pod: PodView {
                uid: uid.into(),
                name: format!("p-{uid}"),
                run: Some("r".into()),
                step: Some("s".into()),
                attempt: Some(att.into()),
                component: None,
                phase: "Running".into(),
                ready: true,
                deleting: false,
                has_root: false,
                containers: vec![ContainerView {
                    name: pods::STEP_CONTAINER.into(),
                    init: false,
                    state: if end.is_some() { "terminated" } else { "running" }.into(),
                    started_at_ms: Some(start),
                    finished_at_ms: end,
                    exit_code: end.map(|_| 0),
                    restarts: 0,
                }],
                grace_secs: Some(30),
            },
        }
    }

    fn base() -> Facts {
        Facts {
            runs: vec![RunRow {
                id: "r".into(),
                status: "succeeded".into(),
                creating: false,
            }],
            events: vec![
                ev("r", 1, serde_json::json!({"AttemptStarted": {"step": "s", "attempt": "a1"}})),
                ev("r", 2, serde_json::json!({"AttemptFinished": {"step": "s", "attempt": "a1", "failure": null}})),
                ev("r", 3, serde_json::json!({"StepTransitioned": {"step": "s", "from": "Running", "to": "Succeeded"}})),
                ev("r", 4, serde_json::json!({"RunTransitioned": {"from": "Running", "to": "Succeeded"}})),
            ],
            attempts: vec![AttemptRow {
                run: "r".into(),
                step: "s".into(),
                attempt: "a1".into(),
                outcome: Some("succeeded".into()),
                output_snapshot: Some("abc".into()),
            }],
            steps: vec![StepRow {
                run: "r".into(),
                step: "s".into(),
                status: "succeeded".into(),
                output_snapshot: Some("abc".into()),
                evidence_attempt: Some("a1".into()),
            }],
            pod_records: vec![pod_rec("u1", "a1", 10, Some(20))],
            quiesce_deadline_ms: 1_000,
            now_ms: 2_000_000,
            ..Default::default()
        }
    }

    fn failed(v: &Verdict) -> Vec<&str> {
        v.findings
            .iter()
            .filter(|f| f.severity == Severity::Fail)
            .map(|f| f.invariant.as_str())
            .collect()
    }

    #[test]
    fn a_clean_run_passes() {
        let v = evaluate(&base());
        assert!(v.pass, "{:?}", v.findings);
    }

    #[test]
    fn two_pods_for_one_attempt_fail_i1a() {
        let mut f = base();
        f.pod_records.push(pod_rec("u2", "a1", 30, Some(40)));
        assert_eq!(failed(&evaluate(&f)), vec!["I1a"]);
    }

    #[test]
    fn overlapping_attempts_fail_i1b_unless_the_first_was_journalled_ended() {
        let mut f = base();
        f.pod_records.push(pod_rec("u2", "a2", 15, Some(40)));
        f.events.push(ev("r", 5, serde_json::json!({"AttemptStarted": {"step": "s", "attempt": "a2"}})));
        // a1 is journalled finished at seq 2, before a2 started at seq 5: allowed.
        let v = evaluate(&f);
        assert!(failed(&v).is_empty(), "{:?}", v.findings);
        assert!(v.findings.iter().any(|x| x.invariant == "I1b" && x.severity == Severity::Warn));
        // Without the journal entry it is a double effect.
        f.events.retain(|e| event_kind(&e.payload) != "AttemptFinished");
        assert!(failed(&evaluate(&f)).contains(&"I1b"));
    }

    /// a1 was journalled ended and its Pod marked for deletion at 1 000 with
    /// a 30 s grace period: the bound is 60 000 ms from there. a2 starts at
    /// 500. a1's step container finishing at `end` decides the verdict.
    fn i1b_bound_facts(end: i64) -> Facts {
        let mut f = base();
        f.pod_records = vec![pod_rec("u1", "a1", 10, None), pod_rec("u2", "a2", 500, Some(70_000))];
        let mut deleting = pod_rec("u1", "a1", 10, None);
        deleting.at_ms = 1_000;
        deleting.pod.deleting = true;
        f.pod_records.push(deleting);
        f.pod_records.push(pod_rec("u1", "a1", 10, Some(end)));
        f.events.push(ev("r", 5, serde_json::json!({"AttemptStarted": {"step": "s", "attempt": "a2"}})));
        f
    }

    #[test]
    fn i1b_allows_an_overlap_up_to_the_grace_period_plus_30_s_after_removal() {
        let v = evaluate(&i1b_bound_facts(1_000 + 60_000));
        assert!(failed(&v).is_empty(), "{:?}", v.findings);
        assert!(v.findings.iter().any(|x| x.invariant == "I1b" && x.severity == Severity::Warn));
    }

    #[test]
    fn i1b_fails_an_overlap_past_the_grace_period_plus_30_s_after_removal() {
        let v = evaluate(&i1b_bound_facts(1_000 + 60_001));
        assert_eq!(failed(&v), vec!["I1b"], "{:?}", v.findings);
        assert!(v.findings[0].detail.contains("past the bound of 60000 ms"), "{}", v.findings[0].detail);
    }

    /// The bound uses the Pod's own grace period: at 600 s the same overrun
    /// is within it.
    #[test]
    fn i1b_bound_follows_the_pods_grace_period() {
        let mut f = i1b_bound_facts(1_000 + 60_001);
        for r in &mut f.pod_records {
            r.pod.grace_secs = Some(600);
        }
        assert!(failed(&evaluate(&f)).is_empty());
        // A record written before the field existed falls back to 30 s.
        for r in &mut f.pod_records {
            r.pod.grace_secs = None;
        }
        assert_eq!(failed(&evaluate(&f)), vec!["I1b"]);
    }

    /// The Pod gone before a2 started allows the start without a journal
    /// entry; a Pod still there and no journal entry does not.
    #[test]
    fn i1b_allows_a_start_after_the_pod_is_gone() {
        let mut f = i1b_bound_facts(30_000);
        f.events.retain(|e| event_kind(&e.payload) != "AttemptFinished");
        assert_eq!(failed(&evaluate(&f)), vec!["I1b"]);
        let mut gone = pod_rec("u1", "a1", 10, None);
        gone.at_ms = 400;
        gone.event = "deleted".into();
        f.pod_records.push(gone);
        let v = evaluate(&f);
        assert!(failed(&v).is_empty(), "{:?}", v.findings);
    }

    #[test]
    fn two_green_attempts_fail_i1c() {
        let mut f = base();
        f.attempts.push(AttemptRow {
            attempt: "a2".into(),
            ..f.attempts[0].clone()
        });
        assert!(failed(&evaluate(&f)).contains(&"I1c"));
    }

    #[test]
    fn two_terminal_transitions_fail_t1_and_a_late_one_fails_i3() {
        let mut f = base();
        f.events.push(ev("r", 9, serde_json::json!({"RunTransitioned": {"from": "Succeeded", "to": "Failed"}})));
        assert!(failed(&evaluate(&f)).contains(&"T1"));
        let mut f = base();
        f.quiesce_deadline_ms = 2;
        assert_eq!(failed(&evaluate(&f)), vec!["I3"]);
        let mut f = base();
        f.runs[0].status = "running".into();
        f.events.pop();
        assert_eq!(failed(&evaluate(&f)), vec!["I3"]);
        let mut f = base();
        f.runs[0].creating = true;
        assert_eq!(failed(&evaluate(&f)), vec!["I3"]);
    }

    #[test]
    fn a_row_the_journal_does_not_fold_to_fails_i5() {
        // The step row went terminal with no journalled transition
        // (git-bug 586187a: the CAS and its event in separate writes).
        let mut f = base();
        f.events.retain(|e| event_kind(&e.payload) != "StepTransitioned");
        let v = evaluate(&f);
        assert_eq!(failed(&v), vec!["I5"], "{:?}", v.findings);
        assert!(v.findings[0].detail.contains("folds to pending, step_runs.status is succeeded"));
        // The journal says Failed, the row says succeeded.
        let mut f = base();
        f.events[2] = ev("r", 3, serde_json::json!({"StepTransitioned": {"step": "s", "from": "Running", "to": "Failed"}}));
        assert_eq!(failed(&evaluate(&f)), vec!["I5"]);
        // A later rerun's transition wins the fold: Succeeded, then
        // re-armed to Pending, while the row says pending.
        let mut f = base();
        f.events.push(ev("r", 9, serde_json::json!({"StepTransitioned": {"step": "s", "from": "Succeeded", "to": "Pending"}})));
        f.steps[0].status = "pending".into();
        assert!(!evaluate(&f).findings.iter().any(|x| x.invariant == "I5"));
    }

    #[test]
    fn snapshot_problems_fail_i4() {
        let mut f = base();
        f.snapshots.push(SnapshotCheck {
            problems: vec!["blob x is in no pack".into()],
            ..Default::default()
        });
        assert_eq!(failed(&evaluate(&f)), vec!["I4"]);
    }

    #[test]
    fn running_leftovers_fail_i8_and_completed_pods_do_not() {
        let mut f = base();
        f.live.push(LiveObject {
            kind: "Pod".into(),
            name: "p".into(),
            run: Some("r".into()),
            phase: Some("Succeeded".into()),
        });
        assert!(evaluate(&f).pass);
        f.live[0].phase = Some("Running".into());
        assert_eq!(failed(&evaluate(&f)), vec!["I8"]);
    }

    #[test]
    fn i10_stale_staging_fails_only_for_recorded_fences_and_missing_objects_fail() {
        let mut f = base();
        f.packs.push(PackRow {
            pack_key: "packs/f/x.pack".into(),
            fence_key: "f".into(),
            committed: false,
            created_at: 1,
            bytes: 10,
            object: Ok(10),
        });
        let v = evaluate(&f);
        assert!(v.pass && v.findings.iter().any(|x| x.invariant == "I10"));
        f.recorded_fences.insert("f".into());
        assert_eq!(failed(&evaluate(&f)), vec!["I10"]);
        let mut f = base();
        f.packs.push(PackRow {
            pack_key: "packs/f/y.pack".into(),
            fence_key: "f".into(),
            committed: true,
            created_at: 1,
            bytes: 10,
            object: Err("not found".into()),
        });
        assert_eq!(failed(&evaluate(&f)), vec!["I10"]);
    }

    #[test]
    fn pick_prefers_own_fence_then_borrows() {
        let row = |fence: &str, committed: bool| MemberRow {
            pack_key: format!("packs/{fence}/p"),
            offset: 0,
            len: 1,
            fence_key: fence.into(),
            committed,
        };
        let none = HashSet::new();
        assert!(pick(&[], "own", &none).unwrap_err().contains("no pack"));
        assert!(pick(&[row("own", false)], "own", &none).unwrap_err().contains("uncommitted"));
        assert_eq!(pick(&[row("x", true), row("own", true)], "own", &none).unwrap().fence_key, "own");
        assert!(pick(&[row("x", true)], "own", &none).unwrap_err().contains("borrows"));
        let b: HashSet<String> = ["x".to_string()].into();
        assert_eq!(pick(&[row("x", true)], "own", &b).unwrap().fence_key, "x");
    }
}
