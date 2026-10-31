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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
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

/// At most this many TokenReviews in flight. The broker's port is reachable
/// from any Pod in the cluster (step Pods running PR code included), and
/// every novel token costs one POST to the API server: this is the bound on
/// what a flood of junk can put on it. The local precheck
/// ([`precheck_jwt`]) only filters obvious junk — a well-formed, unexpired
/// `scarab-broker` JWT with a forged signature passes it — so this is the
/// real bound.
const MAX_CONCURRENT_TOKEN_REVIEWS: usize = 4;
/// How long a call waits for a TokenReview slot before it is refused (and
/// not cached — a busy API server says nothing about the token).
const TOKEN_REVIEW_SLOT_WAIT: Duration = Duration::from_secs(5);
/// A projected ServiceAccount token is ~1 KiB; nothing legitimate is near
/// this.
const MAX_TOKEN_BYTES: usize = 16 * 1024;

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
    /// The one audience a token must be for: `scarab-broker` for the
    /// broker, `scarab-api` for the gate API (ADR-0076 §7). Each consumer
    /// has its own verifier, so a token minted for one is refused by the
    /// other.
    audience: &'static str,
    cache: Mutex<VerdictCache>,
    reviews: tokio::sync::Semaphore,
}

impl TokenReviewVerifier {
    /// A verifier for broker tokens (audience `scarab-broker`).
    pub fn new(client: Client) -> Self {
        Self::for_audience(client, BROKER_TOKEN_AUDIENCE)
    }

