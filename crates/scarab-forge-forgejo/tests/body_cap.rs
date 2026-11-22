//! Every forge answer is read under one cap (git-bug bfa8dfd):
//! [`FORGE_BODY_MAX_BYTES`](scarab_forge::FORGE_BODY_MAX_BYTES). A body of
//! exactly the cap is read; one byte more is an error that names the cap and
//! carries none of the body.
//!
//! Each body is a whole, valid answer followed by spaces, sent with no
//! `Content-Length` (the connection closes at its end), so a body cut at the
//! cap would still parse: only "never parse a cut body" fails the long one.
//! Served over a real socket from 127.0.0.1.

use scarab_forge::{
    Commit, ForgeError, ForgePort, RepoRef, Status, StatusState, FORGE_BODY_MAX_BYTES,
};
use scarab_forge_forgejo::ForgejoForge;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
/// Text inside every answer; an error must never quote it.
const MARKER: &str = "BODY-MARKER";

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    }
}

/// `json` followed by spaces up to `len` bytes.
fn padded(json: &str, len: usize) -> Vec<u8> {
    let mut body = json.as_bytes().to_vec();
    assert!(body.len() <= len);
    body.resize(len, b' ');
    body
}

/// A stub Forgejo that answers every request with `status` and `body`, with
/// no `Content-Length`. The request is read whole first, its body included,
/// so a POST is answered cleanly.
async fn serve_unsized(status: &'static str, body: Vec<u8>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if conn.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let sent = String::from_utf8_lossy(&head)
                    .lines()
                    .find_map(|l| {
                        let (name, value) = l.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                let mut request_body = vec![0u8; sent];
                if conn.read_exact(&mut request_body).await.is_err() {
                    return;
                }
                let status_line = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                     connection: close\r\n\r\n"
                );
                let _ = conn.write_all(status_line.as_bytes()).await;
                let _ = conn.write_all(&body).await;
                let _ = conn.shutdown().await;
            });
        }
    });
    base
}

fn assert_too_large(err: ForgeError, case: &str) {
    let ForgeError::Api(msg) = err else {
        panic!("{case}: not an Api error: {err:?}");
    };
    assert!(
        msg.contains(&format!("over {FORGE_BODY_MAX_BYTES} bytes")),
        "{case}: {msg}"
    );
    assert!(!msg.contains(MARKER), "{case}: the error quotes the body");
}

/// `set_status` reads its answer through `ok_json`.
#[tokio::test]
async fn an_ok_json_answer_is_read_up_to_the_cap_and_not_one_byte_more() {
    let answer = format!(r#"{{"id":1,"description":"{MARKER}"}}"#);
    let commit = Commit {
        sha: SHA.into(),
        message: String::new(),
    };
    let status = || Status {
        context: "scarab".into(),
        state: StatusState::Success,
        target_url: "http://scarab.test/runs/1".into(),
        description: None,
    };
    for (case, code, len, ok) in [
        ("2xx at the cap", "201 Created", FORGE_BODY_MAX_BYTES, true),
        (
            "2xx over the cap",
            "201 Created",
            FORGE_BODY_MAX_BYTES + 1,
            false,
        ),
        (
            "5xx over the cap",
            "500 Internal Server Error",
            FORGE_BODY_MAX_BYTES + 1,
            false,
        ),
    ] {
        let base = serve_unsized(code, padded(&answer, len)).await;
        let forge = ForgejoForge::new(&base, "token");
        let got = forge.set_status(&repo(), &commit, status()).await;
        match (ok, got) {
            (true, Ok(())) => {}
            (false, Err(e)) => assert_too_large(e, case),
            (_, got) => panic!("{case}: {got:?}"),
        }
    }
}

/// `latest_commit` reads its answer through the conditional GET.
#[tokio::test]
async fn a_conditional_get_is_read_up_to_the_cap_and_not_one_byte_more() {
    // Forgejo's commits list: `limit=1` gives the tip.
    let answer = format!(r#"[{{"sha":"{SHA}","commit":{{"message":"{MARKER}"}}}}]"#);

    let base = serve_unsized("200 OK", padded(&answer, FORGE_BODY_MAX_BYTES)).await;
    let forge = ForgejoForge::new(&base, "token");
    let commit = forge.latest_commit(&repo(), "main").await.unwrap();
    assert_eq!(commit.sha, SHA);

    let base = serve_unsized("200 OK", padded(&answer, FORGE_BODY_MAX_BYTES + 1)).await;
    let forge = ForgejoForge::new(&base, "token");
    let err = forge.latest_commit(&repo(), "main").await.unwrap_err();
    assert_too_large(err, "over the cap");
}
