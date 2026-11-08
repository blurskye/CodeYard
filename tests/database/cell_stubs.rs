//! Remote metadata keeps references without creating local accounts or credentials.
use crate::support::database::Database;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use codeyard_database::{cells::stubs, models::Organization, DbPool};
use codeyard_types::cell_ids::Prefix;
use serde_json::Value;
use tower::ServiceExt;

struct NoticeHome(std::sync::atomic::AtomicU8);
impl codeyard_database::cells::authorization::Home for NoticeHome {
    fn authorize<'a>(
        &'a self,
        _: &'a codeyard_database::cells::authorization::Proof,
    ) -> futures_util::future::BoxFuture<
        'a,
        anyhow::Result<Option<codeyard_database::cells::authorization::Answer>>,
    > {
        Box::pin(async { Ok(None) })
    }
    fn user<'a>(
        &'a self,
        _: &'a str,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<Option<codeyard_database::models::User>>>
    {
        Box::pin(async { Ok(None) })
    }
    fn notice_public(&self, _: i64) -> futures_util::future::BoxFuture<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            match self.0.load(std::sync::atomic::Ordering::SeqCst) {
                0 => Ok(false),
                1 => Ok(true),
                _ => anyhow::bail!("Home is unreachable"),
            }
        })
    }
}

