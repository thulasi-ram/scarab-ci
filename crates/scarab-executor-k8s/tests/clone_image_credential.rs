//! The clone image's credential scope (`docker/clone/scarab-askpass`).
//!
//! The clone Step hands git a forge token through `GIT_ASKPASS`. On Forgejo
//! and GitHub PAT connections that token is long-lived and reads every
//! repository its user can. git asks the helper for a credential whenever ANY
//! fetch in the clone gets a 401: the repository itself, a submodule named in
//! the checked-out `.gitmodules`, or an LFS endpoint named in `.lfsconfig`.
//! The last two are content of the commit being built, so a fork pull
//! request controls them. The helper must answer only for the repository
//! being cloned.
//!
//! These tests run the real `git` against the real helper script. No
//! container, no cluster: the helper is a shell script and the questions git
//! asks it are the contract.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TOKEN: &str = "LONG-LIVED-PAT";
const USER: &str = "scarab-bot";
/// `scarab-bot:LONG-LIVED-PAT` as an HTTP Basic credential.
const BASIC: &str = "Basic c2NhcmFiLWJvdDpMT05HLUxJVkVELVBBVA==";

/// A temp dir holding the token file and a wrapper that runs the image's
/// askpass script from the source tree (so the test does not depend on the
/// checkout preserving the exec bit).
struct Helper {
    dir: tempfile::TempDir,
}

impl Helper {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let src = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docker/clone/scarab-askpass"
        );
        let wrapper = dir.path().join("askpass");
        std::fs::write(&wrapper, format!("#!/bin/sh\nexec sh '{src}' \"$@\"\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.path().join("token"), TOKEN).unwrap();
        Helper { dir }
    }

    fn askpass(&self) -> PathBuf {
        self.dir.path().join("askpass")
    }

    /// A `git` command with a clean environment: the helper wired in as the
    /// clone image wires it, and `clone_url` as the Step's clone URL.
    fn git(&self, clone_url: &str) -> Command {
        let mut cmd = Command::new("git");
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", self.askpass())
            .env("SCARAB_CLONE_URL", clone_url)
            .env("SCARAB_CLONE_TOKEN_FILE", self.dir.path().join("token"))
            .env("SCARAB_CLONE_USERNAME", USER);
        cmd
    }

    /// Ask git for the credential of `url` the way a fetch does when the
    /// forge answers 401, with `credential.useHttpPath` on as `scarab-clone.sh`
    /// sets it. Returns the password git obtained, if any.
    fn fill(&self, clone_url: &str, url: &str) -> Option<String> {
        let mut child = self
            .git(clone_url)
            .args(["-c", "credential.useHttpPath=true", "credential", "fill"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("git runs");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("url={url}\n\n").as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("password=").map(str::to_string))
    }

    /// Run the helper with a prompt as git-lfs words it: `Username for
    /// "<url>"` (Go `%q`, double quotes, no trailing colon).
    fn lfs_prompt(&self, clone_url: &str, what: &str, url: &str) -> Option<String> {
        let out = Command::new(self.askpass())
            .arg(format!("{what} for \"{url}\""))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("SCARAB_CLONE_URL", clone_url)
            .env("SCARAB_CLONE_TOKEN_FILE", self.dir.path().join("token"))
            .env("SCARAB_CLONE_USERNAME", USER)
            .stderr(Stdio::null())
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).to_string())
    }
}

/// An HTTP listener on 127.0.0.1 that answers every request with a Basic
/// challenge and records each request's path and `Authorization` header.
fn challenge_server() -> (u16, Arc<Mutex<Vec<(String, Option<String>)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<(String, Option<String>)>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match conn.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let head = String::from_utf8_lossy(&buf).to_string();
            let path = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or_default()
                .to_string();
            let auth = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .map(|l| l.split_once(':').unwrap().1.trim().to_string());
            let answered = auth.is_some();
            log.lock().unwrap().push((path, auth));
            let resp = if answered {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"forge\"\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            };
            let _ = conn.write_all(resp.as_bytes());
        }
    });
    (port, seen)
}

