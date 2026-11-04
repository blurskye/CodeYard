use super::github::request;
use crate::support::database::Database;
use crate::support::http_client::Client;
use codeyard::backend::db;
use codeyard::backend::identity;
use codeyard::backend::identity::gpg;
use codeyard::backend::identity::tokens;
use codeyard_identity::tokens::TokenInput;
use pgp::{
    composed::{
        EncryptionCaps, KeyType, SecretKeyParamsBuilder, SignedPublicKey, SignedSecretKey,
        SubkeyParamsBuilder,
    },
    crypto::ecc_curve::ECCCurve,
};
use serde_json::json;

pub(super) fn key(email: &str) -> SignedSecretKey {
    let sign = SubkeyParamsBuilder::default()
        .key_type(KeyType::Ed25519Legacy)
        .can_sign(true)
        .build()
        .unwrap();
    let encrypt = SubkeyParamsBuilder::default()
        .key_type(KeyType::ECDH(ECCCurve::Curve25519Legacy))
        .can_encrypt(EncryptionCaps::All)
        .build()
        .unwrap();
    SecretKeyParamsBuilder::default()
        .key_type(KeyType::Ed25519Legacy)
        .can_certify(true)
        .can_sign(true)
        .primary_user_id(format!("Alice <{email}>"))
        .user_ids(vec!["Work <work@example.com>".into()])
        .subkeys(vec![sign, encrypt])
        .build()
        .unwrap()
        .generate(rand::thread_rng())
        .unwrap()
}
pub(super) fn armor(key: &SignedSecretKey) -> String {
    SignedPublicKey::from(key.clone())
        .to_armored_string(Default::default())
        .unwrap()
}

