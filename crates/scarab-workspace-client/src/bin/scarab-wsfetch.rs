//! `scarab-wsfetch` — the ADR-0061 s3-feed workspace fetcher.
//!
//! One short-lived init container per Step Pod. It reads the Step's input
//! **Workspace Snapshot** roots from its own environment, presents the tmpfs
//! workspace token, and materialises each root — **in order** — into
//! `/workspace`. Then it exits. There is no marker file, no wait loop, and no
//! `kubectl exec` anywhere in the path.
//!
//! # What this replaces, and why it is not a performance story
//!
//! Before this binary, `drive_workspace` materialised every input into a tempdir
//! on the control plane, tarred it, and streamed the tar into a `busybox`
//! doorstop over `kubectl exec`. ADR-0061's s0 measurement priced that tunnel at
//! **4–15%** of a Step boundary, so deleting it is not where the wall-clock is.
//! The justification is structural:
//!
//! - bulk workspace data no longer crosses the **Kubernetes API server**, a
//!   control-plane component never intended for it;
//! - the control plane no longer buffers and re-writes the whole workspace
//!   through its page cache (s2 measured that tmpdir round-trip *growing* to
//!   ~4.7 s once the CAS leg got faster — it has to be removed, not accelerated);
//! - the `exec` tar path could publish a **partial tree** as a Step's
//!   authoritative snapshot (git-bug `a3e7845`); a failed fetch here fails the
//!   init container, so the Step never starts;
//! - and it is the prerequisite for **lazy materialisation** (ADR-0061 part 2),
//!   which *is* the load-bearing part.
//!
//! # Eager, and loudly so
//!
//! This is a **stepping stone**, not an endpoint (ADR-0061 D2.3): it still moves
//! the whole snapshot to every fresh node. The node driver replaces it, and the
//! ticket that closes only when `docker/wsfetch/` is deleted is git-bug
//! `0628369`. So every invocation prints
//! `mode=eager (ADR-0061 s3-feed stepping stone — the node driver replaces this)`
//! into the Step Pod's own logs — a stone you can stand on forever is a floor,
//! and this line is what stops it becoming one silently.
//!
//! # It reuses `materialize`; it does not reimplement it
//!
//! [`WorkspaceClient::materialize`](scarab_workspace_client::WorkspaceClient)
//! already carries the fidelity rules ADR-0061's s7 slice fixed and pinned:
//! unlink-before-write, directories widened for the walk then restored deepest
//! first, mtime-then-mode ordering, symlinks as blobs. Re-implementing any of
//! that here would fork the contract that
//! `crates/scarab-storage-s3/tests/fidelity.rs` and
//! `crates/scarab-workspace-client/tests/service_roundtrip.rs` exist to hold.
//!
//! # Merge-in-order is load-bearing
//!
//! Roots are materialised in the order the executor listed them, so a later
//! input overlays an earlier one (ADR-0007). That is not a detail:
//! `crates/scarab-storage-s3/tests/fidelity.rs::a_later_input_overlays_a_read_only_checkout`
//! exists because a read-only file from input A must be replaceable by input B.
//!
//! # The group-writability contract (git-bug b04697f)
//!
//! Under the ADR-0039 restricted baseline every capability — `DAC_OVERRIDE`
//! included — is dropped, so **group membership is the only thing** that lets a
//! Step write its own workspace. The old feed path ran
//! `chmod -R g+rwX /workspace` + setgid-on-dirs over `exec` after untarring,
//! precisely for that. This binary does the same as a final pass, in-process:
//! see [`widen_for_the_group`].
//!
//! # Exit codes
//!
//! | code | meaning | what the executor does |
//! |---|---|---|
//! | 0 | the workspace is provisioned | the Step runs |
//! | 1 | transient (service unreachable, 5xx, I/O) | the Pod fails; the engine's bounded retry may land elsewhere |
//! | 2 | permanent (a snapshot the store does not have) | same class, and the message says it is permanent |
//!
//! Both failures surface as `Infra { never_started: true }` — the Step's main
//! process never ran, so no side effect is possible. That is deliberately the
//! same verdict the deleted control-plane path produced for
//! `DriveErr::InputMissing`.

use std::os::unix::fs::PermissionsExt;

use scarab_storage::{StorageError, TreeHash};
use scarab_workspace_client::WorkspaceClient;

