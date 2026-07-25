//! LIVE verification of the Forgejo adapter's *shapes* against a REAL Forgejo
//! (git-bug 3863d5e).
//!
//! `contract_live.rs` next door runs the shared port contract; this file exists
//! for the three things that contract cannot answer, because they are guesses
//! about Forgejo's own wire format rather than about the port:
//!
//! 1. **`/user/repos` and its pagination** — the bind pick-list is built from it,
//!    and a paginator that quietly stops after page 1 looks identical to a small
//!    instance. The tier seeds MORE than one page on purpose.
//! 2. **`config.secret` on hook creation** — whether a real Forgejo accepts the
//!    field *and then signs deliveries with it*. This is the guess that already
//!    bit us: hooks were created with no secret, so every delivery from a
//!    Scarab-registered hook would have been rejected 401 by our own endpoint,
//!    while registration reported success.
//! 3. **The push payload spelling** — normalization was written from docs, never
//!    from a delivery.
//!
//! Env-gated (`SCARAB_TEST_FORGEJO=1`) exactly like the `SCARAB_TEST_KUBE` tier,
//! so a plain `cargo test` / CI run never needs a Forgejo. The lifecycle is
//! owned by `just forgejo-verify`; see `deploy/local-forgejo/README.md`.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use scarab_forge::{Event, ForgePort, RepoRef, WebhookDelivery};
use scarab_forge_forgejo::{normalize, verify_signature, ForgejoForge};

/// The env guard every scenario starts with. A macro (not a fn) so the `return`
/// leaves the calling test — the same shape as `scarab-e2e`'s `require_e2e!`.
macro_rules! require_forgejo {
    () => {
        if std::env::var("SCARAB_TEST_FORGEJO").ok().as_deref() != Some("1") {
            eprintln!(
                "SKIPPED (live Forgejo): set SCARAB_TEST_FORGEJO=1 against a seeded instance — \
                 `just forgejo-verify` owns the lifecycle (deploy/local-forgejo/)"
            );
            return;
        }
    };
}

fn env(key: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| panic!("{key} is not set — `just forgejo-verify` writes it"))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client")
}

/// Everything an evidence run produces lands here, so a green run leaves behind
/// the REAL payloads rather than only an assertion that passed. Promote a file
/// from here into a fixture if you want to pin a shape without a Forgejo.
fn capture_dir() -> std::path::PathBuf {
    let dir =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/forgejo-capture");
    std::fs::create_dir_all(&dir).expect("create the capture dir");
    dir
}

fn capture(name: &str, contents: &str) {
    let path = capture_dir().join(name);
    std::fs::write(&path, contents).expect("write the capture file");
    eprintln!("captured: {}", path.display());
}

/// Commit a new file through the contents API. Forgejo turns this into a real
/// commit on `main` and fires a real `push` webhook — the same delivery a
/// `git push` produces, without needing an SSH key or a clone in the test.
async fn commit_file(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    owner: &str,
    repo: &str,
    path: &str,
    contents: &str,
    message: &str,
) {
    use base64::Engine as _;
    let body = serde_json::json!({
        "content": base64::engine::general_purpose::STANDARD.encode(contents),
        "message": message,
        "branch": "main",
    });
    let resp = client
        .post(format!(
            "{base}/api/v1/repos/{owner}/{repo}/contents/{path}"
        ))
        .header("authorization", format!("token {token}"))
        .json(&body)
        .send()
        .await
        .expect("POST contents");
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "committing {path} to {owner}/{repo} failed ({status}): {text}"
    );
}

// --- the delivery capture listener -------------------------------------------

