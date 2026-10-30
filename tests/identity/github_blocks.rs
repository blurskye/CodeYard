//! Private blocking policy is enforced at the same database effects used by every transport.
use super::github::request;
use crate::support::{
    database::{wait_until_blocked, Database},
    http_client::Client,
};
use codeyard::backend::{
    db::{
        self,
        models::AccessLevel,
        repositories::creation::{NewRepository, RepositoryCreationOwner},
        users::accounts::AccountCredential,
    },
    identity::{
        self,
        tokens::{self, TokenInput},
    },
};
use serde_json::json;

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let carol = f.actor("carol").await;
    let dave = f.actor("dave").await;
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
    for (operation, method, path) in [
        (
            "users/list-blocked-by-authenticated-user",
            "GET",
            "/api/v3/user/blocks",
        ),
        ("users/check-blocked", "GET", "/api/v3/user/blocks/bob"),
        ("users/block", "PUT", "/api/v3/user/blocks/bob"),
        ("users/unblock", "DELETE", "/api/v3/user/blocks/bob"),
    ] {
        request(&router, operation, method, path, None, json!(null), 401).await;
    }
    for (operation, method) in [("users/block", "PUT"), ("users/unblock", "DELETE")] {
        request(
            &router,
            operation,
            method,
            "/api/v3/user/blocks/bob",
            Some(&credentials[1]),
            json!(null),
            403,
        )
        .await;
    }
    request(
        &router,
        "users/block",
        "PUT",
        "/api/v3/user/blocks/alice",
        write,
        json!(null),
        422,
    )
    .await;
    request(
        &router,
        "users/block",
        "PUT",
        "/api/v3/user/blocks/missing",
        write,
        json!(null),
        404,
    )
    .await;
    request(
        &router,
        "users/check-blocked",
        "GET",
        "/api/v3/user/blocks/bob",
        write,
        json!(null),
        404,
    )
    .await;
    let public = db::repositories::records::create_repository(
        f.pool(),
        alice,
        "project",
        None,
        false,
        "/fixture/alice/project",
    )
    .await
    .unwrap();
    let private = db::repositories::records::create_repository(
        f.pool(),
        alice,
        "private",
        None,
        true,
        "/fixture/alice/private",
    )
    .await
    .unwrap();
    let other_repo = db::repositories::records::create_repository(
        f.pool(),
        carol,
        "shared",
        None,
        false,
        "/fixture/carol/shared",
    )
    .await
    .unwrap();
    for repo in [&public, &private] {
        db::repositories::access::grant_repo_access(f.pool(), bob, repo.id, AccessLevel::Write)
            .await
            .unwrap();
    }
    let issue = db::issues::create_issue(f.pool(), public.id, alice, "Issue", "Body")
        .await
        .unwrap();
    db::assignees::assign_user(
        f.pool(),
        alice,
        public.id,
        db::assignees::AssignedItem::Issue(issue.id),
        bob,
    )
    .await
    .unwrap();
    for (a, b) in [(alice, bob), (bob, alice), (carol, alice)] {
        db::following::follow_user(f.pool(), a, b, &db::users::accounts::AccountCredential::Trusted).await.unwrap();
    }
    for actor in [bob, carol] {
        db::repositories::engagement::star_repo(f.pool(), actor, public.id)
            .await
            .unwrap();
        db::repositories::engagement::watch_repo(f.pool(), actor, public.id)
            .await
            .unwrap();
    }
    db::issues::add_issue_comment(f.pool(), issue.repo_id, issue.id, bob, "Before block", &codeyard::backend::db::users::accounts::AccountCredential::Trusted)
        .await
        .unwrap();
    db::notifications::create_notification(
        f.pool(),
        db::notifications::NewNotification {
            user_id: alice,
            actor_id: Some(bob),
            repo_id: Some(public.id),
            kind: "mention",
            title: "Before block",
            body: None,
            link: None,
            metadata: &json!({}),
        },
    )
    .await
    .unwrap();
    request(
        &router,
        "users/block",
        "PUT",
        "/api/v3/user/blocks/bob",
        write,
        json!(null),
        204,
    )
    .await;
    request(
        &router,
        "users/block",
        "PUT",
        "/api/v3/user/blocks/bob",
        write,
        json!(null),
        204,
    )
    .await;
    request(
        &router,
        "users/check-blocked",
        "GET",
        "/api/v3/user/blocks/bob",
        write,
        json!(null),
        204,
    )
    .await;
    assert!(!db::following::is_user_following(f.pool(), alice, bob)
        .await
        .unwrap());
    assert!(!db::following::is_user_following(f.pool(), bob, alice)
        .await
        .unwrap());
    assert!(db::following::is_user_following(f.pool(), carol, alice)
        .await
        .unwrap());
    assert!(
        !db::repositories::engagement::is_repo_starred(f.pool(), bob, public.id)
            .await
            .unwrap()
    );
    assert!(
        !db::repositories::engagement::is_repo_watched(f.pool(), bob, public.id)
            .await
            .unwrap()
    );
    assert_eq!(f.repository(public.id).await.stars_count, 1);
    assert_eq!(f.repository(public.id).await.watchers_count, 1);
    assert!(
        db::assignees::list_assignees(f.pool(), db::assignees::AssignedItem::Issue(issue.id))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db::repositories::access::get_user_repo_access(f.pool(), bob, private.id)
            .await
            .unwrap(),
        AccessLevel::None
    );
    assert_eq!(
        db::repositories::access::get_user_repo_access(f.pool(), bob, public.id)
            .await
            .unwrap(),
        AccessLevel::Read
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id=$1 AND actor_id=$2",
            &[alice, bob]
        )
        .await,
        0
    );
    request(
        &router,
        "users/follow",
        "PUT",
        "/api/v3/user/following/alice",
        Some(&credentials[2]),
        json!(null),
        403,
    )
    .await;
    request(
        &router,
        "activity/star-repo-for-authenticated-user",
        "PUT",
        "/api/v3/user/starred/alice/project",
        Some(&credentials[2]),
        json!(null),
        403,
    )
    .await;
    let target = db::reactions::Target {
        repo_id: public.id,
        item_id: issue.id,
        subject: db::reactions::Subject::Issue,
        subject_id: issue.id,
    };
    assert!(db::reactions::create_reaction(f.pool(), bob, &db::users::accounts::AccountCredential::Session(&other_session.session), target, "+1")
        .await
        .unwrap_err()
        .is::<db::discussions::DiscussionWriteRefusal>());
    let reply = other_session
        .form_reply(
            &format!("/alice/project/issues/{}/comments", issue.number),
            &[
                ("body", "Blocked comment"),
                ("repository_id", &public.id.to_string()),
                ("issue_id", &issue.id.to_string()),
            ],
        )
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::FORBIDDEN);
    assert!(
        db::issues::create_issue(f.pool(), public.id, bob, "Refused", "Body")
            .await
            .unwrap_err()
            .is::<db::discussions::DiscussionWriteRefusal>()
    );
    assert!(
        db::issues::add_issue_comment(f.pool(), issue.repo_id, issue.id, bob, "Refused", &codeyard::backend::db::users::accounts::AccountCredential::Trusted)
            .await
            .is_err()
    );
    assert!(
        db::repositories::engagement::watch_repo(f.pool(), bob, public.id)
            .await
            .unwrap_err()
            .is::<db::users::blocks::InteractionBlocked>()
    );
    assert!(db::repositories::engagement::set_watch_level(
        f.pool(),
        bob,
        public.id,
        "custom",
        "issues"
    )
    .await
    .is_err());
    assert!(db::repositories::access::grant_repo_access(
        f.pool(),
        bob,
        public.id,
        AccessLevel::Write
    )
    .await
    .is_err());
    let fork = db::repositories::creation::insert_repository(
        f.pool(),
        NewRepository {
            owner: RepositoryCreationOwner::personal(bob),
            name: "fork",
            description: None,
            is_private: false,
            disk_path: "/fixture/fork",
            storage_node_id: None,
            source_id: Some(public.id),
        },
    )
    .await;
    assert!(fork
        .unwrap_err()
        .is::<db::repositories::creation::RepositoryStorageRefusal>());
    let refused = db::notifications::create_notification(
        f.pool(),
        db::notifications::NewNotification {
            user_id: alice,
            actor_id: Some(bob),
            repo_id: Some(public.id),
            kind: "mention",
            title: "Refused",
            body: None,
            link: None,
            metadata: &json!({}),
        },
    )
    .await;
    assert!(refused
        .unwrap_err()
        .is::<db::notifications::records::NotificationSuppressed>());
    // Unrelated owners and organization scopes are not account-wide bans.
    let organization = db::organizations::profiles::NewOrganization::new(
        "shared-org",
        db::organizations::profiles::OrganizationProfile::new(None, None).unwrap(),
    )
    .unwrap();
    let organization =
        match db::organizations::profiles::create_organization(f.pool(), alice, &organization)
            .await
            .unwrap()
        {
            db::organizations::OrganizationOutcome::Applied(org) => org,
            other => panic!("{other:?}"),
        };
    let org_repo = db::repositories::creation::insert_repository(
        f.pool(),
        NewRepository {
            owner: RepositoryCreationOwner {
                actor_id: alice,
                target: db::repositories::creation::RepositoryTarget::Organization(organization.id),
            },
            name: "shared",
            description: None,
            is_private: false,
            disk_path: "/fixture/shared-org/shared",
            storage_node_id: None,
            source_id: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    db::issues::create_issue(
        f.pool(),
        org_repo.id,
        bob,
        "Organization contribution",
        "Body",
    )
    .await
    .unwrap();
    db::repositories::engagement::star_repo(f.pool(), bob, org_repo.id)
        .await
        .unwrap();
    db::issues::create_issue(f.pool(), other_repo.id, bob, "Allowed", "Body")
        .await
        .unwrap();
    db::repositories::engagement::star_repo(f.pool(), bob, other_repo.id)
        .await
        .unwrap();
    let (_, blocks) = request(
        &router,
        "users/list-blocked-by-authenticated-user",
        "GET",
        "/api/v3/user/blocks",
        Some(&credentials[2]),
        json!(null),
        200,
    )
    .await;
    assert_eq!(blocks, json!([]));
    request(
        &router,
        "users/block",
        "PUT",
        "/api/v3/user/blocks/dave",
        write,
        json!(null),
        204,
    )
    .await;
    let (headers, blocks) = request(
        &router,
        "users/list-blocked-by-authenticated-user",
        "GET",
        "/api/v3/user/blocks?per_page=1",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(blocks[0]["login"], "bob");
    assert!(headers["link"].to_str().unwrap().contains("page=2"));
    let (_, blocks) = request(
        &router,
        "users/list-blocked-by-authenticated-user",
        "GET",
        "/api/v3/user/blocks?per_page=1&page=2",
        write,
        json!(null),
        200,
    )
    .await;
    assert_eq!(blocks[0]["login"], "dave");
    request(
        &router,
        "users/unblock",
        "DELETE",
        "/api/v3/user/blocks/dave",
        write,
        json!(null),
        204,
    )
    .await;
    assert!(!db::users::blocks::is_blocked(f.pool(), alice, dave)
        .await
        .unwrap());
    f.execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[bob])
        .await;
    request(
        &router,
        "users/check-blocked",
        "GET",
        "/api/v3/user/blocks/bob",
        write,
        json!(null),
        404,
    )
    .await;
    f.execute("UPDATE users SET is_banned=FALSE WHERE id=$1", &[bob])
        .await;
    let page = session.page_ok("/settings?tab=blocked").await;
    assert!(page.contains("name=\"username\" value=\"bob\""));
    assert!(page.contains("/settings/blocks"));
    request(
        &router,
        "users/unblock",
        "DELETE",
        "/api/v3/user/blocks/bob",
        write,
        json!(null),
        204,
    )
    .await;
    request(
        &router,
        "users/check-blocked",
        "GET",
        "/api/v3/user/blocks/bob",
        write,
        json!(null),
        404,
    )
    .await;
    // Unblocking restores contribution eligibility, not deleted relationships or grants.
    assert!(!db::following::is_user_following(f.pool(), bob, alice)
        .await
        .unwrap());
    assert!(
        !db::repositories::engagement::is_repo_starred(f.pool(), bob, public.id)
            .await
            .unwrap()
    );
    assert_eq!(
        db::repositories::access::get_user_repo_access(f.pool(), bob, private.id)
            .await
            .unwrap(),
        AccessLevel::None
    );
    db::issues::add_issue_comment(f.pool(), issue.repo_id, issue.id, bob, "After unblock", &codeyard::backend::db::users::accounts::AccountCredential::Trusted)
        .await
        .unwrap();
    let reply = session
        .form_reply("/settings/blocks", &[("username", "bob")])
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::SEE_OTHER);
    request(
        &router,
        "users/check-blocked",
        "GET",
        "/api/v3/user/blocks/bob",
        write,
        json!(null),
        204,
    )
    .await;
    let reply = session
        .form_reply(
            "/settings/blocks",
            &[("username", "bob"), ("unblock", "yes")],
        )
        .await;
    assert_eq!(reply.status, axum::http::StatusCode::SEE_OTHER);
    assert!(!db::users::blocks::is_blocked(f.pool(), alice, bob)
        .await
        .unwrap());
    let error = identity::blocks::change(
        f.pool(),
        alice,
        &AccountCredential::Session("revoked"),
        "bob",
        true,
    )
    .await;
    assert_eq!(error, Err(identity::blocks::BlockError::Denied));
    if pg {
        // Every contribution effect waits behind policy changes, then reloads their outcome.
        let mut gate = f.pg().begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()),37002)")
            .execute(&mut *gate)
            .await
            .unwrap();
        sqlx::query("INSERT INTO user_blocks(user_id,blocked_id) VALUES($1,$2)")
            .bind(alice)
            .bind(bob)
            .execute(&mut *gate)
            .await
            .unwrap();
        let pool = f.pool().clone();
        let repo = public.id;
        let issue_writer = tokio::spawn(async move {
            db::issues::create_issue(&pool, repo, bob, "Waiting", "Body")
                .await
                .is_err()
        });
        let pool = f.pool().clone();
        let star_writer = tokio::spawn(async move {
            db::repositories::engagement::star_repo(&pool, bob, repo)
                .await
                .is_err()
        });
        let pool = f.pool().clone();
        let follower =
            tokio::spawn(
                async move { db::following::follow_user(&pool, bob, alice, &db::users::accounts::AccountCredential::Trusted).await.is_err() },
            );
        wait_until_blocked(f.pg(), pid, 3).await;
        gate.commit().await.unwrap();
        assert!(issue_writer.await.unwrap());
        assert!(star_writer.await.unwrap());
        assert!(follower.await.unwrap());
    }
    f.finish().await;
}

#[tokio::test]
async fn sqlite_github_blocks() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_github_blocks() {
    contract(true).await;
}
