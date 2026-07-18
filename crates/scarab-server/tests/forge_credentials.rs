//! ForgeConnection credentials resolve at USE-time from SecretProvider
//! (ADR-0046): the connection row carries only `credential_ref`; the material
//! lives under the reserved `_forge` org scope and never on the connection.

use scarab_forge::{ForgeConnection, ForgeKind};
use scarab_secrets::SecretScope;
use scarab_server::{connection_credential, FORGE_CREDENTIALS_ORG};
use scarab_testkit::FakeSecrets;

fn conn(credential_ref: &str) -> ForgeConnection {
    ForgeConnection {
        id: "gh-acme".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: credential_ref.into(),
    }
}

#[tokio::test]
async fn credential_material_resolves_by_handle_at_use_time() {
    let scope = SecretScope::Org {
        org: FORGE_CREDENTIALS_ORG.to_string(),
    };
    let secrets = FakeSecrets::new().with_secret(&scope, "gh-acme-app-pem", b"PEM BYTES");

    let bytes = connection_credential(&secrets, &conn("gh-acme-app-pem"))
        .await
        .expect("credential resolves");
    assert_eq!(bytes, b"PEM BYTES");

    // A dangling handle fails loudly — never a silent empty credential.
    assert!(connection_credential(&secrets, &conn("missing"))
        .await
        .is_err());
}