/// The env var naming the tmpfs file holding the workspace token. Must agree
/// with `scarab_executor_k8s::workspace_token::WORKSPACE_TOKEN_FILE_ENV`;
/// duplicated as a literal rather than imported because this binary must not
/// link the *kubernetes executor* to read a file (the node driver will reuse
/// this crate too).
const TOKEN_FILE_ENV: &str = "SCARAB_WORKSPACE_TOKEN_FILE";
/// The env var carrying the workspace service's base URL.
const URL_ENV: &str = "SCARAB_WORKSPACE_URL";
/// Comma-separated snapshot roots, **in merge order**.
const INPUTS_ENV: &str = "SCARAB_WORKSPACE_INPUTS";
/// Where to build the Workspace. Overridable only so the binary is testable.
const TARGET_ENV: &str = "SCARAB_WORKSPACE_TARGET";
const DEFAULT_TARGET: &str = "/workspace";

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(FetchError::Permanent(msg)) => {
            eprintln!("scarab-wsfetch: PERMANENT: {msg}");
            2
        }
        Err(FetchError::Transient(msg)) => {
            eprintln!("scarab-wsfetch: {msg}");
            1
        }
    };
    std::process::exit(code);
}

enum FetchError {
    /// The snapshot cannot ever be provisioned — retrying the identical spec
    /// cannot fix it.
    Permanent(String),
    /// Anything else: unreachable service, 5xx, local I/O.
    Transient(String),
}

impl From<std::io::Error> for FetchError {
    fn from(e: std::io::Error) -> Self {
        FetchError::Transient(e.to_string())
    }
}

fn run() -> Result<(), FetchError> {
    let target = env_or(TARGET_ENV, DEFAULT_TARGET);
    let roots: Vec<String> = env_or(INPUTS_ENV, "")
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .collect();

    // Guard #1 of ADR-0061 D2.3's three anti-calcification guards: this line is
    // printed into every Step Pod's own log, on every Pod, forever — until the
    // node driver deletes this binary. Observable, not silent.
    println!(
        "scarab-wsfetch: mode=eager (ADR-0061 s3-feed stepping stone — the node driver \
         replaces this) inputs={} target={target}",
        roots.len()
    );

    if roots.is_empty() {
        // Nothing to provision. The executor does not schedule this container in
        // that case, so reaching here means a Pod spec drifted — say so and
        // succeed, because an empty workspace is exactly what a no-`needs` Step
        // is entitled to.
        println!("scarab-wsfetch: no input snapshots — leaving the workspace empty");
        return Ok(());
    }

    let base = std::env::var(URL_ENV).map_err(|_| {
        FetchError::Permanent(format!(
            "{URL_ENV} is not set — this Pod has input snapshots but no workspace service \
             to fetch them from (ADR-0061)"
        ))
    })?;
    let token_file = std::env::var(TOKEN_FILE_ENV).map_err(|_| {
        FetchError::Permanent(format!(
            "{TOKEN_FILE_ENV} is not set — the workspace token is delivered on tmpfs and is \
             never read from env (ADR-0061)"
        ))
    })?;

    // A multi-thread runtime, not `current_thread`: `materialize` overlaps blob
    // downloads with `spawn_blocking` filesystem writes, and on one thread the
    // two would serialise — which is the property this binary exists to have.
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| FetchError::Transient(format!("tokio runtime: {e}")))?;
    runtime.block_on(fetch(&base, &token_file, &roots, &target))?;

    widen_for_the_group(std::path::Path::new(&target))?;
    println!("scarab-wsfetch: workspace provisioned at {target}");
    Ok(())
}

async fn fetch(
    base: &str,
    token_file: &str,
    roots: &[String],
    target: &str,
) -> Result<(), FetchError> {
    let client = WorkspaceClient::from_token_file(base, token_file)
        .map_err(|e| FetchError::Transient(format!("workspace token: {e}")))?;
    for (i, root) in roots.iter().enumerate() {
        let started = std::time::Instant::now();
        // Merge-in-order (ADR-0007): later roots overlay earlier ones, so this
        // loop is sequential ON PURPOSE. Parallelising it would make the result
        // depend on which download finished first.
        scarab_storage::Cas::materialize(&client, &TreeHash(root.clone()), target)
            .await
            .map_err(|e| match e {
                StorageError::NotFound => FetchError::Permanent(format!(
                    "input snapshot {root} is not in the workspace service or its cold \
                     archive — this Step can never be provisioned (evicted, or the store \
                     was wiped)"
                )),
                other => FetchError::Transient(format!("materialize {root}: {other}")),
            })?;
        println!(
            "scarab-wsfetch: input {}/{} {} materialised in {} ms",
            i + 1,
            roots.len(),
            root,
            started.elapsed().as_millis()
        );
    }
    Ok(())
}

