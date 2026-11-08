//! `LoginKind` from stored and serialized strings.

use scarab_identity::{LoginKind, Session};

#[test]
fn login_kind_reads_only_its_exact_strings() {
    for (s, want) in [
        ("browser", LoginKind::Browser),
        ("api", LoginKind::Api),
        ("", LoginKind::Unrecorded),
        ("API", LoginKind::Unrecorded),
        ("Browser", LoginKind::Unrecorded),
        (" browser", LoginKind::Unrecorded),
        ("browser\0", LoginKind::Unrecorded),
        ("sso", LoginKind::Unrecorded),
        ("brówser", LoginKind::Unrecorded),
    ] {
        assert_eq!(LoginKind::from_stored(s), want, "{s:?}");
        let from_json: LoginKind = serde_json::from_value(serde_json::json!(s)).unwrap();
        assert_eq!(from_json, want, "json {s:?}");
    }
    for k in [LoginKind::Browser, LoginKind::Api, LoginKind::Unrecorded] {
        assert_eq!(LoginKind::from_stored(k.as_str()), k);
        let v = serde_json::to_value(k).unwrap();
        assert_eq!(serde_json::from_value::<LoginKind>(v).unwrap(), k);
    }
}

/// A serialized Session whose `login_kind` is "Browser" (wrong case) or
/// unknown is never fresh; one with the key absent reads Unrecorded.
#[test]
fn a_session_with_an_odd_login_kind_is_never_fresh() {
    let base = |kind: Option<serde_json::Value>| {
        let mut v = serde_json::json!({
            "id": "sid",
            "principal": { "subject": "amy", "roles": [] },
            "expires_at": i64::MAX,
            "csrf_hash": "",
            "created_at": 1_000,
        });
        if let Some(k) = kind {
            v["login_kind"] = k;
        }
        v
    };
    for k in ["Browser", "API", "sso", ""] {
        let s: Session = serde_json::from_value(base(Some(serde_json::json!(k)))).unwrap();
        assert_eq!(s.fresh_login_until(), None, "{k:?}");
    }
    let s: Session = serde_json::from_value(base(None)).unwrap();
    assert_eq!(s.login_kind, LoginKind::Unrecorded);
    let s: Session = serde_json::from_value(base(Some(serde_json::json!("browser")))).unwrap();
    assert_eq!(
        s.fresh_login_until(),
        Some(1_000 + scarab_identity::FRESH_LOGIN_MS)
    );
    // A JSON null login_kind is refused, as the old `String` field refused it.
    let null = serde_json::from_value::<Session>(base(Some(serde_json::Value::Null)));
    assert!(null.is_err(), "{:?}", null.map(|s| s.login_kind));
}
