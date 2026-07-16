//! Fork-PR lockout acceptance (ADR-0015, 0005): an untrusted fork PR reads no
//! secrets and its OIDC subject is downgraded to `env/none`, while a trusted
//! (same-repo) run keeps its secrets and target environment. Hermetic.

use std::sync::Arc;

use scarab_forge::{Event, RepoRef};
use scarab_identity::Claims;
use scarab_server::{fork_policy, resolve_step_secrets, LogService};
use scarab_secrets::SecretScope;
use scarab_testkit::{FakeSecrets, InMemoryDb, InMemoryObjectStore};

fn scope() -> SecretScope {
    SecretScope::Repo {
        org: "acme".into(),
        repo: "app".into(),
    }
}

fn pr(fork: bool) -> Event {
    Event::PullRequest {
        repo: RepoRef {
            owner: "acme".into(),
            name: "app".into(),
        },
        number: 1,
        head: "abc".into(),
        fork,
    }
}

#[tokio::test]
async fn fork_pr_is_locked_out_of_secrets_and_gets_env_none() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let logs = LogService::new(Arc::new(InMemoryObjectStore::new()), db);
    let secrets = FakeSecrets::new().with_secret(&scope(), "TOKEN", b"top-secret");
    let keys = vec!["TOKEN".to_string()];

    // Fork PR → locked out + downgraded subject env.
    let policy = fork_policy(&pr(true), "prod");
    assert!(policy.secrets_locked_out);
    assert_eq!(policy.oidc_env, "none");

    let env = resolve_step_secrets(&secrets, &logs, &scope(), &keys, policy.secrets_locked_out)
        .await
        .unwrap();
    assert!(env.is_empty(), "a fork PR must read no secrets");

    let subject = Claims::run_subject("acme", "app", &policy.oidc_env, "refs/pull/1/head");
    assert_eq!(subject, "scarab:org/acme/repo/app/env/none/ref/refs/pull/1/head");

    // A trusted (same-repo) PR keeps its secrets and target environment.
    let trusted = fork_policy(&pr(false), "prod");
    assert!(!trusted.secrets_locked_out);
    assert_eq!(trusted.oidc_env, "prod");
    let env = resolve_step_secrets(&secrets, &logs, &scope(), &keys, trusted.secrets_locked_out)
        .await
        .unwrap();
    assert_eq!(env, vec![("TOKEN".to_string(), "top-secret".to_string())]);
}