async fn credentials(pool: &DbPool, user: i64, session: &str) -> bool {
    let expiry = chrono::Utc::now() + chrono::Duration::hours(1);
    match pool {
        DbPool::Postgres(pool) => {
            sqlx::query("INSERT INTO sessions(id,user_id,expires_at) VALUES($1,$2,$3)")
                .bind(session)
                .bind(user)
                .bind(expiry)
                .execute(pool)
                .await
                .is_ok()
        }
        DbPool::Sqlite(pool) => {
            sqlx::query("INSERT INTO sessions(id,user_id,expires_at) VALUES($1,$2,$3)")
                .bind(session)
                .bind(user)
                .bind(expiry.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
                .execute(pool)
                .await
                .is_ok()
        }
    }
}
async fn mutate(pool: &DbPool, statement: &str, id: i64) -> bool {
    match pool {
        DbPool::Postgres(pool) => sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(id)
            .execute(pool)
            .await
            .is_ok(),
        DbPool::Sqlite(pool) => sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(id)
            .execute(pool)
            .await
            .is_ok(),
    }
}
async fn contract(pg: bool) {
    let mut database = Database::empty(pg).await;
    database.complete_setup().await;
    let pool = database.pool();
    let native = codeyard_db_identity::users::accounts::create_user_with_password(
        pool,
        "native",
        Some("local@example.test"),
        "local-password-hash",
    )
    .await
    .unwrap();
    let before = codeyard_db_identity::users::accounts::count_users(pool)
        .await
        .unwrap();
    let source = Prefix::new(1);
    let mut remote = native.clone();
    remote.id = *source.ids().start();
    remote.username = "remote".into();
    remote.home_cell = 1;
    remote.email = Some("private@example.test".into());
    remote.password_hash = Some("secret-hash".into());
    remote.avatar_file_id = Some(uuid::Uuid::new_v4());
    remote.is_admin = true;
    remote.two_factor_enabled = true;
    remote.display_name = Some("Remote person".into());
    assert!(stubs::user(pool, source, &remote).await.unwrap());
    let stub = codeyard_db_identity::users::accounts::get_user_by_id(pool, remote.id)
        .await
        .unwrap()
        .unwrap();
    assert!(stub.is_stub);
    assert_eq!(stub.home_cell, 1);
    assert_eq!(stub.display_name, remote.display_name);
    assert!(stub.email.is_none() && stub.password_hash.is_none() && stub.avatar_file_id.is_none());
    assert!(!stub.is_admin && stub.two_factor_enabled);
    assert_eq!(
        codeyard_db_identity::users::accounts::count_users(pool)
            .await
            .unwrap(),
        before
    );
    assert!(!codeyard_db_identity::users::accounts::list_users(pool)
        .await
        .unwrap()
        .iter()
        .any(|user| user.id == stub.id));
    let mut update = remote.clone();
    update.updated_at += chrono::Duration::seconds(1);
    update.display_name = Some("Newer metadata".into());
    assert!(stubs::user(pool, source, &update).await.unwrap());
    assert!(!stubs::user(pool, source, &remote).await.unwrap());
    let current = codeyard_db_identity::users::accounts::get_user_by_id(pool, remote.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.display_name, update.display_name);
    update.website = Some("javascript:alert(1)".into());
    assert!(stubs::user(pool, source, &update).await.is_err());
    let mut forged = remote.clone();
    forged.id = native.id;
    forged.username = "replacement".into();
    assert!(!stubs::user(pool, source, &forged).await.unwrap());
    assert_eq!(
        codeyard_db_identity::users::accounts::get_user_by_id(pool, native.id)
            .await
            .unwrap()
            .unwrap()
            .username,
        native.username
    );
    assert!(stubs::user(pool, Prefix::ZERO, &remote).await.is_err());
    assert!(!credentials(pool, remote.id, &format!("1.{}", "a".repeat(64))).await);
    assert!(credentials(pool, native.id, &"b".repeat(64)).await);
    assert!(
        !mutate(
            pool,
            &format!(
                "UPDATE sessions SET user_id=$1 WHERE id='{}'",
                "b".repeat(64)
            ),
            remote.id
        )
        .await
    );
    assert_eq!(
        database
            .scalar(
                "SELECT COUNT(*) FROM sessions WHERE user_id=$1",
                &[native.id]
            )
            .await,
        1
    );
    let convertible = codeyard_db_identity::users::accounts::create_user(pool, "convertible", None)
        .await
        .unwrap();
    assert!(credentials(pool, convertible.id, &"c".repeat(64)).await);
    assert!(
        !mutate(
            pool,
            "UPDATE users SET is_stub=TRUE,home_cell=1 WHERE id=$1",
            convertible.id
        )
        .await
    );
    assert!(
        mutate(
            pool,
            "DELETE FROM sessions WHERE user_id=$1",
            convertible.id
        )
        .await
    );
    assert!(
        mutate(
            pool,
            "UPDATE users SET is_stub=TRUE,home_cell=1 WHERE id=$1",
            convertible.id
        )
        .await
    );

    let now = chrono::Utc::now();
    let organization = Organization {
        id: remote.id,
        home_cell: 1,
        is_stub: false,
        name: "remote-org".into(),
        display_name: Some("Remote org".into()),
        description: None,
        avatar_file_id: Some(uuid::Uuid::new_v4()),
        billing_email: Some("billing@example.test".into()),
        is_verified: true,
        created_at: now,
        updated_at: now,
    };
    assert!(stubs::organization(pool, source, &organization)
        .await
        .unwrap());
    assert!(
        mutate(
            pool,
            &format!(
                "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,{},'owner')",
                native.id
            ),
            organization.id
        )
        .await
    );
    assert!(
        codeyard_db_orgs::organizations::discovery::list_organizations(
            pool, None, false, 0, 0, 100
        )
        .await
        .unwrap()
        .iter()
        .all(|org| org.id != organization.id)
    );

    // References to remote members remain intact, but account listings and their counts agree.
    use crate::support::organizations::applied;
    use codeyard_db_orgs::organizations::{membership, profiles, teams, OrganizationPage};
    let listed = applied(
        profiles::list_actor_organizations(pool, native.id, OrganizationPage::new(0, 100).unwrap())
            .await
            .unwrap(),
    );
    assert!(
        listed.iter().all(|org| org.id != organization.id),
        "Private native indexes exclude foreign organization stubs"
    );
    let choices = applied(
        profiles::list_repository_creation_organizations(
            pool,
            native.id,
            OrganizationPage::new(0, 100).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(choices.iter().all(|org| org.id != organization.id));
    let org = applied(
        profiles::create_organization(
            pool,
            native.id,
            &profiles::NewOrganization::new(
                "local-org",
                profiles::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    database.execute(
        "INSERT INTO org_memberships(org_id,user_id,role,is_public) VALUES($1,$2,'member',TRUE)",
        &[org.id, remote.id],
    ).await;
    database
        .execute(
            "UPDATE org_memberships SET is_public=TRUE WHERE org_id=$1",
            &[org.id],
        )
        .await;
    database.execute(
        "INSERT INTO teams(org_id,name,slug,privacy) VALUES($1,'Contributors','contributors','visible')",
        &[org.id],
    ).await;
    let team = database
        .scalar("SELECT id FROM teams WHERE org_id=$1", &[org.id])
        .await;
    for user in [native.id, remote.id] {
        database
            .execute(
                "INSERT INTO team_memberships(team_id,user_id,role) VALUES($1,$2,'member')",
                &[team, user],
            )
            .await;
    }
    let page = OrganizationPage::new(0, 100).unwrap();
    assert_eq!(
        applied(
            membership::count_organization_members(pool, native.id, org.id)
                .await
                .unwrap()
        ),
        1
    );
    let members = applied(
        membership::list_organization_members(pool, native.id, org.id, page)
            .await
            .unwrap(),
    );
    assert_eq!(
        members
            .iter()
            .map(|member| member.user_id)
            .collect::<Vec<_>>(),
        vec![native.id]
    );
    let members = applied(
        teams::list_organization_team_members(pool, native.id, org.id, team, page)
            .await
            .unwrap(),
    );
    assert_eq!(
        members
            .iter()
            .map(|member| member.user_id)
            .collect::<Vec<_>>(),
        vec![native.id]
    );
    let listing = applied(
        teams::list_organization_teams(pool, native.id, org.id, page)
            .await
            .unwrap(),
    );
    assert_eq!(listing[0].member_count, 1);
    assert_eq!(
        teams::get_public_team(pool, native.id, org.id, "contributors")
            .await
            .unwrap()
            .unwrap()
            .member_count,
        1
    );
    assert_eq!(
        database
            .scalar(
                "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1",
                &[org.id]
            )
            .await,
        2
    );
    let token = codeyard_types::cell_ids::Prefix::ZERO
        .token(codeyard_types::tokens::PAT_PREFIX, &"d".repeat(64))
        .unwrap();
    codeyard_db_identity::users::tokens::create_token(
        pool,
        native.id,
        &"b".repeat(64),
        "members",
        &codeyard_security::token_hash::token_hash(&token),
        "write",
        None,
    )
    .await
    .unwrap()
    .unwrap();

    let root = tempfile::tempdir().unwrap();
    let git_path = root.path().join("actual.git");
    let git = git2::Repository::init_bare(&git_path).unwrap();
    let original_head = git
        .find_reference("HEAD")
        .unwrap()
        .symbolic_target()
        .unwrap()
        .unwrap()
        .to_owned();
    let local = codeyard_db_repos::repositories::records::create_repository(
        pool,
        native.id,
        "local",
        None,
        false,
        git_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let mut metadata = local.clone();
    metadata.id = remote.id + 1;
    metadata.name = "remote.repo".into();
    metadata.home_cell = 1;
    metadata.owner_user_id = Some(remote.id);
    assert!(stubs::repository(pool, source, &metadata).await.unwrap());
    let repository = database.repository(metadata.id).await;
    assert!(
        repository.is_stub
            && repository.disk_path.is_empty()
            && repository.storage_node_id.is_none()
    );
    assert!(
        !codeyard_identity::repository_access::can_read(pool, None, &repository)
            .await
            .unwrap()
    );
    // Collaborator projections evaluate the real stub marker, including the admin case.
    let session = "b".repeat(64);
    let credential =
        codeyard_database::queries::users::accounts::AccountCredential::Session(&session);
    assert!(
        codeyard_db_actions::variables::list(pool, native.id, &credential, local.id)
            .await
            .unwrap()
            .is_empty()
    );
    for admin in [false, true] {
        assert!(
            mutate(
                pool,
                &format!("UPDATE users SET is_admin={admin} WHERE id=$1"),
                native.id
            )
            .await
        );
        assert!(matches!(
            codeyard_db_actions::variables::list(pool, native.id, &credential, repository.id).await,
            Err(codeyard_db_actions::variables::VariableError::Denied)
        ));
    }
    assert!(
        mutate(
            pool,
            "UPDATE users SET is_admin=FALSE WHERE id=$1",
            native.id
        )
        .await
    );
    let mut hostile_path = repository.clone();
    hostile_path.disk_path = git_path.to_str().unwrap().into();
    let store = codeyard_git_storage::repositories::store::repo_store(&hostile_path, None);
    assert!(store.info().await.is_err());
    assert!(store
        .set_default_branch_as(pool, repository.id, native.id, "changed")
        .await
        .is_err());
    assert_eq!(
        git.find_reference("HEAD")
            .unwrap()
            .symbolic_target()
            .unwrap(),
        Some(original_head.as_str())
    );
    metadata.id += 1;
    metadata.owner_user_id = Some(remote.id + 100);
    assert!(stubs::repository(pool, source, &metadata).await.is_err());
    assert_eq!(
        database
            .scalar(
                "SELECT COUNT(*) FROM repositories WHERE id=$1",
                &[metadata.id]
            )
            .await,
        0
    );

    let router = database.router(root.path()).await;
    for path in [
        "/api/v3/users?per_page=100",
        "/api/v3/search/users?q=remote",
        "/api/v3/search/repositories?q=remote.repo",
    ] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1048576).await.unwrap())
                .unwrap();
        assert!(
            !body.to_string().contains("Remote person")
                && !body.to_string().contains("remote.repo"),
            "{path}: {body}"
        );
        let items = body
            .get("items")
            .and_then(Value::as_array)
            .or_else(|| body.as_array())
            .unwrap();
        assert!(
            items
                .iter()
                .all(|item| item["id"].as_i64() != Some(remote.id)),
            "{path}: {body}"
        );
        if path.contains("/search/") {
            assert_eq!(body["total_count"], 0, "{path}: {body}");
        }
    }
    for (path, field, count) in [
        ("/api/v3/orgs/local-org/members", "login", false),
        ("/api/v3/orgs/local-org/public_members", "login", false),
        (
            "/api/v3/orgs/local-org/teams/contributors/members",
            "login",
            false,
        ),
        (
            "/api/v3/orgs/local-org/teams/contributors",
            "members_count",
            true,
        ),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1048576).await.unwrap())
                .unwrap();
        if count {
            assert_eq!(body[field], 1, "{path}: {body}");
        } else {
            let members = body.as_array().unwrap();
            assert_eq!(members.len(), 1, "{path}: {body}");
            assert_eq!(members[0][field], "native", "{path}: {body}");
        }
    }
    // The same stale stub must not authorize either engine's inbox, badges or emails.
    let home = std::sync::Arc::new(NoticeHome(std::sync::atomic::AtomicU8::new(1)));
    pool.set_foreign_authorization(home.clone());
    let notice = codeyard_db_repos::notifications::records::create_notification(
        pool,
        codeyard_database::queries::notifications::records::NewNotification {
            user_id: native.id,
            actor_id: None,
            repo_id: Some(repository.id),
            kind: "mention",
            title: "Remote notice",
            body: Some("Private after delivery"),
            link: None,
            metadata: &serde_json::json!({"number":1,"subject":"Remote notice"}),
        },
    )
    .await
    .unwrap();
    for state in [1, 0, 2] {
        home.0.store(state, std::sync::atomic::Ordering::SeqCst);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v3/notifications")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1048576).await.unwrap())
                .unwrap();
        assert_eq!(
            body.as_array().unwrap().len(),
            usize::from(state == 1),
            "{body}"
        );
        assert_eq!(
            codeyard_db_repos::notifications::records::count_header_unread(pool, native.id)
                .await
                .unwrap(),
            i64::from(state == 1)
        );
        assert_eq!(
            codeyard_db_repos::notifications::records::notification_email_allowed(pool, notice.id)
                .await
                .unwrap(),
            state == 1
        );
        assert_eq!(
            codeyard_db_repos::notifications::records::get_notification(pool, native.id, notice.id)
                .await
                .unwrap()
                .is_some(),
            state == 1
        );
    }
    database.finish().await;
}
#[tokio::test]
async fn sqlite_remote_stubs_are_metadata_only() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_remote_stubs_are_metadata_only() {
    contract(true).await;
}
