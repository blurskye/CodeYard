//! A cookie's routing prefix cannot alias a local session with the same random value.
use crate::support::{database::Database, http_client::Client};
use axum::http::StatusCode;
use codeyard_types::cells::Prefix;

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut database = Database::new(pg).await;
    database.complete_setup().await;
    let actor = database.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let router = database.router(root.path()).await;
    let mut client = Client::login(&router, database.pool(), actor).await;
    let stored = client.session.clone();
    let random = codeyard_types::cells::Secret::cookie(&stored)
        .unwrap()
        .secret
        .to_owned();
    let local = client
        .exchange("GET", "/settings", "text/plain", Vec::new())
        .await;
    assert_eq!(local.status, StatusCode::OK);
    client.session = Prefix::new(1).cookie(&random).unwrap();
    let foreign = client
        .exchange("GET", "/settings", "text/plain", Vec::new())
        .await;
    assert_eq!(foreign.status, StatusCode::SEE_OTHER);
    assert!(foreign.location.unwrap().starts_with("/login"));
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf_token", &client.csrf)
        .finish()
        .into_bytes();
    let logout = client
        .exchange(
            "POST",
            "/logout",
            "application/x-www-form-urlencoded",
            body.clone(),
        )
        .await;
    assert_eq!(logout.status, StatusCode::FORBIDDEN);
    assert_eq!(
        codeyard_db_identity::users::sessions::get_session_account(database.pool(), &stored)
            .await
            .unwrap()
            .unwrap()
            .id,
        actor
    );
    client.session = stored.clone();
    let local_logout = client
        .exchange("POST", "/logout", "application/x-www-form-urlencoded", body)
        .await;
    assert_eq!(local_logout.status, StatusCode::SEE_OTHER);
    assert!(
        codeyard_db_identity::users::sessions::get_session_account(database.pool(), &stored)
            .await
            .unwrap()
            .is_none()
    );
    database.finish().await;
}

#[tokio::test]
async fn foreign_cookie_cannot_alias_or_revoke_a_local_postgres_session() {
    contract(true).await;
}
#[tokio::test]
async fn foreign_cookie_cannot_alias_or_revoke_a_local_sqlite_session() {
    contract(false).await;
}
