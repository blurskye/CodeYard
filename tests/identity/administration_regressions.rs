use crate::support::{database::Database, fake_webui_bundle, http_client::Client};
use axum::http::StatusCode;
use codeyard::backend::{db, identity};
use codeyard::webui::{app::Page, pages::account};

async fn bounded_administration(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,node_mode,setup_completed) VALUES(1,'private',TRUE) ON CONFLICT(id) DO UPDATE SET node_mode='private',setup_completed=TRUE", &[]).await;
    let admin = f.actor("pagination_admin").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[admin])
        .await;
    for index in 0..105 {
        f.actor(&format!("page_user_{index:03}")).await;
        db::users::signup_requests::create_signup_request(
            f.pool(),
            &format!("pending_{index:03}"),
            None,
            "hash",
        )
        .await
        .unwrap();
        f.execute(&format!("INSERT INTO ssh_keys(user_id,name,fingerprint,public_key,key_type) VALUES($1,'key-{index}','fingerprint-{index}','{}','ssh-ed25519')", "x".repeat(32000)), &[admin]).await;
    }
    assert_eq!(
        db::users::accounts::list_admin_account_summaries(f.pool(), 1000, 0)
            .await
            .unwrap()
            .len(),
        100
    );
    assert!(
        !db::users::accounts::list_admin_account_summaries(f.pool(), 100, 100)
            .await
            .unwrap()
            .is_empty()
    );
    let requests =
        db::users::signup_requests::list_signup_requests(f.pool(), "pending", 1000, 0, 0)
            .await
            .unwrap();
    assert_eq!(requests.len(), 100);
    let next = db::users::signup_requests::list_signup_requests(
        f.pool(),
        "pending",
        100,
        0,
        requests.last().unwrap().id,
    )
    .await
    .unwrap();
    assert_eq!(next.len(), 5);
    assert!(next.iter().all(|row| row.id > requests.last().unwrap().id));
    assert_eq!(
        db::users::signup_requests::count_signup_requests(f.pool(), "pending")
            .await
            .unwrap(),
        105
    );
    let keys = db::users::ssh_keys::list_user_key_summaries(f.pool(), admin, 0)
        .await
        .unwrap();
    assert_eq!(keys.len(), 100);
    assert_eq!(
        db::users::ssh_keys::list_user_key_summaries(f.pool(), admin, keys.last().unwrap().id)
            .await
            .unwrap()
            .len(),
        5
    );

    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let client = Client::login(&router, f.pool(), admin).await;
    let (status, page) = client.model("/admin").await;
    assert_eq!(status, StatusCode::OK);
    let Page::Identity(account::Page::Admin { model }) = page else {
        panic!("admin model")
    };
    assert_eq!(model.pending_requests, Some(105));
    let (status, page) = client.model("/admin/users").await;
    assert_eq!(status, StatusCode::OK);
    let Page::Identity(account::Page::AdminUsers { model, .. }) = page else {
        panic!("users model")
    };
    assert_eq!(model.requests.len(), 30);
    assert_eq!(model.requests_count, 105);
    let (status, body) = client
        .send("GET", "/admin/users", "text/html", String::new())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Next requests"));
    let (status, body) = client
        .send("GET", "/settings?tab=keys", "text/html", String::new())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Next SSH keys"));
    assert!(!body.contains(&"x".repeat(1000)));
    let (_, keys) = client
        .api("GET", "/api/user/keys", serde_json::Value::Null)
        .await;
    assert_eq!(keys.as_array().unwrap().len(), 100);
    assert!(!keys.to_string().contains("public_key"));
    f.finish().await;
}