/// Make the restored workspace **group**-writable, and make directories setgid so
/// files the Step creates stay in that group for the drain.
///
/// Which group is not this process's choice: the snapshot lands owned by whatever
/// gid this container runs as (65532, matching the executor's `WORKSPACE_GID`), and
/// every Step is put in that group via `supplementalGroups`. So widening the group
/// bits is exactly and only what makes the workspace writable.
///
/// This is `chmod -R g+rwX` + `find -type d -exec chmod g+s` — verbatim the two
/// commands the deleted `exec` feed ran, and for verbatim the same reason
/// (git-bug b04697f). A Step runs as whatever uid its image or governance grant
/// dictates, the snapshot arrives owned by *this* process's uid, and the ADR-0039
/// baseline has dropped `DAC_OVERRIDE`. Without this pass the first write in the
/// single most common CI shape — "clone, then build in the workspace" — is
/// `Permission denied`.
///
/// `X` is the standard `chmod` "execute only if it is a directory or already has
/// some execute bit" semantics, so a data file does not become executable.
///
/// Deliberately **after** `materialize`, never interleaved: the mode a snapshot
/// records is the mode `materialize` must restore (ADR-0061 s7 fidelity), and
/// widening is a separate, later, additive act. Symlinks are skipped — a link's
/// own mode is meaningless and `chmod` would follow it out of the workspace.
fn widen_for_the_group(root: &std::path::Path) -> Result<(), std::io::Error> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let is_dir = meta.is_dir();
        let mode = meta.permissions().mode() & 0o7777;
        let mut widened = mode | 0o060; // g+rw
        if is_dir || mode & 0o111 != 0 {
            widened |= 0o010; // g+X
        }
        if is_dir {
            widened |= 0o2000; // g+s — the drain must see the group survive
        }
        if widened != mode {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(widened))?;
        }
        if is_dir {
            for entry in std::fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        }
    }
    Ok(())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The b04697f contract, at the grain the fetcher owns it: after the pass a
    /// group member can read, write and traverse everything, directories are
    /// setgid, a data file does NOT become executable, and an executable stays
    /// executable.
    #[test]
    fn widening_grants_the_group_exactly_what_a_step_needs() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("dist/nested")).expect("dirs");
        std::fs::write(root.join("dist/data.txt"), b"x").expect("file");
        std::fs::write(root.join("dist/run.sh"), b"#!/bin/sh\n").expect("file");
        // A read-only file and a locked-down directory — what a checkout of a
        // vendored dependency tree actually looks like.
        std::fs::set_permissions(
            &root.join("dist/data.txt"),
            std::fs::Permissions::from_mode(0o400),
        )
        .expect("chmod");
        std::fs::set_permissions(
            &root.join("dist/run.sh"),
            std::fs::Permissions::from_mode(0o500),
        )
        .expect("chmod");
        std::fs::set_permissions(
            &root.join("dist/nested"),
            std::fs::Permissions::from_mode(0o500),
        )
        .expect("chmod");

        widen_for_the_group(root).expect("widen");

        let mode = |p: &str| {
            std::fs::metadata(root.join(p))
                .expect(p)
                .permissions()
                .mode()
                & 0o7777
        };
        assert_eq!(mode("dist/data.txt") & 0o070, 0o060, "g+rw, and NOT g+x");
        assert_eq!(mode("dist/run.sh") & 0o070, 0o070, "already +x ⇒ g+x too");
        assert_eq!(mode("dist/nested") & 0o070, 0o070, "a dir is always g+x");
        assert_eq!(mode("dist/nested") & 0o2000, 0o2000, "and setgid");
        assert_eq!(mode("dist") & 0o2000, 0o2000);
        // The owner's bits are never taken away.
        assert_eq!(mode("dist/data.txt") & 0o700, 0o400);
    }

    /// A symlink must not be followed: chmod-ing through one would change a file
    /// outside the workspace, and a link's own mode means nothing.
    #[test]
    fn widening_does_not_follow_symlinks() {
        let tmp = tempfile::tempdir().expect("tmp");
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"secret").expect("file");
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o400)).expect("chmod");

        let root = tmp.path().join("ws");
        std::fs::create_dir(&root).expect("ws");
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("symlink");

        widen_for_the_group(&root).expect("widen");
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode() & 0o7777,
            0o400,
            "the symlink's target must be untouched"
        );
    }

    /// The roots parse is merge-ORDER-preserving and tolerant of the shapes the
    /// annotation actually produces (trailing commas, spaces, an empty value).
    #[test]
    fn inputs_parse_preserves_order_and_drops_empties() {
        let parse = |s: &str| -> Vec<String> {
            s.split(',')
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(str::to_string)
                .collect()
        };
        assert_eq!(parse("b,a"), vec!["b".to_string(), "a".to_string()]);
        assert_eq!(parse(""), Vec::<String>::new());
        assert_eq!(parse(" b , ,a,"), vec!["b".to_string(), "a".to_string()]);
    }
}
