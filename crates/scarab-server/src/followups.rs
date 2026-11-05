//! `on: upstream:` follow-ups (git-bug 149b4db): when a Run finishes, start
//! the Pipelines in the same repository that follow it.
//!
//! A Run's move to `Succeeded` or `Failed` writes a
//! [`RUN_FINISHED`](scarab_engine::RUN_FINISHED) outbox message in the same
//! commit. [`drain_followups`] claims those messages, builds an
//! [`Event::Upstream`] from the finished Run's record, reads the Pipelines
//! that follow it at the Run's commit, and starts each one through the same
//! compile and admission path a push takes.
//!
//! - Same repository only. The follow-up builds the upstream Run's commit
//!   and runs on its ref, so an Environment's allowed refs apply to it.
//! - A Run started by a fork pull request starts no follow-up.
//! - A Run reopened by a Rerun since the message was written starts none
//!   for that older verdict: the Rerun's own ending is a new message.
//! - Delivery is at-least-once. A follow-up's Run id is derived from the
//!   upstream Run, its verdict and the pipeline, so a redelivery finds the
//!   Run it already started.
//! - A pipeline already in the chain, or a chain at
//!   [`MAX_UPSTREAM_CHAIN`](scarab_pipeline::MAX_UPSTREAM_CHAIN), is a
//!   refused Run.

use std::time::Duration;

use scarab_engine::{
    Clock, Db, DbError, EventPayload, OutboxMessage, RunId, RunStatus, MAX_DELIVERY_ATTEMPTS,
    RUN_FINISHED,
};
use scarab_forge::{Event, ForgeError, ForgePort, RepoRef};
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::{
    read_event_pipelines, trigger_runs_from_files, AdmissionPorts, AppState, TriggerError,
};

/// How often the drain runs.
const TICK_MS: u64 = 2_000;

/// How many messages one pass claims.
const BATCH: u32 = 32;

/// How long a claimed message is hidden from other replicas: long enough to
/// read and compile the follow-ups and create their Runs.
const VISIBILITY_MS: i64 = 60_000;

/// Run [`drain_followups`] every [`TICK_MS`] on every replica; the outbox
/// lease keeps two replicas off one message.
pub fn spawn(st: AppState, owner: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Some(forge) = st.forge.clone() {
                if let Err(e) = drain_followups(
                    forge.as_ref(),
                    st.db.as_ref(),
                    st.clock.as_ref(),
                    st.admission_ports(),
                    &owner,
                    BATCH,
                    VISIBILITY_MS,
                )
                .await
                {
                    tracing::warn!(error = %e, "upstream follow-up drain failed");
                }
            }
            tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
        }
    })
}

/// Claim up to `limit` `run_finished` messages and start the follow-ups of
/// each. A message is retired once its follow-ups are started, refused or
/// found to be none. One that fails (the forge is down, a database error)
/// stays for redelivery and is dead-lettered after
/// [`MAX_DELIVERY_ATTEMPTS`]. Returns how many Runs were started.
pub async fn drain_followups(
    forge: &dyn ForgePort,
    db: &dyn Db,
    clock: &dyn Clock,
    ports: AdmissionPorts<'_>,
    owner: &str,
    limit: u32,
    visibility_ms: i64,
) -> Result<usize, DbError> {
    let msgs = db
        .claim_outbox(owner, Some(RUN_FINISHED), limit, visibility_ms)
        .await?;
    let mut started = 0;
    for msg in msgs {
        match follow(forge, db, clock, ports, &msg).await {
            Ok(n) => {
                db.mark_dispatched(msg.id).await?;
                started += n;
            }
            Err(e) => {
                let failures = db.record_outbox_failure(msg.id).await?;
                tracing::warn!(
                    run = %msg.run.0,
                    failures,
                    error = %e,
                    "starting upstream follow-ups failed; left on the outbox for redelivery"
                );
                if failures >= MAX_DELIVERY_ATTEMPTS {
                    db.dead_letter_outbox(msg.id).await?;
                    tracing::error!(
                        run = %msg.run.0,
                        failures,
                        error = %e,
                        "upstream follow-ups dead-lettered: they will not start"
                    );
                }
            }
        }
    }
    Ok(started)
}