fn run_bounded(mut child: std::process::Child) {
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The red team's case (`redteam_forge_token_r2.rs` on the libraries branch):
/// a submodule URL naming another repository on the forge's host. Over the
/// wire, against a listener that challenges every request.
#[test]
fn the_clone_token_does_not_answer_for_another_repository_on_the_forge() {
    let helper = Helper::new();
    let (port, seen) = challenge_server();
    let base = format!("http://127.0.0.1:{port}");
    let child = helper
        .git(&format!("{base}/acme/web.git"))
        .args(["ls-remote", &format!("{base}/other-org/private.git")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("git runs");
    run_bounded(child);
    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "git never reached the listener");
    let leaked: Vec<_> = seen
        .iter()
        .filter(|(path, auth)| path.starts_with("/other-org/") && auth.as_deref() == Some(BASIC))
        .collect();
    assert!(
        leaked.is_empty(),
        "the clone token was sent for another repository on the forge: {leaked:?}"
    );
}

/// The same question over https, where the token is allowed at all: git's
/// own credential prompt for another repository on the clone URL's host gets
/// nothing. The positive cases are the control: they show the test can see a
/// credential when one is given.
#[test]
fn over_https_the_token_answers_only_for_the_cloned_repository() {
    let helper = Helper::new();
    let clone = "https://forge.example/acme/web.git";

    for other in [
        "https://forge.example/other-org/private.git",
        "https://forge.example/acme/web-secrets.git",
        "https://forge.example/acme/web.git/../../other-org/private.git",
        "https://forge.example/acme",
        "https://forge.example/",
        "https://other.example/acme/web.git",
        "https://forge.example.evil.test/acme/web.git",
    ] {
        assert_eq!(
            helper.fill(clone, other),
            None,
            "the clone token was handed out for {other}"
        );
    }

    for own in [
        "https://forge.example/acme/web.git",
        "https://forge.example/acme/web",
        "https://forge.example/acme/web.git/",
        "https://forge.example/acme/web.git/info/lfs",
    ] {
        assert_eq!(
            helper.fill(clone, own).as_deref(),
            Some(TOKEN),
            "the clone token must still answer for the repository itself: {own}"
        );
    }
    // A clone URL written without `.git` names the same repository.
    assert_eq!(
        helper
            .fill(
                "https://forge.example/acme/web",
                "https://forge.example/acme/web.git"
            )
            .as_deref(),
        Some(TOKEN)
    );
}

/// git-lfs words its prompt differently from git (double quotes, no colon).
/// A helper that cannot parse a prompt must refuse it, not fall through to
/// answering.
#[test]
fn an_lfs_prompt_for_another_repository_gets_nothing() {
    let helper = Helper::new();
    let clone = "https://forge.example/acme/web.git";
    for what in ["Username", "Password"] {
        assert_eq!(
            helper.lfs_prompt(
                clone,
                what,
                "https://forge.example/other-org/private.git/info/lfs"
            ),
            None,
            "{what} handed out for another repository's LFS endpoint"
        );
        assert_eq!(
            helper.lfs_prompt(clone, what, "https://attacker.example/lfs"),
            None,
            "{what} handed out for a foreign LFS host"
        );
    }
    assert_eq!(
        helper
            .lfs_prompt(
                clone,
                "Password",
                "https://scarab-bot@forge.example/acme/web.git/info/lfs"
            )
            .as_deref(),
        Some(TOKEN)
    );
    // A prompt the helper does not recognise is refused.
    assert_eq!(helper.lfs_prompt(clone, "Password", "").as_deref(), None);
}

/// The token never goes over plain http, even to the cloned repository.
#[test]
fn the_token_is_not_sent_when_the_clone_url_is_not_https() {
    let helper = Helper::new();
    for clone in [
        "http://forge.example/acme/web.git",
        "ftp://forge.example/acme/web.git",
    ] {
        let scheme = clone.split(':').next().unwrap();
        assert_eq!(
            helper.fill(clone, &format!("{scheme}://forge.example/acme/web.git")),
            None,
            "the clone token was handed out for {clone}"
        );
    }
    // A https prompt does not unlock a http clone URL either.
    assert_eq!(
        helper.fill(
            "http://forge.example/acme/web.git",
            "https://forge.example/acme/web.git"
        ),
        None
    );
}

/// `scarab-clone.sh` runs the submodule fetch with no askpass helper at all,
/// so a submodule is fetched anonymously whatever its URL, and pins the LFS
/// endpoint over `.lfsconfig`. Checked on the script's commands, not run:
/// running the script needs a forge served over https and git-lfs.
#[test]
fn the_clone_script_scopes_the_credential() {
    let text = std::fs::read_to_string(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docker/clone/scarab-clone.sh"
    )))
    .unwrap();
    let script: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();
    assert!(
        script
            .iter()
            .any(|l| l.contains("credential.useHttpPath true")),
        "the askpass helper can only check the path if git puts it in the prompt"
    );
    let submodule_line = script
        .iter()
        .find(|l| l.contains("submodule update"))
        .expect("the script fetches submodules");
    assert!(
        submodule_line.contains("GIT_ASKPASS= "),
        "submodules must be fetched without the credential: {submodule_line}"
    );
    let lfs_line = script
        .iter()
        .find(|l| l.contains("lfs pull"))
        .expect("the script pulls LFS objects");
    assert!(
        lfs_line.contains("lfs.url="),
        "the LFS endpoint must be pinned to the cloned repository's: {lfs_line}"
    );
}
