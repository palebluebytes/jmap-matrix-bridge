#![allow(clippy::unwrap_used)]

//! Tests for automatic double-puppet via shared-secret-auth (#28): the
//! HMAC-SHA512 login password and the token mint (a password login whose
//! password is that HMAC).

use jmap_matrix_bridge::puppet::{mint_via_shared_secret, shared_secret_password};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn shared_secret_password_is_hmac_sha512_hex() {
    let p = shared_secret_password("secret", "@alice:example.com");
    // HMAC-SHA512 → 64 bytes → 128 lowercase hex chars (distinguishes SHA512
    // from SHA256, which would be 64).
    assert_eq!(p.len(), 128);
    assert!(
        p.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    // Deterministic, and sensitive to both the mxid and the secret.
    assert_eq!(p, shared_secret_password("secret", "@alice:example.com"));
    assert_ne!(p, shared_secret_password("secret", "@bob:example.com"));
    assert_ne!(p, shared_secret_password("other", "@alice:example.com"));
}

#[tokio::test]
async fn mint_logs_in_with_the_hmac_password_and_returns_the_token() {
    let server = MockServer::start().await;
    let expected_pw = shared_secret_password("sek", "@alice:localhost");

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/login"))
        // The login must carry the computed HMAC as its password.
        .and(body_string_contains(expected_pw))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "minted-tok",
            "user_id": "@alice:localhost",
            "device_id": "JMAP_BRIDGE_PUPPET"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let token = mint_via_shared_secret(&server.uri(), "@alice:localhost", "sek")
        .await
        .unwrap();
    assert_eq!(token, "minted-tok");
}
