//! The shared ForgePort contract suite (ADR-0046), run against the in-memory
//! fake — proving the suite is runnable against any implementation. The
//! GitHub and Forgejo adapters run this same suite (live-gated) so no vendor
//! assumption can hide in the port.

use scarab_forge::contract::{assert_contract, ContractFixture};
use scarab_forge::{Event, RepoRef, WebhookDelivery};
use scarab_testkit::FakeForge;

#[tokio::test]
async fn fake_forge_passes_the_port_contract() {
    let repo = RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    };
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", "on: {push: {}}\nsteps: []")
        .with_commit("refs/heads/main", "cafe1234")
        .with_branch("main", "cafe1234")
        .with_branch("feat/widget", "beef5678")
        .with_tag("v1.0.0", "cafe1234");

    // The fake's wire format is the canonical Event itself.
    let push_delivery = WebhookDelivery {
        id: "d1".into(),
        event: "push".into(),
        signature: None,
        payload: serde_json::to_value(Event::Push {
            actor: "octocat".into(),
            repo: repo.clone(),
            r#ref: "refs/heads/main".into(),
            after: "cafe1234".into(),
            message: "initial commit".into(),
        })
        .unwrap(),
    };

    let fx = ContractFixture {
        repo,
        r#ref: "refs/heads/main".into(),
        commit_sha: "cafe1234".into(),
        dir: ".scarab".into(),
        known_file: (
            ".scarab/ci.yaml".into(),
            b"on: {push: {}}\nsteps: []".to_vec(),
        ),
        push_delivery,
        known_branch: Some("main".into()),
    };
    assert_contract(&forge, &fx).await;
}