async fn account_writes(pg: bool) {
    let mut f = Database::new(pg).await;
    // All fixture data is disposable; setup needs an installation with no accounts.
    f.execute("DELETE FROM users", &[]).await;
    f.execute("DELETE FROM node_configuration", &[]).await;
    let signed_in = identity::setup::complete_setup(
        f.pool(),
        identity::setup::SetupInput {
            account: identity::credentials::PasswordAccount {
                username: "setup_admin",
                email: Some("   "),
                password: "sufficient-password",
            },
            node_mode: "private",
            admin_secret: None,
        },
        None,
        identity::sessions::SessionContext {
            initiating: identity::sessions::InitiatingSession::Missing,
            fresh_id: &"ab".repeat(32),
            user_agent: None,
            client: None,
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        },
    )
    .await
    .unwrap();
    assert!(signed_in.user.email.is_none());
    for (username, email) in [("managed_one", ""), ("managed_two", "   ")] {
        let user = identity::accounts::create_managed_account(
            f.pool(),
            Some(&signed_in.session_id),
            identity::credentials::PasswordAccount {
                username,
                email: Some(email),
                password: "sufficient-password",
            },
            false,
        )
        .await
        .unwrap();
        assert!(user.email.is_none());
        assert_eq!(
            identity::profiles::update(
                f.pool(),
                user.id,
                "invalid-session",
                identity::profiles::ProfileUpdate {
                    display_name: Some("must not change"),
                    bio: None,
                    location: None,
                    website: None,
                }
            )
            .await
            .unwrap_err(),
            identity::profiles::ProfileError::Denied
        );
    }
    let actor = signed_in.user.id;
    assert!(
        db::node_config::set_ai_crawlers(f.pool(), actor, &signed_in.session_id, "allow")
            .await
            .unwrap()
    );
    db::cluster::register_own_node(
        f.pool(),
        &"a".repeat(64),
        "test-node",
        &["storage".into()],
        "test",
    )
    .await
    .unwrap();
    let authority = || db::cluster::Authority::Administrator {
        user_id: actor,
        session: &signed_in.session_id,
    };
    assert!(matches!(
        db::cluster::set_node_status(f.pool(), authority(), "test-node", "revoked")
            .await
            .unwrap(),
        db::models::NodeStatusChange::Changed { .. }
    ));
    db::users::sessions::delete_session(f.pool(), &signed_in.session_id)
        .await
        .unwrap();
    assert!(
        !db::node_config::set_ai_crawlers(f.pool(), actor, &signed_in.session_id, "block")
            .await
            .unwrap()
    );
    assert_eq!(
        db::cluster::set_node_status(f.pool(), authority(), "test-node", "active")
            .await
            .unwrap(),
        db::models::NodeStatusChange::Denied
    );
    f.execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[actor])
        .await;
    assert_eq!(
        identity::profiles::update(
            f.pool(),
            actor,
            &signed_in.session_id,
            identity::profiles::ProfileUpdate {
                display_name: Some("after suspension"),
                bio: None,
                location: None,
                website: None,
            }
        )
        .await
        .unwrap_err(),
        identity::profiles::ProfileError::Denied
    );
    assert!(db::users::accounts::get_user_by_id(f.pool(), actor)
        .await
        .unwrap()
        .unwrap()
        .display_name
        .is_none());
    f.finish().await;
}

#[tokio::test]
async fn postgres_search_omits_badges_when_native_query_budget_expires() {
    fake_webui_bundle::install();
    let mut f = Database::new(true).await;
    f.complete_setup().await;
    let actor = f.actor("search_budget_reader").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let client = Client::login(&router, f.pool(), actor).await;
    let raw = f.pg();
    let mut gate = raw.begin().await.unwrap();
    sqlx::query("LOCK TABLE issues IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *gate)
        .await
        .unwrap();
    let (status, page) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.model("/search?q=absent-token&type=repos"),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
    let Page::Discovery(codeyard::webui::pages::discovery::Page::Search(model)) = page else {
        panic!("search")
    };
    assert_eq!(model.counts_unavailable, vec!["issues"]);
    gate.rollback().await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn sqlite_administration_queries_and_pages_are_bounded() {
    bounded_administration(false).await;
}
#[tokio::test]
async fn postgres_administration_queries_and_pages_are_bounded() {
    bounded_administration(true).await;
}
#[tokio::test]
async fn sqlite_blank_emails_and_effect_time_account_authority() {
    account_writes(false).await;
}
#[tokio::test]
async fn postgres_blank_emails_and_effect_time_account_authority() {
    account_writes(true).await;
}