    /// A verifier for tokens of `audience` only.
    pub fn for_audience(client: Client, audience: &'static str) -> Self {
        Self {
            client,
            audience,
            cache: Mutex::new(VerdictCache::default()),
            reviews: tokio::sync::Semaphore::new(MAX_CONCURRENT_TOKEN_REVIEWS),
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
        // Junk never reaches the cache or the API server. Not cached either:
        // the check is cheaper than a lookup, and caching junk would only
        // churn out real verdicts.
        precheck_jwt_for(token, unix_now(), self.audience)?;
        let key: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if let Some(verdict) = self.cached(&key) {
            return verdict;
        }
        let _slot = tokio::time::timeout(TOKEN_REVIEW_SLOT_WAIT, self.reviews.acquire())
            .await
            .map_err(|_| TokenRejected("TokenReview busy; try again".into()))?
            .map_err(|_| TokenRejected("TokenReview unavailable".into()))?;
        let review = TokenReview {
            spec: TokenReviewSpec {
                // Asked for explicitly: the API server answers for THIS
                // audience only, so a token minted for it (or for any other
                // consumer) is not authenticated here.
                audiences: Some(vec![self.audience.to_string()]),
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
        let verdict = verdict_for(reviewed.status.as_ref(), self.audience);
        self.remember(key, verdict.clone());
        verdict
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A cheap, local look at the token before anyone asks the API server: a
/// JWT (three base64url segments, JSON header and claims) for the broker
/// audience that has not expired. It proves nothing — the signature is the
/// API server's to check — it only turns away what TokenReview would
/// certainly refuse, so junk costs the cluster nothing.
#[cfg(test)]
fn precheck_jwt(token: &str, now: i64) -> Result<(), TokenRejected> {
    precheck_jwt_for(token, now, BROKER_TOKEN_AUDIENCE)
}

/// [`precheck_jwt`] for tokens of `audience`.
fn precheck_jwt_for(token: &str, now: i64, audience: &str) -> Result<(), TokenRejected> {
    let reject = |why: &str| Err(TokenRejected(format!("not a broker token: {why}")));
    if token.len() > MAX_TOKEN_BYTES {
        return reject("too long");
    }
    let mut segments = token.split('.');
    let (Some(header), Some(claims), Some(signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return reject("not a JWT");
    };
    if signature.is_empty() {
        return reject("unsigned");
    }
    let decode = |segment: &str| -> Option<serde_json::Value> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(segment)
            .ok()?;
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .filter(serde_json::Value::is_object)
    };
    let (Some(header), Some(claims)) = (decode(header), decode(claims)) else {
        return reject("not a JWT");
    };
    if !header.get("alg").is_some_and(serde_json::Value::is_string) {
        return reject("no algorithm");
    }
    let for_broker = match claims.get("aud") {
        Some(serde_json::Value::String(aud)) => aud == audience,
        Some(serde_json::Value::Array(auds)) => auds.iter().any(|a| a.as_str() == Some(audience)),
        _ => false,
    };
    if !for_broker {
        return reject("not for the broker audience");
    }
    match claims.get("exp").and_then(serde_json::Value::as_i64) {
        Some(exp) if exp > now => Ok(()),
        Some(_) => reject("expired"),
        None => reject("no expiry"),
    }
}

/// Judge a TokenReview's answer: authenticated, for the broker audience, a
/// ServiceAccount, and bound to a Pod. Anything less is a rejection —
/// including a legacy Secret-based token or a TokenRequest minted with no
/// bound object, which would outlive any Pod.
#[cfg(test)]
fn verdict_from_status(status: Option<&TokenReviewStatus>) -> Verdict {
    verdict_for(status, BROKER_TOKEN_AUDIENCE)
}

/// [`verdict_from_status`] for tokens of `audience`.
fn verdict_for(status: Option<&TokenReviewStatus>, audience: &str) -> Verdict {
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
        .is_some_and(|a| a.iter().any(|a| a == audience));
    if !for_broker {
        return reject(format!("not a `{audience}` token"));
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
mod audience_tests {
    use super::*;
    use base64::Engine;

    fn jwt(aud: &str, exp: i64) -> String {
        let enc = |v: serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
        };
        format!(
            "{}.{}.sig",
            enc(serde_json::json!({"alg": "RS256"})),
            enc(serde_json::json!({"aud": [aud], "exp": exp}))
        )
    }

    #[test]
    fn a_token_is_taken_only_for_its_own_audience() {
        let now = 1_000;
        let api = jwt(scarab_plugin::API_TOKEN_AUDIENCE, now + 600);
        let broker = jwt(BROKER_TOKEN_AUDIENCE, now + 600);
        assert!(precheck_jwt_for(&api, now, scarab_plugin::API_TOKEN_AUDIENCE).is_ok());
        assert!(precheck_jwt_for(&api, now, BROKER_TOKEN_AUDIENCE).is_err());
        assert!(precheck_jwt_for(&broker, now, scarab_plugin::API_TOKEN_AUDIENCE).is_err());
        let status = |aud: &str| TokenReviewStatus {
            authenticated: Some(true),
            audiences: Some(vec![aud.to_string()]),
            ..Default::default()
        };
        let refused = verdict_for(
            Some(&status(BROKER_TOKEN_AUDIENCE)),
            scarab_plugin::API_TOKEN_AUDIENCE,
        )
        .unwrap_err();
        assert!(refused.0.contains("scarab-api"), "{refused}");
    }
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

    fn jwt(header: &serde_json::Value, claims: &serde_json::Value) -> String {
        let enc = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
        };
        format!("{}.{}.c2lnbmF0dXJl", enc(header), enc(claims))
    }

    #[test]
    fn a_well_formed_unexpired_broker_jwt_passes_the_precheck() {
        let header = serde_json::json!({"alg": "RS256", "kid": "k"});
        let now = 1_000_000;
        for aud in [
            serde_json::json!("scarab-broker"),
            serde_json::json!(["other", "scarab-broker"]),
        ] {
            let token = jwt(&header, &serde_json::json!({"aud": aud, "exp": now + 600}));
            assert_eq!(precheck_jwt(&token, now), Ok(()));
        }
    }

    /// Junk is turned away locally: it never costs a TokenReview.
    #[test]
    fn junk_never_reaches_the_api_server() {
        let header = serde_json::json!({"alg": "RS256"});
        let now = 1_000_000;
        let good = serde_json::json!({"aud": ["scarab-broker"], "exp": now + 600});
        let mut cases: Vec<(&str, String)> = vec![
            ("empty", String::new()),
            ("not a JWT", "hello".into()),
            ("two segments", "a.b".into()),
            ("four segments", format!("{}.x", jwt(&header, &good))),
            ("not base64", "!!.!!.sig".into()),
            ("too long", "a".repeat(MAX_TOKEN_BYTES + 1)),
        ];
        let unsigned = jwt(&header, &good);
        cases.push((
            "unsigned",
            unsigned[..unsigned.rfind('.').unwrap() + 1].to_string(),
        ));
        cases.push(("no alg", jwt(&serde_json::json!({"typ": "JWT"}), &good)));
        cases.push((
            "another audience",
            jwt(
                &header,
                &serde_json::json!({"aud": ["https://kubernetes.default.svc"], "exp": now + 600}),
            ),
        ));
        cases.push((
            "no audience",
            jwt(&header, &serde_json::json!({"exp": now + 600})),
        ));
        cases.push((
            "expired",
            jwt(
                &header,
                &serde_json::json!({"aud": "scarab-broker", "exp": now}),
            ),
        ));
        cases.push((
            "no expiry",
            jwt(&header, &serde_json::json!({"aud": "scarab-broker"})),
        ));
        for (name, token) in cases {
            assert!(precheck_jwt(&token, now).is_err(), "{name}");
        }
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
