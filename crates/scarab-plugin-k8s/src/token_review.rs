//! [`TokenReviewVerifier`]: who is calling the broker, as the cluster vouches
//! for it (ADR-0072 §11).
//!
//! TokenReview rather than JWKS: the API server checks the **bound object**
//! — a projected token dies with its Pod and its ServiceAccount, so deleting
//! a killed installation's ServiceAccount revokes its tokens at the source —
//! and it needs no issuer or JWKS reachability, so it works the same on
//! kind, k3s and EKS, and from a proc-mode host process through the kind
//! kubeconfig. The control plane's ServiceAccount needs
//! `system:auth-delegator` (the chart binds it).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec, TokenReviewStatus};
use kube::api::PostParams;
use kube::{Api, Client};
use sha2::{Digest, Sha256};

use scarab_plugin::{
    PluginTokenVerifier, TokenRejected, VerifiedServiceAccount, BROKER_TOKEN_AUDIENCE,
};

/// How long a verdict is reused. Short, because a cached "yes" outlives the
/// Pod or ServiceAccount it was bound to by at most this long — and the kill
/// switch does not depend on it at all: the broker reads the installation's
/// state from the database on every request.
pub const VERDICT_CACHE_TTL: Duration = Duration::from_secs(30);

/// The most verdicts held. A flood of junk tokens can churn the cache but
/// never grow it; past this, expired entries go, and if that is not enough,
/// everything does (the next calls just ask the API server again).
const VERDICT_CACHE_MAX_ENTRIES: usize = 4096;

/// The `extra` keys the API server sets on a token bound to a Pod.
const POD_NAME_EXTRA: &str = "authentication.kubernetes.io/pod-name";
const POD_UID_EXTRA: &str = "authentication.kubernetes.io/pod-uid";

const SERVICE_ACCOUNT_PREFIX: &str = "system:serviceaccount:";

type Verdict = Result<VerifiedServiceAccount, TokenRejected>;

/// Verdicts by SHA-256 of the token: the cache never holds a usable token.
#[derive(Default)]
struct VerdictCache {
    entries: HashMap<[u8; 32], (Instant, Verdict)>,
}

impl VerdictCache {
    fn get(&mut self, key: &[u8; 32], now: Instant) -> Option<Verdict> {
        match self.entries.get(key) {
            Some((expires, verdict)) if *expires > now => Some(verdict.clone()),
            Some(_) => {
                self.entries.remove(key);
                None
            }
            None => None,
        }
    }

    fn put(&mut self, key: [u8; 32], verdict: Verdict, now: Instant) {
        if self.entries.len() >= VERDICT_CACHE_MAX_ENTRIES {
            self.entries.retain(|_, (expires, _)| *expires > now);
            if self.entries.len() >= VERDICT_CACHE_MAX_ENTRIES {
                self.entries.clear();
            }
        }
        self.entries.insert(key, (now + VERDICT_CACHE_TTL, verdict));
    }
}

/// Verifies broker tokens with TokenReview: audience `scarab-broker`, a
/// ServiceAccount identity, bound to a live Pod. Which namespace and which
/// ServiceAccount are acceptable is the broker's to decide (it maps the
/// ServiceAccount to an installation); this only proves the identity.
pub struct TokenReviewVerifier {
    client: Client,
    cache: Mutex<VerdictCache>,
}

impl TokenReviewVerifier {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            cache: Mutex::new(VerdictCache::default()),
        }
    }

    fn cached(&self, key: &[u8; 32]) -> Option<Verdict> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key, Instant::now())
    }

    fn remember(&self, key: [u8; 32], verdict: Verdict) {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .put(key, verdict, Instant::now());
    }
}

#[async_trait]
impl PluginTokenVerifier for TokenReviewVerifier {
    async fn verify(&self, token: &str) -> Result<VerifiedServiceAccount, TokenRejected> {
        let key: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if let Some(verdict) = self.cached(&key) {
            return verdict;
        }
        let review = TokenReview {
            spec: TokenReviewSpec {
                // Asked for explicitly: the API server answers for THIS
                // audience only, so a token minted for it (or for any other
                // consumer) is not authenticated here.
                audiences: Some(vec![BROKER_TOKEN_AUDIENCE.to_string()]),
                token: token.to_string(),
            },
            ..Default::default()
        };
        let reviewed = Api::<TokenReview>::all(self.client.clone())
            .create(&PostParams::default(), &review)
            .await
            // Not cached: an API server blip must not pin a "no" on a good
            // token for the cache's lifetime. Fail closed for this call only.
            .map_err(|e| TokenRejected(format!("TokenReview unavailable: {e}")))?;
        let verdict = verdict_from_status(reviewed.status.as_ref());
        self.remember(key, verdict.clone());
        verdict
    }
}

