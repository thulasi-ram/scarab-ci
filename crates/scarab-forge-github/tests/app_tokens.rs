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
    assert_eq!(body["permissions"], serde_json::json!({ "contents": "write" }));
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
    assert_eq!(body["permissions"], serde_json::json!({ "contents": "read" }));
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