/// Start the follow-ups of one `run_finished` message; the number started.
async fn follow(
    forge: &dyn ForgePort,
    db: &dyn Db,
    clock: &dyn Clock,
    ports: AdmissionPorts<'_>,
    msg: &OutboxMessage,
) -> Result<usize, TriggerError> {
    let Some(event) = upstream_event(db, msg).await? else {
        return Ok(0);
    };
    let Event::Upstream {
        repo,
        sha,
        pipeline,
        ..
    } = &event
    else {
        return Ok(0);
    };
    let files = match recorded_followups(db, &msg.run).await? {
        // Recorded at creation: read only those files.
        Some(paths) if paths.is_empty() => return Ok(0),
        Some(paths) => read_files(forge, repo, sha, &paths).await?,
        // Not recorded (a dispatched or cron Run): read `.scarab/`.
        None => read_event_pipelines(forge, &event).await?.files,
    };
    // Only the files that follow this pipeline are compiled.
    let files: Vec<(String, String)> = files
        .into_iter()
        .filter(|(_, yaml)| {
            scarab_pipeline::upstream_pipeline_of(yaml).as_deref() == Some(pipeline.as_str())
        })
        .collect();
    if files.is_empty() {
        return Ok(0);
    }
    // `files` is not the whole of `.scarab/`, so the follow-ups record
    // nothing of their own: each one's ending reads `.scarab/` itself.
    let outcome = trigger_runs_from_files(forge, db, clock, ports, &event, &files, false).await?;
    Ok(outcome.runs.len())
}

/// The `upstream` event of a `run_finished` message, or `None` when the
/// finished Run starts no follow-up.
async fn upstream_event(db: &dyn Db, msg: &OutboxMessage) -> Result<Option<Event>, DbError> {
    let run = &msg.run;
    let status = match msg
        .payload
        .get("to")
        .and_then(|v| serde_json::from_value::<RunStatus>(v.clone()).ok())
    {
        Some(RunStatus::Succeeded) => "succeeded",
        Some(RunStatus::Failed) => "failed",
        _ => return Ok(None),
    };
    let Some(generation) = msg.payload.get("generation").and_then(Value::as_u64) else {
        return Ok(None);
    };
    // A Rerun reopened the Run since: its own ending is the news.
    if generation < db.run_reopen_generation(run).await? {
        return Ok(None);
    }
    let Some(pipeline) = db.run_pipeline(run).await? else {
        return Ok(None);
    };
    let Some((trigger, actor)) = trigger_of(db, run).await? else {
        return Ok(None);
    };
    // A fork pull request's Run never starts a follow-up.
    if trigger["fork"].as_bool() == Some(true) {
        return Ok(None);
    }
    let (Some(owner), Some(name), Some(sha)) = (
        trigger["repo"]["owner"].as_str(),
        trigger["repo"]["name"].as_str(),
        trigger["sha"].as_str().filter(|s| !s.is_empty()),
    ) else {
        return Ok(None);
    };
    let r#ref = match (trigger["ref"].as_str(), trigger["number"].as_u64()) {
        (Some(r), _) => r.to_string(),
        (None, Some(n)) => format!("refs/pull/{n}/head"),
        (None, None) => String::new(),
    };
    let mut chain: Vec<String> = match trigger["kind"].as_str() {
        Some("upstream") => trigger["chain"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    chain.push(pipeline.clone());
    Ok(Some(Event::Upstream {
        repo: RepoRef {
            owner: owner.to_string(),
            name: name.to_string(),
        },
        run: run.0.clone(),
        generation,
        pipeline,
        status: status.to_string(),
        r#ref,
        sha: sha.to_string(),
        actor: actor.unwrap_or_default(),
        chain,
    }))
}

/// The trigger context (`event`) a Run was created from, and its actor.
async fn trigger_of(
    db: &dyn Db,
    run: &RunId,
) -> Result<Option<(Value, Option<String>)>, DbError> {
    for e in db.events(run).await? {
        if let EventPayload::Raw(v) = &e.kind {
            if v["trigger"]["event"].is_object() {
                let actor = v["actor"].as_str().map(str::to_string);
                return Ok(Some((v["trigger"]["event"].clone(), actor)));
            }
        }
    }
    Ok(None)
}

/// The follow-up files recorded when the Run was created, if they were.
async fn recorded_followups(db: &dyn Db, run: &RunId) -> Result<Option<Vec<String>>, DbError> {
    for e in db.events(run).await? {
        if let EventPayload::Raw(v) = &e.kind {
            if let Some(paths) = v["followups"].as_array() {
                return Ok(Some(
                    paths
                        .iter()
                        .filter_map(|p| p.as_str().map(str::to_string))
                        .collect(),
                ));
            }
        }
    }
    Ok(None)
}

/// Read `paths` at `sha`. A file gone since is left out; any other forge
/// error fails the message, to be retried.
async fn read_files(
    forge: &dyn ForgePort,
    repo: &RepoRef,
    sha: &str,
    paths: &[String],
) -> Result<Vec<(String, String)>, TriggerError> {
    let mut files = Vec::new();
    for path in paths {
        match forge.read_file_at_ref(repo, sha, path).await {
            Ok(bytes) => {
                let yaml = String::from_utf8(bytes).map_err(|_| TriggerError::NotUtf8)?;
                files.push((path.clone(), yaml));
            }
            Err(ForgeError::NotFound(_)) => {}
            Err(e) => return Err(TriggerError::Forge(e)),
        }
    }
    Ok(files)
}