/// Judge a TokenReview's answer: authenticated, for the broker audience, a
/// ServiceAccount, and bound to a Pod. Anything less is a rejection —
/// including a legacy Secret-based token or a TokenRequest minted with no
/// bound object, which would outlive any Pod.
fn verdict_from_status(status: Option<&TokenReviewStatus>) -> Verdict {
    let reject = |why: String| Err(TokenRejected(why));
    let Some(status) = status else {
        return reject("TokenReview returned no status".into());
    };
    if status.authenticated != Some(true) {
        return reject(format!(
            "not authenticated{}",
            status
                .error
                .as_deref()
                .filter(|e| !e.is_empty())
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        ));
    }
    let for_broker = status
        .audiences
        .as_ref()
        .is_some_and(|a| a.iter().any(|a| a == BROKER_TOKEN_AUDIENCE));
    if !for_broker {
        return reject(format!("not a `{BROKER_TOKEN_AUDIENCE}` token"));
    }
    let Some(user) = status.user.as_ref() else {
        return reject("authenticated, but no user".into());
    };
    let username = user.username.as_deref().unwrap_or("");
    let Some((namespace, service_account)) = username
        .strip_prefix(SERVICE_ACCOUNT_PREFIX)
        .and_then(|rest| rest.split_once(':'))
        .filter(|(ns, sa)| !ns.is_empty() && !sa.is_empty())
    else {
        return reject(format!("`{username}` is not a ServiceAccount"));
    };
    let bound = |key: &str| {
        user.extra
            .as_ref()
            .and_then(|extra| extra.get(key))
            .is_some_and(|values| values.iter().any(|v| !v.is_empty()))
    };
    if !(bound(POD_NAME_EXTRA) && bound(POD_UID_EXTRA)) {
        return reject(format!(
            "the token of `{username}` is not bound to a Pod (only a projected token is accepted)"
        ));
    }
    Ok(VerifiedServiceAccount {
        namespace: namespace.to_string(),
        service_account: service_account.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::authentication::v1::UserInfo;
    use std::collections::BTreeMap;

    fn bound_status(username: &str) -> TokenReviewStatus {
        TokenReviewStatus {
            authenticated: Some(true),
            audiences: Some(vec!["scarab-broker".into()]),
            user: Some(UserInfo {
                username: Some(username.into()),
                extra: Some(BTreeMap::from([
                    (
                        POD_NAME_EXTRA.to_string(),
                        vec!["plugin-0123456789-abc".into()],
                    ),
                    (POD_UID_EXTRA.to_string(), vec!["7b1d".into()]),
                ])),
                ..Default::default()
            }),
            error: None,
        }
    }

    #[test]
    fn a_pod_bound_broker_token_names_its_service_account() {
        assert_eq!(
            verdict_from_status(Some(&bound_status(
                "system:serviceaccount:scarab-plugins:plugin-0123456789"
            ))),
            Ok(VerifiedServiceAccount {
                namespace: "scarab-plugins".into(),
                service_account: "plugin-0123456789".into(),
            })
        );
    }

    #[test]
    fn anything_less_is_rejected() {
        let sa = "system:serviceaccount:scarab-plugins:plugin-0123456789";
        let mut cases: Vec<(&str, TokenReviewStatus)> = Vec::new();
        let mut s = bound_status(sa);
        s.authenticated = Some(false);
        s.error = Some("token has expired".into());
        cases.push(("unauthenticated", s));
        let mut s = bound_status(sa);
        s.authenticated = None;
        cases.push(("no verdict", s));
        let mut s = bound_status(sa);
        s.audiences = Some(vec!["https://kubernetes.default.svc".into()]);
        cases.push(("another audience", s));
        let mut s = bound_status(sa);
        s.audiences = None;
        cases.push(("no audience", s));
        cases.push(("a human", bound_status("amy@example.com")));
        cases.push((
            "a malformed ServiceAccount",
            bound_status("system:serviceaccount:scarab-plugins"),
        ));
        let mut s = bound_status(sa);
        s.user.as_mut().unwrap().extra = None;
        cases.push(("not bound to a Pod", s));
        let mut s = bound_status(sa);
        s.user
            .as_mut()
            .unwrap()
            .extra
            .as_mut()
            .unwrap()
            .remove(POD_UID_EXTRA);
        cases.push(("no Pod uid", s));
        for (name, status) in cases {
            assert!(verdict_from_status(Some(&status)).is_err(), "{name}");
        }
        assert!(verdict_from_status(None).is_err());
    }

    #[test]
    fn a_verdict_is_reused_only_until_it_expires() {
        let mut cache = VerdictCache::default();
        let t0 = Instant::now();
        let verdict: Verdict = Ok(VerifiedServiceAccount {
            namespace: "scarab-plugins".into(),
            service_account: "plugin-0123456789".into(),
        });
        cache.put([1; 32], verdict.clone(), t0);
        assert_eq!(
            cache.get(&[1; 32], t0 + Duration::from_secs(29)),
            Some(verdict)
        );
        assert_eq!(cache.get(&[1; 32], t0 + VERDICT_CACHE_TTL), None);
        assert_eq!(cache.get(&[2; 32], t0), None);
    }

    #[test]
    fn a_flood_of_tokens_never_grows_the_cache_past_its_bound() {
        let mut cache = VerdictCache::default();
        let t0 = Instant::now();
        for i in 0..(VERDICT_CACHE_MAX_ENTRIES + 10) {
            let mut key = [0u8; 32];
            key[..8].copy_from_slice(&(i as u64).to_le_bytes());
            cache.put(key, Err(TokenRejected("junk".into())), t0);
            assert!(cache.entries.len() <= VERDICT_CACHE_MAX_ENTRIES);
        }
    }
}
