//! The clone image's pinned commit (`docker/clone/scarab-clone.sh`).
//!
//! The script passes `SCARAB_CLONE_SHA` to `git fetch` as an argument. git
//! reads an argument that starts with `-` as an option, and
//! `--upload-pack=<program>` makes it run that program. The SHA comes from
//! the forge, and the server checks its shape before the launch; the script
//! checks it again, before any git command, so the image is safe on its own
//! (red team 2026-10-30).
//!
//! These tests run the real script with the real `git` against a local
//! repository over `file://`. No container, no cluster.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docker/clone/scarab-clone.sh"
);
const SCRUB: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docker/clone/scarab-config-scrub"
);

/// A source repository with one commit, and a place to clone it.
struct Fixture {
    dir: tempfile::TempDir,
    sha: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&src)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", dir.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        std::fs::write(src.join("README"), "hello\n").unwrap();
        git(&["add", "README"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "first",
        ]);
        let sha = git(&["rev-parse", "HEAD"]);

        // The image puts its helpers on PATH; run them from the source tree.
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let scrub = bin.join("scarab-config-scrub");
        std::fs::write(&scrub, format!("#!/bin/sh\nexec sh '{SCRUB}' \"$@\"\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&scrub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The token and username files `forge_token` mounts (ADR-0076 §9).
        std::fs::write(dir.path().join("forge-token"), "TOKEN").unwrap();
        std::fs::write(dir.path().join("forge-username"), "scarab-bot").unwrap();
        Fixture { dir, sha }
    }

    fn workspace(&self) -> PathBuf {
        self.dir.path().join("ws")
    }

    fn marker(&self) -> PathBuf {
        self.dir.path().join("pwned")
    }

    /// Run the clone script for `sha` into a fresh workspace.
    fn clone(&self, sha: &str, depth: &str) -> Output {
        let ws = self.workspace();
        let _ = std::fs::remove_dir_all(&ws);
        let home = self.dir.path().join("home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("sh")
            .arg(SCRIPT)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "SCARAB_CLONE_URL",
                format!("file://{}", self.dir.path().join("src").display()),
            )
            .env("SCARAB_CLONE_SHA", sha)
            .env("SCARAB_CLONE_DEPTH", depth)
            .env("SCARAB_CLONE_TOKEN_FILE", self.dir.path().join("forge-token"))
            .env(
                "SCARAB_CLONE_USERNAME_FILE",
                self.dir.path().join("forge-username"),
            )
            .env("SCARAB_WORKSPACE", &ws)
            .output()
            .unwrap()
    }
}

fn head_of(ws: &Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(ws)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CEILING_DIRECTORIES", ws.parent().unwrap())
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A SHA that is not hex never reaches git: the script exits non-zero before
/// it creates the workspace, and the program an `--upload-pack=` names does
/// not run. The control clones a real commit, so a script that fails for
/// another reason (a shell that cannot parse it) cannot pass this test.
#[test]
fn a_clone_sha_that_is_not_hex_never_reaches_git() {
    let fx = Fixture::new();

    for depth in ["1", "3", "full"] {
        let out = fx.clone(&fx.sha, depth);
        assert!(
            out.status.success(),
            "the control clone (depth {depth}) failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(head_of(&fx.workspace()), fx.sha, "depth {depth}");
    }

    let marker = fx.marker();
    let upload_pack = format!("--upload-pack=touch '{}';git-upload-pack", marker.display());
    for depth in ["1", "3", "full"] {
        for sha in [
            upload_pack.as_str(),
            "-c core.sshCommand=id",
            "main; id",
            "HEAD",
            "CAFE1234",
            "",
        ] {
            let out = fx.clone(sha, depth);
            assert!(
                !out.status.success(),
                "the clone SHA {sha:?} (depth {depth}) was accepted"
            );
            assert!(
                !marker.exists(),
                "git ran the program in the clone SHA {sha:?} (depth {depth})"
            );
            assert!(
                !fx.workspace().join(".git").exists(),
                "git ran before the clone SHA {sha:?} was checked (depth {depth})"
            );
        }
    }
}
