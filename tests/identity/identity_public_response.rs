#![cfg(not(target_arch = "wasm32"))]

use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client as requests;
use crate::support::native_database as native;

use axum::http::StatusCode;
use codeyard::backend::api::user::responses::PublicUserResponse;
use codeyard::backend::db;
use codeyard::backend::db::users::accounts::AuthorDisplay;
use serde_json::json;

#[tokio::test]
async fn profile_updates_reject_nul_in_json_and_native_forms_on_both_engines() {
    bundle::install();
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    for pool in [&postgres.pool, &sqlite.pool] {
        let db::users::transitions::SetupClaim::Completed(user) =
            db::users::transitions::complete_setup(
                pool,
                "profile_owner",
                None,
                "fixture",
                "public",
                None,
            )
            .await
            .unwrap()
        else {
            panic!("fresh setup must succeed")
        };
        let root = tempfile::tempdir().unwrap();
        let router = crate::support::database::router(pool, root.path()).await;
        let client = requests::Client::login(&router, pool, user.id).await;
        let valid = json!({
            "display_name": "é".repeat(50), "bio": "é".repeat(2000),
            "location": "é".repeat(100), "website": format!("https://example.com/{}", "x".repeat(235)),
        });
        let original = client
            .expect("PATCH", "/api/user", valid.clone(), StatusCode::OK)
            .await;
        for field in ["display_name", "bio", "location", "website"] {
            let error = client
                .expect(
                    "PATCH",
                    "/api/user",
                    json!({field: "a\0b"}),
                    StatusCode::BAD_REQUEST,
                )
                .await;
            assert_eq!(error["error"], "validation_error");
            assert!(error["message"].as_str().unwrap().contains("NUL"));
            let (status, page) = client.form("/settings/profile", &[(field, "a\0b")]).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
            assert!(page.contains("Profile fields must not contain NUL characters"));
            // A website that is not a web address is refused with a message that says so.
            let (status, page) = client
                .form("/settings/profile", &[("website", "not a url")])
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
            assert!(
                page.contains("Website must be a web address starting with http:// or https://")
            );
            // /api/user accepts only PATCH; the public profile carries the same four fields.
            let stored = client
                .expect(
                    "GET",
                    "/api/users/profile_owner",
                    serde_json::Value::Null,
                    StatusCode::OK,
                )
                .await;
            for key in ["display_name", "bio", "location", "website"] {
                assert_eq!(stored[key], original[key]);
            }
        }
        let fields: Vec<_> = valid
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str().unwrap()))
            .collect();
        let (status, page) = client.form("/settings/profile", &fields).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    }
}

#[tokio::test]
async fn setup_rejects_nul_email_on_both_engines() {
    use codeyard::backend::identity::{
        credentials::PasswordAccount,
        sessions::{InitiatingSession, SessionContext},
        setup::{self, SetupError, SetupInput},
    };
    for pg in [false, true] {
        let mut database = crate::support::database::Database::new(pg).await;
        let fresh = native::token();
        let result = setup::complete_setup(
            database.pool(),
            SetupInput {
                account: PasswordAccount {
                    username: "setup-owner",
                    email: Some("bad\0@example.test"),
                    password: "correct-password",
                },
                node_mode: "public",
                admin_secret: None,
            },
            None,
            SessionContext {
                initiating: InitiatingSession::Missing,
                fresh_id: &fresh,
                user_agent: None,
                client: None,
                expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            },
        )
        .await;
        assert!(matches!(result, Err(SetupError::InputBounds)));
        assert!(
            db::users::accounts::get_user_by_username(database.pool(), "setup-owner")
                .await
                .unwrap()
                .is_none()
        );
        database.finish().await;
    }
}

#[test]
fn author_display_preserves_public_user_wire_fields() {
    let avatar = uuid::Uuid::from_u128(1);
    let mut author = AuthorDisplay {
        id: 42,
        username: "alice".into(),
        avatar_file_id: Some(avatar),
        display_name: Some("Alice".into()),
        bio: Some("Writes code".into()),
        location: Some("Earth".into()),
        website: Some("https://example.com".into()),
        is_admin: true,
        created_at: "2025-01-02T03:04:05Z".parse().unwrap(),
    };
    assert_eq!(
        serde_json::to_value(PublicUserResponse::from_display(&author)).unwrap(),
        json!({
            "id": 42,
            "username": "alice",
            "avatar_url": format!("/api/files/{avatar}/download"),
            "display_name": "Alice",
            "bio": "Writes code",
            "location": "Earth",
            "website": "https://example.com",
            "is_admin": true,
            "created_at": "2025-01-02T03:04:05+00:00",
        }),
    );

    author.avatar_file_id = None;
    author.display_name = None;
    author.bio = None;
    author.location = None;
    author.website = None;
    let response = serde_json::to_value(PublicUserResponse::from_display(&author)).unwrap();
    for field in ["avatar_url", "display_name", "bio", "location", "website"] {
        assert_eq!(response[field], serde_json::Value::Null);
    }
    assert_eq!(response.as_object().unwrap().len(), 9);
}