#[derive(Clone, Debug)]
struct Delivery {
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Delivery {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

type Inbox = Arc<Mutex<Vec<Delivery>>>;

/// A listener on the docker host that records what Forgejo actually posts.
/// Bound on 0.0.0.0 because the delivery arrives from inside the container.
async fn capture_listener() -> (u16, Inbox) {
    async fn sink(State(inbox): State<Inbox>, headers: HeaderMap, body: Bytes) -> &'static str {
        inbox.lock().expect("inbox lock").push(Delivery {
            headers: headers
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_string(),
                        v.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect(),
            body: body.to_vec(),
        });
        "accepted"
    }
    let inbox: Inbox = Arc::new(Mutex::new(Vec::new()));
    let app = axum::Router::new()
        .route("/webhooks/forgejo", post(sink))
        .with_state(inbox.clone());
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("bind the capture listener");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (port, inbox)
}

async fn await_delivery(inbox: &Inbox, timeout: Duration) -> Delivery {
    let start = Instant::now();
    loop {
        if let Some(d) = inbox.lock().expect("inbox lock").first().cloned() {
            return d;
        }
        assert!(
            start.elapsed() < timeout,
            "no delivery arrived within {timeout:?}. Forgejo refuses to deliver to \
             private/loopback addresses unless `webhook.ALLOWED_HOST_LIST` allows them \
             (deploy/local-forgejo/compose.yaml sets it), and the callback host must be \
             reachable from inside the container."
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// --- 1. the pick-list -----------------------------------------------------------

/// `list_accessible_repos` really walks every page.
///
/// The tier seeds more repos than the adapter's page size, so an implementation
/// that fetched one page — or that stopped on the first SHORT page — comes back
/// with a truncated pick-list and fails here. Nothing about that failure is
/// visible in production: the admin simply cannot find their repo.
#[tokio::test(flavor = "multi_thread")]
async fn list_accessible_repos_walks_past_the_first_page() {
    require_forgejo!();

    let forge = ForgejoForge::new(
        env("SCARAB_TEST_FORGEJO_URL"),
        env("SCARAB_TEST_FORGEJO_TOKEN"),
    );
    let owner = env("SCARAB_TEST_FORGEJO_OWNER");
    let expected: usize = env_or("SCARAB_TEST_FORGEJO_REPO_TOTAL", "57")
        .parse()
        .expect("SCARAB_TEST_FORGEJO_REPO_TOTAL is a number");
    assert!(
        expected > 50,
        "the seed must exceed the adapter's page size (50) or this test proves nothing; \
         got {expected} — raise FORGEJO_PAD_REPOS"
    );

    let repos = forge
        .list_accessible_repos()
        .await
        .expect("/user/repos is readable with the configured token");
    capture(
        "user-repos.json",
        &serde_json::to_string_pretty(
            &repos
                .iter()
                .map(|r| format!("{}/{}", r.owner, r.name))
                .collect::<Vec<_>>(),
        )
        .unwrap(),
    );

    assert_eq!(
        repos.len(),
        expected,
        "every seeded repo must appear; a page-1-only walk returns 50"
    );
    // A paginator that ignored `page=` would return page 1 over and over: the
    // count could still look right while the SET is wrong.
    let unique: BTreeSet<_> = repos.iter().map(|r| (&r.owner, &r.name)).collect();
    assert_eq!(unique.len(), repos.len(), "duplicate rows: {repos:?}");
    // The owner spelling actually used by the real response — `repos_from_page`
    // accepts `login` or `username`, and an owner-less row is dropped silently,
    // so an empty owner here would mean the pick-list lost its coordinates.
    for r in &repos {
        assert_eq!(r.owner, owner, "every seeded repo belongs to {owner}");
    }
    // And the two working repos are in the set, not just the filler.
    for name in [
        env("SCARAB_TEST_FORGEJO_HOOK_REPO"),
        env("SCARAB_TEST_FORGEJO_ONBOARD_REPO"),
    ] {
        assert!(
            repos.iter().any(|r| r.name == name),
            "{name} missing from the pick-list"
        );
    }
}

// --- 2 + 3. registration, signing, and the payload spelling ---------------------

/// The guess that already bit us, answered by Forgejo itself: a hook this adapter
/// registers must arrive **signed with the secret we configured**, and the push
/// payload it carries must be one `normalize` understands.
///
/// The proof that `config.secret` was accepted is not a readback — Forgejo
/// redacts hook secrets from `GET /hooks` — it is that
/// [`verify_signature`] accepts the real delivery under the same secret. That is
/// also exactly the check `/webhooks/forgejo` performs, so a green assertion here
/// means a real push cannot 401.
#[tokio::test(flavor = "multi_thread")]
async fn a_registered_hook_delivers_a_signed_push_we_can_normalize() {
    require_forgejo!();

    let base = env("SCARAB_TEST_FORGEJO_URL");
    let token = env("SCARAB_TEST_FORGEJO_TOKEN");
    let secret = env("SCARAB_TEST_FORGEJO_WEBHOOK_SECRET");
    let owner = env("SCARAB_TEST_FORGEJO_OWNER");
    let name = env("SCARAB_TEST_FORGEJO_HOOK_REPO");
    let callback_host = env_or("SCARAB_TEST_FORGEJO_CALLBACK_HOST", "host.docker.internal");
    let repo = RepoRef {
        owner: owner.clone(),
        name: name.clone(),
    };
    let client = http();

    let (port, inbox) = capture_listener().await;
    let callback = format!("http://{callback_host}:{port}/webhooks/forgejo");

    let forge = ForgejoForge::new(&base, &token).with_webhook_secret(secret.clone());
    forge
        .register_webhook(&repo, &callback)
        .await
        .expect("register_webhook against a real Forgejo");
    // Idempotency, against the REAL list response. Forgejo 16 reports a hook's
    // callback at top-level `url` and marks `config` deprecated, so an adapter
    // that only read `config.url` would create a second hook here — and every
    // later push would fan out into duplicate runs.
    forge
        .register_webhook(&repo, &callback)
        .await
        .expect("re-registration is a no-op");

    let hooks: serde_json::Value = client
        .get(format!("{base}/api/v1/repos/{owner}/{name}/hooks"))
        .header("authorization", format!("token {token}"))
        .send()
        .await
        .expect("GET hooks")
        .json()
        .await
        .expect("hooks JSON");
    capture("hooks.json", &serde_json::to_string_pretty(&hooks).unwrap());
    let matching = hooks
        .as_array()
        .expect("hooks is an array")
        .iter()
        .filter(|h| {
            h.get("url").and_then(serde_json::Value::as_str) == Some(callback.as_str())
                || h.pointer("/config/url").and_then(serde_json::Value::as_str)
                    == Some(callback.as_str())
        })
        .count();
    assert_eq!(
        matching, 1,
        "exactly one hook for {callback}; two means the idempotency check missed \
         the URL spelling this Forgejo uses: {hooks:#}"
    );

    // --- the real push -------------------------------------------------------
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    let message = "feat: prove the forgejo delivery path";
    commit_file(
        &client,
        &base,
        &token,
        &owner,
        &name,
        &format!("pushes/{stamp}.txt"),
        "scarab forgejo verification\n",
        message,
    )
    .await;

    let delivery = await_delivery(&inbox, Duration::from_secs(90)).await;
    capture(
        "push-headers.txt",
        &delivery
            .headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    capture(
        "push-payload.json",
        &String::from_utf8_lossy(&delivery.body),
    );

    // The event + signature headers the server reads (`forgejo_webhook` accepts
    // the `x-forgejo-*` spelling or the `x-gitea-*` one).
    let event = delivery
        .header("x-forgejo-event")
        .or_else(|| delivery.header("x-gitea-event"))
        .expect("a delivery must name its event")
        .to_string();
    assert_eq!(event, "push", "headers: {:?}", delivery.headers);
    let signature = delivery
        .header("x-forgejo-signature")
        .or_else(|| delivery.header("x-gitea-signature"))
        .unwrap_or_else(|| {
            panic!(
                "the delivery arrived UNSIGNED — `config.secret` was not honoured. \
                 Headers: {:?}",
                delivery.headers
            )
        })
        .to_string();

    // THE assertion: the secret we put in `config.secret` is the one Forgejo
    // signed with. This is byte-for-byte what `/webhooks/forgejo` does.
    verify_signature(secret.as_bytes(), &delivery.body, Some(&signature))
        .expect("the real signature must verify under the registered secret");

    // --- the payload spelling ------------------------------------------------
    let payload: serde_json::Value =
        serde_json::from_slice(&delivery.body).expect("the delivery body is JSON");
    let owner_spelling = ["login", "username"]
        .into_iter()
        .find(|k| payload.pointer(&format!("/repository/owner/{k}")).is_some())
        .unwrap_or_else(|| {
            panic!("repository.owner carries neither `login` nor `username`: {payload:#}")
        });
    eprintln!("real push payload spells repository.owner.{owner_spelling}");
    assert_eq!(
        payload
            .pointer(&format!("/repository/owner/{owner_spelling}"))
            .and_then(serde_json::Value::as_str),
        Some(owner.as_str())
    );

    let normalized = normalize(&WebhookDelivery {
        id: delivery
            .header("x-forgejo-delivery")
            .or_else(|| delivery.header("x-gitea-delivery"))
            .unwrap_or_default()
            .to_string(),
        event,
        signature: Some(signature),
        payload: payload.clone(),
    })
    .expect("a real push delivery must normalize");
    match normalized {
        Event::Push {
            repo: got,
            r#ref,
            after,
            message: got_message,
            ..
        } => {
            assert_eq!(got, repo);
            assert_eq!(r#ref, "refs/heads/main");
            assert_eq!(
                after.len(),
                40,
                "`after` must be the new tip SHA, got {after:?}"
            );
            assert!(
                got_message.starts_with(message),
                "head_commit.message feeds the run Headline (ADR-0057); got {got_message:?}"
            );
        }
        other => panic!("a push delivery normalized to {other:?}"),
    }
}
