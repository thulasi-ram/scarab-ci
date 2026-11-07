//! GitHub App tokens for `forge_token` (ADR-0076 §9), over real HTTP against a
//! stub GitHub: a write mint asks for `contents: write` on one repository, a
//! revocation ends the token with the token itself, and a fixed-token
//! connection neither mints a write token nor revokes its token.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use scarab_forge::{ForgePort, RepoRef};
use scarab_forge_github::{GithubApp, GithubForge};

// ---------------------------------------------------------------- stub GitHub

/// One scripted reply, popped in order.
struct Reply {
    status: StatusCode,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Reply {
    fn new(status: StatusCode, body: &str) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    fn ok(body: &str) -> Self {
        Self::new(StatusCode::OK, body)
    }
}

/// What the stub saw — the wire observation the assertions are made against.
#[derive(Clone)]
struct Seen {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

#[derive(Default)]
struct Recorder {
    seen: Vec<Seen>,
    replies: VecDeque<Reply>,
}

struct Stub {
    base_url: String,
    recorder: Arc<Mutex<Recorder>>,
}

impl Stub {
    async fn start(replies: Vec<Reply>) -> Self {
        let recorder = Arc::new(Mutex::new(Recorder {
            seen: Vec::new(),
            replies: replies.into(),
        }));
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(recorder.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base_url: format!("http://{addr}"),
            recorder,
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.recorder.lock().unwrap().seen.clone()
    }

    /// A forge in App mode pointed at this stub.
    fn app_forge(&self) -> GithubForge {
        GithubForge::app(app_key().app.clone()).with_base_url(&self.base_url)
    }
}

async fn handle(
    State(recorder): State<Arc<Mutex<Recorder>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let str_header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let reply = {
        let mut r = recorder.lock().unwrap();
        r.seen.push(Seen {
            method: method.to_string(),
            path: uri.path().to_string(),
            authorization: str_header("authorization"),
            body: String::from_utf8_lossy(&body).into_owned(),
        });
        r.replies.pop_front()
    };
    let Some(reply) = reply else {
        // Running out of scripted replies is itself the finding: the adapter
        // issued a request the test did not expect (a retry that should not
        // have happened). The recorded `seen` count is what fails the test.
        return (StatusCode::IM_A_TEAPOT, "unscripted request").into_response();
    };
    let mut resp = (reply.status, reply.body).into_response();
    for (name, value) in reply.headers {
        resp.headers_mut().insert(name, value.parse().unwrap());
    }
    resp
}

// ------------------------------------------------------------ App credentials

struct AppKey {
    app: GithubApp,
}

/// One throwaway RSA keypair for the whole binary — 2048-bit keygen is slow.
fn app_key() -> &'static AppKey {
    static KEY: OnceLock<AppKey> = OnceLock::new();
    KEY.get_or_init(|| {
        use rsa::pkcs8::EncodePrivateKey;
        let mut rng = rsa::rand_core::OsRng;
        let private = rsa::RsaPrivateKey::new(&mut rng, 2048).expect("generate key");
        AppKey {
            app: GithubApp {
                app_id: "4323648".into(),
                private_key_pem: private
                    .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
                    .expect("encode private")
                    .to_string(),
            },
        }
    })
}

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

#[tokio::test]
async fn a_write_mint_asks_for_contents_write_on_one_repository() {
    let stub = Stub::start(vec![
        Reply::ok(r#"{"id": 77}"#),
        Reply::new(StatusCode::CREATED, r#"{"token": "ghs_write"}"#),
    ])
    .await;
    let forge = stub.app_forge();
    assert!(forge.mints_repo_tokens(&repo()).await.unwrap());
    let cred = forge.mint_write_credential(&repo()).await.unwrap();
    assert_eq!(cred.token, "ghs_write");
    assert_eq!(cred.username, "x-access-token");
    let seen = stub.seen();
    assert_eq!(seen[1].path, "/app/installations/77/access_tokens");
    let body: serde_json::Value = serde_json::from_str(&seen[1].body).unwrap();
    assert_eq!(
        body["permissions"],
        serde_json::json!({ "contents": "write" })
    );
    assert_eq!(body["repositories"], serde_json::json!(["web"]));
}

#[tokio::test]
async fn a_read_mint_asks_for_contents_read() {
    let stub = Stub::start(vec![
        Reply::ok(r#"{"id": 77}"#),
        Reply::new(StatusCode::CREATED, r#"{"token": "ghs_read"}"#),
    ])
    .await;
    let cred = stub
        .app_forge()
        .mint_checkout_credential(&repo(), true)
        .await
        .unwrap();
    assert_eq!(cred.token, "ghs_read");
    let body: serde_json::Value = serde_json::from_str(&stub.seen()[1].body).unwrap();
    assert_eq!(
        body["permissions"],
        serde_json::json!({ "contents": "read" })
    );
}

#[tokio::test]
async fn revoking_deletes_the_installation_token_with_the_token_itself() {
    let stub = Stub::start(vec![Reply::new(StatusCode::NO_CONTENT, "")]).await;
    stub.app_forge()
        .revoke_credential(&repo(), "ghs_minted")
        .await
        .unwrap();
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "DELETE");
    assert_eq!(seen[0].path, "/installation/token");
    assert_eq!(seen[0].authorization.as_deref(), Some("Bearer ghs_minted"));
}

#[tokio::test]
async fn a_fixed_token_neither_mints_a_write_token_nor_is_revoked() {
    let stub = Stub::start(vec![]).await;
    let forge = GithubForge::new("ghp_pat".to_string()).with_base_url(&stub.base_url);
    assert!(!forge.mints_repo_tokens(&repo()).await.unwrap());
    assert!(forge.mint_write_credential(&repo()).await.is_err());
    forge.revoke_credential(&repo(), "ghp_pat").await.unwrap();
    assert!(stub.seen().is_empty(), "nothing reaches GitHub");
}

/// git-bug 19b0a45: a connection that knows its installation mints from it
/// alone. The per-repo lookup (`GET /repos/{owner}/{name}/installation`),
/// which answers with whichever installation covers the repo, is never made.
#[tokio::test]
async fn a_pinned_installation_mints_from_itself_without_a_lookup() {
    let stub = Stub::start(vec![Reply::new(
        StatusCode::CREATED,
        r#"{"token": "ghs_pinned"}"#,
    )])
    .await;
    let forge = stub.app_forge().with_installation(55);
    let cred = forge.mint_checkout_credential(&repo(), true).await.unwrap();
    assert_eq!(cred.token, "ghs_pinned");
    let seen = stub.seen();
    assert_eq!(
        seen.len(),
        1,
        "no installation lookup: {:?}",
        seen.iter().map(|s| &s.path).collect::<Vec<_>>()
    );
    assert_eq!(seen[0].path, "/app/installations/55/access_tokens");
}

/// git-bug 19b0a45: enumeration through a pinned installation lists that
/// installation's repositories only; `/app/installations` (every account
/// the App is on) is never walked.
#[tokio::test]
async fn a_pinned_installation_lists_only_its_own_repositories() {
    let stub = Stub::start(vec![
        Reply::new(StatusCode::CREATED, r#"{"token": "ghs_inst"}"#),
        Reply::ok(r#"{"total_count": 1, "repositories": [{"full_name": "acme/web"}]}"#),
    ])
    .await;
    let repos = stub
        .app_forge()
        .with_installation(55)
        .list_accessible_repos()
        .await
        .unwrap();
    assert_eq!(repos, vec![repo()]);
    let paths: Vec<String> = stub.seen().into_iter().map(|s| s.path).collect();
    assert_eq!(
        paths,
        vec![
            "/app/installations/55/access_tokens".to_string(),
            "/installation/repositories".to_string()
        ]
    );
}

/// git-bug 19b0a45: a pinned token request names the repository by name
/// only (`repositories: ["x"]`), so an adapter serving `acme` must refuse
/// `globex/x` itself, or installation 55 would mint for `acme/x`. Unpinned
/// (a config-declared connection), the same owner check stands in front of
/// the per-repo installation lookup.
#[tokio::test]
async fn an_adapter_serving_an_owner_mints_nothing_for_another_owners_repo() {
    let other = RepoRef {
        owner: "globex".into(),
        name: "web".into(),
    };
    let stub = Stub::start(vec![]).await;
    let pinned = stub.app_forge().with_installation(55).with_owner("acme");
    assert!(pinned.mint_checkout_credential(&other, true).await.is_err());
    assert!(pinned.mint_write_credential(&other).await.is_err());
    let unpinned = stub.app_forge().with_owner("acme");
    assert!(unpinned
        .read_file_at_ref(&other, "main", "README.md")
        .await
        .is_err());
    assert!(
        stub.seen().is_empty(),
        "GitHub was asked: {:?}",
        stub.seen().iter().map(|s| &s.path).collect::<Vec<_>>()
    );
}

/// git-bug 19b0a45: an unpinned App adapter (a config-declared connection
/// without an installation id) walks every installation of the App. When it
/// serves an owner it lists that owner's repositories only: the names of
/// another account's private repositories are not the owner's to see.
#[tokio::test]
async fn an_unpinned_adapter_serving_an_owner_lists_only_that_owners_repositories() {
    let stub = Stub::start(vec![
        Reply::ok(r#"[{"id": 1}, {"id": 2}]"#),
        Reply::new(StatusCode::CREATED, r#"{"token": "ghs_one"}"#),
        Reply::ok(r#"{"total_count": 1, "repositories": [{"full_name": "acme/web"}]}"#),
        Reply::new(StatusCode::CREATED, r#"{"token": "ghs_two"}"#),
        Reply::ok(r#"{"total_count": 1, "repositories": [{"full_name": "globex/secret"}]}"#),
    ])
    .await;
    let repos = stub
        .app_forge()
        .with_owner("acme")
        .list_accessible_repos()
        .await
        .unwrap();
    assert_eq!(repos, vec![repo()]);
    assert_eq!(stub.seen()[0].path, "/app/installations", "the walk ran");
}