#[test]
fn gpg_parsing_rejects_private_packets_forged_bindings_and_multiple_keys() {
    let secret = key("alice@example.com");
    let outsider = key("outsider@example.com");
    let public = SignedPublicKey::from(secret.clone());
    let armored = armor(&secret);
    let parsed = gpg::parse(&armored).unwrap();
    assert!(parsed.material.can_sign && parsed.material.can_certify);
    assert_eq!(
        parsed.material.emails,
        ["alice@example.com", "work@example.com"]
    );
    assert_eq!(parsed.subkeys.len(), 2);
    assert!(parsed.subkeys[0].material.can_sign);
    assert!(
        parsed.subkeys[1].material.can_encrypt_comms
            && parsed.subkeys[1].material.can_encrypt_storage
    );
    assert!(!parsed.subkeys[1].material.can_sign);
    let mut forged = public.clone();
    forged.details.users = SignedPublicKey::from(outsider.clone()).details.users;
    assert_eq!(
        gpg::parse(&forged.to_armored_string(Default::default()).unwrap()).unwrap_err(),
        gpg::GpgError::InvalidKey
    );
    let mut forged = public.clone();
    forged.public_subkeys[0].key = SignedPublicKey::from(outsider).public_subkeys[0]
        .key
        .clone();
    assert_eq!(
        gpg::parse(&forged.to_armored_string(Default::default()).unwrap()).unwrap_err(),
        gpg::GpgError::InvalidKey
    );
    let private = secret.to_armored_string(Default::default()).unwrap();
    assert_eq!(gpg::parse(&private).unwrap_err(), gpg::GpgError::InvalidKey);
    assert_eq!(
        gpg::parse(&private.replace("PRIVATE KEY BLOCK", "PUBLIC KEY BLOCK")).unwrap_err(),
        gpg::GpgError::InvalidKey
    );
    assert_eq!(
        gpg::parse(&format!("{armored}\n{armored}")).unwrap_err(),
        gpg::GpgError::InvalidKey
    );
    assert_eq!(
        gpg::parse("not a key").unwrap_err(),
        gpg::GpgError::InvalidKey
    );
}

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    f.execute(
        "UPDATE users SET email='alice@example.com',email_verified=TRUE WHERE id=$1",
        &[alice],
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let session = Client::login(&router, f.pool(), alice).await;
    let other_session = Client::login(&router, f.pool(), bob).await;
    let mut credentials = Vec::new();
    for (actor, session, scope) in [
        (alice, &session.session, "write"),
        (alice, &session.session, "read"),
        (bob, &other_session.session, "write"),
    ] {
        let token = tokens::create(
            f.pool(),
            actor,
            Some(session),
            TokenInput {
                name: scope,
                scope,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        credentials.push(format!("Bearer {}", token.value));
    }
    let write = Some(credentials[0].as_str());
    request(
        &router,
        "users/list-gpg-keys-for-authenticated-user",
        "GET",
        "/api/v3/user/gpg_keys",
        None,
        json!(null),
        401,
    )
    .await;
    request(
        &router,
        "users/create-gpg-key-for-authenticated-user",
        "POST",
        "/api/v3/user/gpg_keys",
        Some(&credentials[1]),
        json!({"armored_public_key":"invalid"}),
        403,
    )
    .await;
    for input in [
        json!({}),
        json!({"armored_public_key":"invalid"}),
        json!({"armored_public_key":true}),
    ] {
        request(
            &router,
            "users/create-gpg-key-for-authenticated-user",
            "POST",
            "/api/v3/user/gpg_keys",
            write,
            input,
            422,
        )
        .await;
    }
    let secret = key("alice@example.com");
    let armored = armor(&secret);
    let (_, created) = request(
        &router,
        "users/create-gpg-key-for-authenticated-user",
        "POST",
        "/api/v3/user/gpg_keys",
        write,
        json!({"name":"Laptop signing","armored_public_key":armored}),
        201,
    )
    .await;
    assert_eq!(created["name"], "Laptop signing");
    let id = created["id"].as_i64().unwrap();
    assert!(created["primary_key_id"].is_null());
    assert_eq!(created["key_id"].as_str().unwrap().len(), 16);
    assert_eq!(created["can_sign"], true);
    assert_eq!(created["can_certify"], true);
    assert_eq!(created["revoked"], false);
    assert!(created["expires_at"].is_null());
    assert!(created["raw_key"]
        .as_str()
        .unwrap()
        .contains("PGP PUBLIC KEY BLOCK"));
    assert_eq!(
        created["emails"],
        json!([{"email":"alice@example.com","verified":true},{"email":"work@example.com","verified":false}])
    );
    let subkeys = created["subkeys"].as_array().unwrap();
    assert_eq!(subkeys.len(), 2);
    assert_ne!(subkeys[0]["id"], id);
    assert_eq!(subkeys[0]["primary_key_id"], id);
    assert_eq!(subkeys[0]["can_sign"], true);
    assert_eq!(subkeys[1]["can_sign"], false);
    assert_eq!(subkeys[1]["can_encrypt_comms"], true);
    for subkey in subkeys {
        assert!(subkey["raw_key"].is_null());
    }
    request(
        &router,
        "users/create-gpg-key-for-authenticated-user",
        "POST",
        "/api/v3/user/gpg_keys",
        write,
        json!({"armored_public_key":armored}),
        422,
    )
    .await;
    request(
        &router,
        "users/create-gpg-key-for-authenticated-user",
        "POST",
        "/api/v3/user/gpg_keys",
        Some(&credentials[2]),
        json!({"armored_public_key":armored}),
        422,
    )
    .await;
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM gpg_keys WHERE user_id=$1", &[bob])
            .await,
        0
    );
    request(
        &router,
        "users/get-gpg-key-for-authenticated-user",
        "GET",
        &format!("/api/v3/user/gpg_keys/{id}"),
        Some(&credentials[2]),
        json!(null),
        404,
    )
    .await;
    request(
        &router,
        "users/delete-gpg-key-for-authenticated-user",
        "DELETE",
        &format!("/api/v3/user/gpg_keys/{id}"),
        Some(&credentials[2]),
        json!(null),
        404,
    )
    .await;
    request(
        &router,
        "users/delete-gpg-key-for-authenticated-user",
        "DELETE",
        &format!("/api/v3/user/gpg_keys/{id}"),
        Some(&credentials[1]),
        json!(null),
        403,
    )
    .await;
    let (_, public) = request(
        &router,
        "users/list-gpg-keys-for-user",
        "GET",
        "/api/v3/users/alice/gpg_keys",
        None,
        json!(null),
        200,
    )
    .await;
    assert_eq!(public, json!([created]));
    // A key UID never verifies an account address by itself; verification comes from email proof.
    identity::emails::change(
        f.pool(),
        alice,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&session.session),
        &["work@example.com".into()],
        true,
    )
    .await
    .unwrap();
    f.execute(
        "UPDATE user_emails SET verified=TRUE WHERE user_id=$1",
        &[alice],
    )
    .await;
    let (_, detail) = request(
        &router,
        "users/get-gpg-key-for-authenticated-user",
        "GET",
        &format!("/api/v3/user/gpg_keys/{id}"),
        Some(&credentials[1]),
        json!(null),
        200,
    )
    .await;
    assert_eq!(detail["emails"][1]["verified"], true);
    let second = key("second@example.com");
    request(
        &router,
        "users/create-gpg-key-for-authenticated-user",
        "POST",
        "/api/v3/user/gpg_keys",
        write,
        json!({"armored_public_key":armor(&second)}),
        201,
    )
    .await;
    let (headers, list) = request(
        &router,
        "users/list-gpg-keys-for-authenticated-user",
        "GET",
        "/api/v3/user/gpg_keys?per_page=1",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let page = session.page_ok("/settings?tab=keys").await;
    assert!(page.contains("Laptop signing"));
    assert!(page.contains("/settings/gpg-keys"));
    assert!(page.contains("name=\"armored_public_key\""));
    let private = secret.to_armored_string(Default::default()).unwrap();
    let reply = session
        .form_reply("/settings/gpg-keys", &[("armored_public_key", &private)])
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!reply.text().contains("BEGIN PGP PRIVATE KEY"));
    request(
        &router,
        "users/delete-gpg-key-for-authenticated-user",
        "DELETE",
        &format!("/api/v3/user/gpg_keys/{id}"),
        write,
        json!(null),
        204,
    )
    .await;
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM gpg_keys WHERE primary_key_id=$1",
            &[id]
        )
        .await,
        0
    );
    let reply = session
        .form_reply(
            "/settings/gpg-keys",
            &[("gpg_name", "Native GPG"), ("armored_public_key", &armored)],
        )
        .await;
    assert_eq!(
        reply.status,
        axum::http::StatusCode::SEE_OTHER,
        "{}",
        reply.text()
    );
    let keys = codeyard_db_identity::users::gpg_keys::list_gpg_keys(f.pool(), alice, 100, 0)
        .await
        .unwrap();
    let native = keys
        .iter()
        .find(|key| key.name.as_deref() == Some("Native GPG"))
        .unwrap();
    let reply = session
        .form_reply(&format!("/settings/gpg-keys/{}/delete", native.id), &[])
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::SEE_OTHER);
    assert!(
        codeyard_db_identity::users::gpg_keys::get_gpg_key(f.pool(), alice, native.id)
            .await
            .unwrap()
            .is_none()
    );
    let error = gpg::enroll(
        f.pool(),
        alice,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Token(i64::MAX, ""),
        None,
        &armored,
    )
    .await;
    assert_eq!(error.unwrap_err(), gpg::GpgError::Denied);
    f.finish().await;
}
#[tokio::test]
async fn sqlite_github_gpg_keys() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_gpg_keys() {
    contract(true).await;
}
