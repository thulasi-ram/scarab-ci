//! `list_collaborators` (ADR-0069 §5) against a stub Forgejo served over real
//! HTTP — the forge is the one true external here.
//!
//! What this pins is the ASSEMBLY, because Forgejo has no single "everyone with
//! access" endpoint: collaborators ∪ (org repo → every team's members | user
//! repo → its owner), each with the EFFECTIVE level from the per-user
//! permission endpoint. And the all-or-nothing rule: one failed page fails the
//! call, because a re-sync deletes the roles of everyone absent from the list.
//! The wire shapes themselves are a guess until `just forgejo-verify` runs the
//! live tier (ADR-0069 s7).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use scarab_forge::{ForgePort, RepoRef};
use scarab_forge_forgejo::ForgejoForge;
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct Stub {
    /// Paths that answer 500 — a forge failing mid-listing.
    broken: Arc<Mutex<Vec<String>>>,
}

fn page(items: Vec<Value>, q: &HashMap<String, String>) -> Json<Value> {
    // 1-based pages of 2 — small enough that a one-page walk would be caught.
    let p: usize = q.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
    Json(Value::Array(
        items.into_iter().skip((p - 1) * 2).take(2).collect(),
    ))
}

fn user(id: u64, login: &str) -> Value {
    json!({ "id": id, "login": login })
}

async fn serve(stub: Stub) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/api/v1/repos/{owner}/{repo}/collaborators",
            get(
                |State(s): State<Stub>,
                 Path((owner, _repo)): Path<(String, String)>,
                 Query(q): Query<HashMap<String, String>>| async move {
                    if s.broken
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|b| b == "collaborators")
                        && q.get("page").map(String::as_str) == Some("2")
                    {
                        return (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
                    }
                    let items = if owner == "acme" {
                        vec![user(1, "ana"), user(2, "ben"), user(3, "cy")]
                    } else {
                        vec![user(4, "dee")]
                    };
                    page(items, &q).into_response()
                },
            ),
        )
        .route(
            "/api/v1/orgs/{name}",
            get(|Path(name): Path<String>| async move {
                if name == "acme" {
                    Json(json!({ "id": 900, "username": "acme" })).into_response()
                } else {
                    (StatusCode::NOT_FOUND, "{}").into_response()
                }
            }),
        )
        .route(
            "/api/v1/users/{name}",
            get(|Path(name): Path<String>| async move { Json(user(50, &name)) }),
        )
        .route(
            "/api/v1/repos/{owner}/{repo}/teams",
            get(|| async { Json(json!([{ "id": 7, "name": "devs" }])) }),
        )
        .route(
            "/api/v1/teams/{id}/members",
            get(|Query(q): Query<HashMap<String, String>>| async move {
                // `ben` is in a team AND a collaborator: one row, not two.
                page(
                    vec![
                        user(2, "ben"),
                        user(5, "eve"),
                        json!({ "id": 6, "username": "fin" }),
                    ],
                    &q,
                )
            }),
        )
        .route(
            "/api/v1/repos/{owner}/{repo}/collaborators/{user}/permission",
            get(
                |Path((_o, _r, u)): Path<(String, String, String)>| async move {
                    let level = match u.as_str() {
                        "ana" | "dee" => "admin",
                        "ben" | "eve" => "write",
                        "bob" => "owner",
                        _ => "read",
                    };
                    Json(json!({ "permission": level }))
                },
            ),
        )
        .with_state(stub);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn repo(owner: &str) -> RepoRef {
    RepoRef {
        owner: owner.into(),
        name: "web".into(),
    }
}

#[tokio::test]
async fn an_org_repo_lists_collaborators_and_team_members_once_each() {
    let base = serve(Stub::default()).await;
    let forge = ForgejoForge::new(base, "tok");
    let got = forge.list_collaborators(&repo("acme")).await.unwrap();
    let summary: Vec<(&str, &str, bool, bool)> = got
        .iter()
        .map(|c| {
            (
                c.external_id.as_str(),
                c.login.as_str(),
                c.permissions.write,
                c.permissions.admin,
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("1", "ana", true, true),
            ("2", "ben", true, false),
            ("3", "cy", false, false),
            ("5", "eve", true, false),
            ("6", "fin", false, false),
        ],
        "every page of both lists, deduplicated by numeric id, `username` accepted"
    );
}

#[tokio::test]
async fn a_user_repo_includes_its_owner() {
    let base = serve(Stub::default()).await;
    let forge = ForgejoForge::new(base, "tok");
    let got = forge.list_collaborators(&repo("bob")).await.unwrap();
    let ids: Vec<(&str, bool)> = got
        .iter()
        .map(|c| (c.login.as_str(), c.permissions.admin))
        .collect();
    assert_eq!(ids, vec![("dee", true), ("bob", true)]);
}

#[tokio::test]
async fn one_failed_page_fails_the_whole_listing() {
    let stub = Stub::default();
    stub.broken.lock().unwrap().push("collaborators".into());
    let base = serve(stub).await;
    let forge = ForgejoForge::new(base, "tok");
    assert!(
        forge.list_collaborators(&repo("acme")).await.is_err(),
        "a partial list would let a re-sync revoke whoever was on page 2"
    );
}
