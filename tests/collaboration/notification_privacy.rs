//! Notification privacy on disposable PostgreSQL and SQLite databases: what each
//! user can list, acknowledge and receive, and the original-session inbox.
//! Setup failure fails the test; nothing is skipped or mocked. Focused relationship
//! tests also exercise their JSON boundary; WebSocket delivery is not covered.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
mod contention;
mod delivery;
mod fixture;
mod provenance;
use crate::support::http_client as pulls;
mod session_inbox;

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db::{self, models::repositories::AccessLevel, organizations},
    notifications::NotificationEvent,
    storage::ObjectStore,
};
use fixture::Fixture;
use pulls::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn ids(rows: &[db::models::Notification]) -> Vec<i64> {
    rows.iter().map(|n| n.id).collect()
}

// Exercise all four unchanged interfaces at every authority transition. Stored
// hidden read_at TEXT and payloads must remain byte-identical, including already
// read hidden history. Fixture-only resets allow the next transition to recheck.
async fn agrees(f: &Fixture, recipient: i64, visible: &[i64]) {
    let pool = f.pool();
    let before = f.history(recipient).await;
    let mut expected = visible.to_vec();
    expected.sort_unstable_by(|a, b| b.cmp(a));
    let unread: Vec<i64> = before
        .iter()
        .filter(|r| expected.contains(&r["id"].as_i64().unwrap()) && r["read_at"].is_null())
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    let all = db::notifications::records::list_notifications_for_user(pool, recipient, 100, false)
        .await
        .unwrap();
    assert_eq!(ids(&all), expected);
    // Positive payloads agree with storage; no hidden title/body/link/metadata
    // can enter a projection, not merely a redacted title or omitted list row.
    for notification in &all {
        assert_eq!(notification.user_id, recipient);
        let stored = before.iter().find(|r| r["id"] == notification.id).unwrap();
        assert_eq!(notification.title, stored["title"].as_str().unwrap());
        assert_eq!(notification.body.as_deref(), stored["body"].as_str());
        assert_eq!(notification.link.as_deref(), stored["link"].as_str());
        assert_eq!(
            notification.metadata,
            serde_json::from_str::<Value>(stored["metadata"].as_str().unwrap()).unwrap()
        );
    }
    assert_eq!(
        ids(
            &db::notifications::records::list_notifications_for_user(pool, recipient, 100, true)
                .await
                .unwrap()
        ),
        unread
    );
    assert_eq!(
        db::notifications::records::count_unread_notifications(pool, recipient)
            .await
            .unwrap(),
        unread.len() as i64
    );
    let hidden: Vec<Value> = before
        .iter()
        .filter(|r| !expected.contains(&r["id"].as_i64().unwrap()))
        .cloned()
        .collect();
    for row in &hidden {
        assert!(db::notifications::records::mark_notification_read(
            pool,
            recipient,
            row["id"].as_i64().unwrap()
        )
        .await
        .unwrap()
        .is_none());
    }
    assert!(
        db::notifications::records::mark_notification_read(pool, recipient, -1)
            .await
            .unwrap()
            .is_none()
    );
    if let Some(id) = unread.first() {
        let acknowledged = db::notifications::records::mark_notification_read(pool, recipient, *id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(acknowledged.id, *id);
        assert_eq!(acknowledged.user_id, recipient);
        assert!(acknowledged.read_at.is_some());
        let again = db::notifications::records::mark_notification_read(pool, recipient, *id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.read_at, acknowledged.read_at, "one-ack is idempotent");
    }
    let remaining = unread.len().saturating_sub(1);
    assert_eq!(
        db::notifications::records::count_unread_notifications(pool, recipient)
            .await
            .unwrap(),
        remaining as i64
    );
    assert_eq!(
        db::notifications::records::mark_all_notifications_read(pool, recipient)
            .await
            .unwrap(),
        remaining as u64
    );
    assert_eq!(
        db::notifications::records::mark_all_notifications_read(pool, recipient)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        db::notifications::records::count_unread_notifications(pool, recipient)
            .await
            .unwrap(),
        0
    );
    assert!(
        db::notifications::records::list_notifications_for_user(pool, recipient, 100, true)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ids(
            &db::notifications::records::list_notifications_for_user(pool, recipient, 100, false)
                .await
                .unwrap()
        ),
        expected
    );
    let after = f.history(recipient).await;
    assert_eq!(
        after
            .iter()
            .filter(|r| !expected.contains(&r["id"].as_i64().unwrap()))
            .cloned()
            .collect::<Vec<_>>(),
        hidden
    );
    for (old, new) in before.iter().zip(&after) {
        let mut old = old.clone();
        let mut new = new.clone();
        old.as_object_mut().unwrap().remove("read_at");
        new.as_object_mut().unwrap().remove("read_at");
        assert_eq!(old, new, "ack must never rewrite the stored payload");
    }
    for id in unread {
        f.execute(
            "UPDATE notifications SET read_at=NULL WHERE id=$1 AND user_id=$2",
            &[id, recipient],
        )
        .await;
    }
}

async fn authority_contract(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("notification_owner").await;
    let recipient = f.actor("notification_recipient").await;
    let site = f.actor("notification_site").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[site])
        .await;
    let org = f
        .scalar(
            "INSERT INTO organizations(name) VALUES('notification_org') RETURNING id",
            &[],
        )
        .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
        &[org, owner],
    )
    .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'member')",
        &[org, recipient],
    )
    .await;
    let personal = f.repo(owner, None, "notification_personal").await;
    let tenant = f.repo(owner, Some(org), "notification_tenant").await;
    let account = f.notify(recipient, None, "owned-account").await;
    let direct = f.notify(recipient, Some(personal), "private-direct").await;
    let team_notice = f.notify(recipient, Some(tenant), "private-team").await;
    agrees(&f, recipient, &[account]).await;
    db::repositories::access::grant_repo_access(f.pool(), recipient, personal, AccessLevel::Read)
        .await
        .unwrap();
    agrees(&f, recipient, &[account, direct]).await;
    assert!(
        db::repositories::access::revoke_repo_access(f.pool(), recipient, personal)
            .await
            .unwrap()
    );
    agrees(&f, recipient, &[account]).await;
    let team = f.scalar("INSERT INTO teams(org_id,name,slug,privacy) VALUES($1,'Notification team','notification-team','secret') RETURNING id", &[org]).await;
    f.execute(
        "INSERT INTO team_memberships(team_id,user_id) VALUES($1,$2)",
        &[team, recipient],
    )
    .await;
    f.execute(
        "INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES($1,$2,1)",
        &[team, tenant],
    )
    .await;
    agrees(&f, recipient, &[account, team_notice]).await;
    for role in ["unknown", "owner"] {
        f.execute(
            &format!("UPDATE team_memberships SET role='{role}' WHERE team_id=$1 AND user_id=$2"),
            &[team, recipient],
        )
        .await;
        agrees(&f, recipient, &[account]).await;
    }
    f.execute(
        "UPDATE team_memberships SET role='maintainer' WHERE team_id=$1 AND user_id=$2",
        &[team, recipient],
    )
    .await;
    agrees(&f, recipient, &[account, team_notice]).await;
    assert!(matches!(
        organizations::revoke_organization_team_repository(f.pool(), owner, org, team, tenant)
            .await
            .unwrap(),
        organizations::OrganizationOutcome::Applied(_)
    ));
    agrees(&f, recipient, &[account]).await;
    f.execute(
        "INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES($1,$2,1)",
        &[team, tenant],
    )
    .await;
    assert!(matches!(
        organizations::remove_organization_team_member(f.pool(), owner, org, team, recipient)
            .await
            .unwrap(),
        organizations::OrganizationOutcome::Applied(_)
    ));
    agrees(&f, recipient, &[account]).await;
    f.execute(
        "INSERT INTO team_memberships(team_id,user_id) VALUES($1,$2)",
        &[team, recipient],
    )
    .await;
    f.execute(
        "UPDATE org_memberships SET role='unknown' WHERE org_id=$1 AND user_id=$2",
        &[org, recipient],
    )
    .await;
    agrees(&f, recipient, &[account]).await;
    f.execute(
        "UPDATE org_memberships SET role='member' WHERE org_id=$1 AND user_id=$2",
        &[org, recipient],
    )
    .await;
    db::repositories::access::grant_repo_access(f.pool(), recipient, tenant, AccessLevel::Read)
        .await
        .unwrap();
    // Real member removal clears direct and team contributions, not notifications.
    assert!(matches!(
        organizations::remove_organization_member(f.pool(), owner, org, recipient)
            .await
            .unwrap(),
        organizations::OrganizationOutcome::Applied(_)
    ));
    agrees(&f, recipient, &[account]).await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'admin')",
        &[org, recipient],
    )
    .await;
    agrees(&f, recipient, &[account, team_notice]).await;
    for role in ["owner", "member", "unknown"] {
        f.execute(
            &format!("UPDATE org_memberships SET role='{role}' WHERE org_id=$1 AND user_id=$2"),
            &[org, recipient],
        )
        .await;
        if role == "owner" {
            agrees(&f, recipient, &[account, team_notice]).await;
        } else {
            agrees(&f, recipient, &[account]).await;
        }
    }
    f.execute(
        "DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2",
        &[org, recipient],
    )
    .await;
    db::repositories::access::grant_repo_access(f.pool(), recipient, tenant, AccessLevel::Admin)
        .await
        .unwrap();
    for restriction in ["is_banned", "must_change_password"] {
        f.execute(
            &format!("UPDATE users SET {restriction}=TRUE WHERE id=$1"),
            &[owner],
        )
        .await;
        assert_eq!(
            db::repositories::access::get_user_repo_access(f.pool(), recipient, tenant)
                .await
                .unwrap(),
            AccessLevel::Read
        );
        agrees(&f, recipient, &[account, team_notice]).await;
        f.execute(
            &format!("UPDATE users SET {restriction}=FALSE WHERE id=$1"),
            &[owner],
        )
        .await;
    }
    f.execute(
        "UPDATE org_memberships SET role='unknown' WHERE org_id=$1 AND user_id=$2",
        &[org, owner],
    )
    .await;
    agrees(&f, recipient, &[account, team_notice]).await;
    f.execute(
        "UPDATE repositories SET is_archived=TRUE WHERE id=$1",
        &[tenant],
    )
    .await;
    agrees(&f, recipient, &[account, team_notice]).await;
    db::repositories::access::revoke_repo_access(f.pool(), recipient, tenant)
        .await
        .unwrap();
    agrees(&f, recipient, &[account]).await;
    f.execute(
        "UPDATE repositories SET is_private=FALSE WHERE id=$1",
        &[tenant],
    )
    .await;
    agrees(&f, recipient, &[account, team_notice]).await;
    // Admin bypass is personal-only, never tenant numeric ownership/recovery.
    let site_personal = f.notify(site, Some(personal), "site-personal").await;
    let site_tenant = f.notify(site, Some(tenant), "site-public-tenant").await;
    agrees(&f, site, &[site_personal, site_tenant]).await;
    f.execute(
        "UPDATE repositories SET is_private=TRUE WHERE id=$1",
        &[tenant],
    )
    .await;
    agrees(&f, site, &[site_personal]).await;
    // Foreign caller, even a platform admin, cannot ack recipient history.
    let before = f.history(recipient).await;
    assert!(
        db::notifications::records::mark_notification_read(f.pool(), site, account)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db::notifications::records::mark_notification_read(f.pool(), site, direct)
            .await
            .unwrap()
            .is_none()
    );
    db::notifications::records::mark_all_notifications_read(f.pool(), site)
        .await
        .unwrap();
    assert_eq!(f.history(recipient).await, before);
    for restriction in ["is_banned", "must_change_password"] {
        f.execute(
            &format!("UPDATE users SET {restriction}=TRUE WHERE id=$1"),
            &[recipient],
        )
        .await;
        agrees(&f, recipient, &[]).await;
        f.execute(
            &format!("UPDATE users SET {restriction}=FALSE WHERE id=$1"),
            &[recipient],
        )
        .await;
    }
    agrees(&f, -1, &[]).await;
    for restriction in ["is_banned", "must_change_password"] {
        f.execute(
            &format!("UPDATE users SET {restriction}=TRUE WHERE id=$1"),
            &[site],
        )
        .await;
        agrees(&f, site, &[]).await;
        f.execute(
            &format!("UPDATE users SET {restriction}=FALSE WHERE id=$1"),
            &[site],
        )
        .await;
    }
    // Exact collision does not confer organization ownership or admin Read.
    f.execute(
        "INSERT INTO users(id,username) VALUES(700000,'notification_collision_user')",
        &[],
    )
    .await;
    f.execute(
        "INSERT INTO organizations(id,name) VALUES(700000,'notification_collision_org')",
        &[],
    )
    .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES(700000,$1,'owner')",
        &[owner],
    )
    .await;
    let collision = f.repo(owner, Some(700000), "notification_collision").await;
    f.notify(700000, Some(collision), "collision-private").await;
    agrees(&f, 700000, &[]).await;
    f.finish().await;
}

async fn bounded_projection(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("page_owner").await;
    let recipient = f.actor("page_recipient").await;
    let repo = f.repo(owner, None, "page_private").await;
    let mut visible = Vec::new();
    for _ in 0..115 {
        visible.push(f.notify(recipient, None, "visible-account").await);
    }
    for _ in 0..125 {
        f.notify(recipient, Some(repo), "HIDDEN_PAYLOAD_SENTINEL")
            .await;
    }
    let stored = f.history(recipient).await;
    for limit in [i64::MIN, -1, 0, 1, 2, 100, 101, i64::MAX] {
        for unread_only in [true, false] {
            let rows = db::notifications::records::list_notifications_for_user(
                f.pool(),
                recipient,
                limit,
                unread_only,
            )
            .await
            .unwrap();
            assert_eq!(
                ids(&rows),
                visible
                    .iter()
                    .rev()
                    .take(limit.clamp(1, 100) as usize)
                    .copied()
                    .collect::<Vec<_>>()
            );
            assert!(!serde_json::to_string(&rows)
                .unwrap()
                .contains("HIDDEN_PAYLOAD_SENTINEL"));
        }
    }
    f.execute(
        "UPDATE notifications SET created_at='2026-01-02T00:00:00.000000Z' WHERE id=$1",
        &[visible[0]],
    )
    .await;
    for unread_only in [true, false] {
        assert_eq!(
            ids(&db::notifications::records::list_notifications_for_user(
                f.pool(),
                recipient,
                2,
                unread_only
            )
            .await
            .unwrap()),
            vec![visible[0], *visible.last().unwrap()],
            "created_at takes precedence over the id tie breaker"
        );
    }
    assert_eq!(
        db::notifications::records::count_unread_notifications(f.pool(), recipient)
            .await
            .unwrap(),
        115
    );
    assert!(db::notifications::records::mark_notification_read(
        f.pool(),
        recipient,
        stored[0]["id"].as_i64().unwrap()
    )
    .await
    .unwrap()
    .is_none());
    assert!(
        db::notifications::records::mark_notification_read(f.pool(), recipient, visible[0])
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        db::notifications::records::mark_all_notifications_read(f.pool(), recipient)
            .await
            .unwrap(),
        114
    );
    assert_eq!(
        db::notifications::records::count_unread_notifications(f.pool(), recipient)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        f.history(recipient)
            .await
            .into_iter()
            .filter(|r| r["repo"] == repo)
            .collect::<Vec<_>>(),
        stored
            .into_iter()
            .filter(|r| r["repo"] == repo)
            .collect::<Vec<_>>()
    );
    f.finish().await;
}

async fn corrupt_row_privacy(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("history_owner").await;
    let recipient = f.actor("history_recipient").await;
    let account = f.notify(recipient, None, "history-account").await;
    let source = f.repo(owner, None, "history_source").await;
    f.execute(
        "UPDATE repositories SET is_private=FALSE WHERE id=$1",
        &[source],
    )
    .await;
    let fork = db::repositories::discovery::create_forked_repository(
        f.pool(),
        db::repositories::discovery::NewForkedRepository {
            owner_id: owner,
            name: "history_fork",
            description: None,
            is_private: false,
            disk_path: "/owned/not-executed-fork.git",
            forked_from: source,
            default_branch: "main",
        },
    )
    .await
    .unwrap()
    .id;
    let fork_notice = f.notify(recipient, Some(fork), "fork-payload").await;
    agrees(&f, recipient, &[account, fork_notice]).await;
    // Native persisted ancestry floor narrows a public fork with its source.
    f.execute(
        "UPDATE repositories SET is_private=TRUE WHERE id=$1",
        &[source],
    )
    .await;
    agrees(&f, recipient, &[account]).await;
    db::repositories::access::grant_repo_access(f.pool(), recipient, fork, AccessLevel::Read)
        .await
        .unwrap();
    agrees(&f, recipient, &[account, fork_notice]).await;
    // Already-read hidden rows retain their exact stored read_at bytes.
    f.execute(
        "UPDATE notifications SET read_at='2020-02-03T04:05:06.123456Z' WHERE id=$1",
        &[fork_notice],
    )
    .await;
    db::repositories::access::revoke_repo_access(f.pool(), recipient, fork)
        .await
        .unwrap();
    agrees(&f, recipient, &[account]).await;
    let malformed = f.repo(owner, None, "history_malformed").await;
    f.notify(recipient, Some(malformed), "malformed-private")
        .await;
    db::repositories::access::grant_repo_access(f.pool(), recipient, malformed, AccessLevel::Read)
        .await
        .unwrap();
    // Owned fixture SQL alone admits invalid ownership and dangling IDs.
    f.corrupt(
        "UPDATE repositories SET owner_kind='unknown' WHERE id=$1",
        &[malformed],
    )
    .await;
    agrees(&f, recipient, &[account]).await;
    f.execute(
        "UPDATE repositories SET is_private=FALSE WHERE id=$1",
        &[malformed],
    )
    .await;
    agrees(&f, recipient, &[account]).await;
    f.corrupt(
        "UPDATE repositories SET owner_kind='user',owner_org_id=700000 WHERE id=$1",
        &[malformed],
    )
    .await;
    agrees(&f, recipient, &[account]).await;
    f.corrupt(
        "UPDATE repositories SET owner_org_id=NULL,owner_user_id=900000 WHERE id=$1",
        &[malformed],
    )
    .await;
    agrees(&f, recipient, &[account]).await;
    f.corrupt("UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=900000 WHERE id=$1", &[malformed]).await;
    agrees(&f, recipient, &[account]).await;
    let missing_source = f.repo(owner, None, "history_missing_source").await;
    let missing = f
        .notify(recipient, Some(missing_source), "missing-repo-private")
        .await;
    f.corrupt("DELETE FROM repositories WHERE id=$1", &[missing_source])
        .await;
    // A corrupt current FK never changes the missing original repository scope.
    f.corrupt(
        "UPDATE notifications SET repo_id=900000 WHERE id=$1",
        &[missing],
    )
    .await;
    agrees(&f, recipient, &[account]).await;
    // Even the personal admin override must fail closed on malformed owners.
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[recipient])
        .await;
    f.corrupt("UPDATE repositories SET owner_kind='user',owner_org_id=NULL,owner_user_id=900000 WHERE id=$1", &[malformed]).await;
    // A valid personal fork remains admin-readable; hidden corrupt rows do not.
    agrees(&f, recipient, &[account, fork_notice]).await;
    f.finish().await;
}

async fn assert_created_event(
    pool: &db::DbPool,
    hint: NotificationEvent,
    user_id: i64,
    kind: &str,
) {
    let session = session_inbox::session(pool, user_id).await;
    let event = codeyard::backend::notifications::current_event(pool, &session, user_id, &hint)
        .await
        .unwrap()
        .unwrap();
    match event {
        NotificationEvent::Created {
            user_id: actual_user,
            kind: actual_kind,
            ..
        } => {
            assert_eq!(actual_user, user_id);
            assert_eq!(actual_kind, kind);
        }
        other => panic!("expected created notification, got {other:?}"),
    }
}

fn assert_no_event(events: &mut tokio::sync::broadcast::Receiver<NotificationEvent>) {
    assert!(
        matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "idempotent request broadcast another notification"
    );
}

async fn notice_count(pool: &db::DbPool, user_id: i64, kind: &str) -> usize {
    db::notifications::records::list_notifications_for_user(pool, user_id, 100, false)
        .await
        .unwrap()
        .into_iter()
        .filter(|notification| notification.kind == kind)
        .count()
}

async fn idempotent_relationship_notifications(pg: bool) {
    let mut f = Fixture::new(pg).await;
    f.complete_setup().await;
    let owner_id = f.actor("idempotent_owner").await;
    let author_id = f.actor("idempotent_author").await;
    let reviewer_id = f.actor("idempotent_reviewer").await;
    let watcher_id = f.actor("idempotent_watcher").await;
    let repository_id = f.repo(owner_id, None, "idempotent_project").await;
    db::repositories::access::grant_repo_access(
        f.pool(),
        author_id,
        repository_id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        reviewer_id,
        repository_id,
        AccessLevel::Write,
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        f.pool(),
        watcher_id,
        repository_id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    let pull = db::pull_requests::records::create_pull_request(
        f.pool(),
        db::pull_requests::records::NewPullRequest {
            repo_id: repository_id,
            head_repo_id: Some(repository_id),
            author_id,
            title: "Review me",
            body: "",
            head_branch: "topic",
            head_sha: "1111111111111111111111111111111111111111",
            base_branch: "main",
            base_sha: "2222222222222222222222222222222222222222",
            is_draft: false,
        },
    )
    .await
    .unwrap();

    // Existing installations can contain other watch levels. Moving one to
    // `watching` is a real change; repeating `watching` is not.
    f.execute(
        "INSERT INTO watches(user_id,repo_id,level) VALUES($1,$2,'ignoring')",
        &[watcher_id, repository_id],
    )
    .await;

    let root = tempfile::tempdir().unwrap();
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let mut events = state.notification_broadcaster.subscribe(owner_id);
    let mut reviewer_events = state.notification_broadcaster.subscribe(reviewer_id);
    let router = create_router(state);
    let owner = Client::login(&router, f.pool(), owner_id).await;
    let watcher = Client::login(&router, f.pool(), watcher_id).await;

    let reviewer_path = format!(
        "/api/repos/idempotent_owner/idempotent_project/pulls/{}/requested_reviewers",
        pull.number
    );
    let first = owner
        .expect(
            "POST",
            &reviewer_path,
            json!({"username": "idempotent_reviewer"}),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(first["user_id"], reviewer_id);
    assert_created_event(
        f.pool(),
        reviewer_events.recv().await.unwrap(),
        reviewer_id,
        "pull_request_review_requested",
    )
    .await;
    owner
        .expect(
            "POST",
            &reviewer_path,
            json!({"username": "idempotent_reviewer"}),
            StatusCode::OK,
        )
        .await;
    assert_no_event(&mut reviewer_events);
    assert_no_event(&mut events);
    assert_no_event(&mut reviewer_events);
    assert_eq!(
        db::pull_requests::reviews::list_pull_request_reviewers(f.pool(), pull.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        notice_count(f.pool(), reviewer_id, "pull_request_review_requested").await,
        1
    );

    let watch_path = "/api/repos/idempotent_owner/idempotent_project/watch";
    assert_eq!(
        watcher
            .expect("GET", watch_path, Value::Null, StatusCode::OK)
            .await,
        json!({"watched": false, "watchers_count": 0})
    );
    assert_eq!(
        watcher
            .expect("PUT", watch_path, Value::Null, StatusCode::OK)
            .await,
        json!({"watched": true, "watchers_count": 1})
    );
    assert_created_event(
        f.pool(),
        events.recv().await.unwrap(),
        owner_id,
        "repository_watched",
    )
    .await;
    assert_eq!(
        notice_count(f.pool(), owner_id, "repository_watched").await,
        1
    );
    assert_eq!(
        watcher
            .expect("PUT", watch_path, Value::Null, StatusCode::OK)
            .await,
        json!({"watched": true, "watchers_count": 1})
    );
    assert_no_event(&mut events);
    assert_eq!(
        notice_count(f.pool(), owner_id, "repository_watched").await,
        1
    );

    assert_eq!(
        watcher
            .expect("DELETE", watch_path, Value::Null, StatusCode::OK)
            .await,
        json!({"watched": false, "watchers_count": 0})
    );

    // Repeated PUTs and toggles must not flood the owner's inbox.
    let star_path = "/api/repos/idempotent_owner/idempotent_project/star";
    watcher
        .expect("PUT", star_path, Value::Null, StatusCode::OK)
        .await;
    assert_created_event(
        f.pool(),
        events.recv().await.unwrap(),
        owner_id,
        "repository_starred",
    )
    .await;
    watcher
        .expect("PUT", star_path, Value::Null, StatusCode::OK)
        .await;
    assert_no_event(&mut events);
    watcher
        .expect("DELETE", star_path, Value::Null, StatusCode::OK)
        .await;
    watcher
        .expect("PUT", star_path, Value::Null, StatusCode::OK)
        .await;
    assert_no_event(&mut events);
    assert_eq!(
        notice_count(f.pool(), owner_id, "repository_starred").await,
        1
    );

    f.finish().await;
}

// The notifications page model that a hydrated client receives. Requesting the model
// keeps these checks independent of the browser bundle that HTML pages need.
pub(super) async fn notifications_model(client: &Client, path: &str) -> Value {
    let response = client
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header(
                    header::ACCEPT,
                    codeyard::webui::bootstrap::MODEL_CONTENT_TYPE,
                )
                .header(header::COOKIE, format!("session_id={}", client.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let envelope: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(envelope["page"]["lane"], "Notifications");
    envelope["page"]["model"].clone()
}

async fn notification_history_pages(pg: bool) {
    let mut f = Fixture::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("history_owner").await;
    let user = f.actor("history_reader").await;
    let repo = f.repo(owner, None, "history_project").await;
    let hidden = f.repo(owner, None, "history_hidden").await;
    db::repositories::access::grant_repo_access(f.pool(), user, repo, AccessLevel::Read)
        .await
        .unwrap();
    for _ in 0..10 {
        f.notify(user, Some(repo), "REVOKED_HISTORY").await;
    }
    let mut visible = Vec::new();
    for index in 0..115 {
        visible.push(f.notify(user, None, &format!("history-{index}")).await);
    }
    for _ in 0..125 {
        f.notify(user, Some(hidden), "HIDDEN_HISTORY").await;
    }
    // Fractional persisted timestamps must survive the SQLite cursor unchanged.
    f.execute(
        "UPDATE notifications SET created_at='2026-01-01T00:00:00.123456Z' WHERE user_id=$1",
        &[user],
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let router = create_router(state);
    let client = Client::login(&router, f.pool(), user).await;
    let unread = client
        .expect(
            "GET",
            "/api/notifications?limit=2&unread_only=true",
            json!(null),
            StatusCode::OK,
        )
        .await;
    let cursor = unread["next_cursor"].as_str().unwrap();
    let second = client
        .expect(
            "GET",
            &format!("/api/notifications?limit=2&unread_only=true&before={cursor}"),
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(second["notifications"][0]["id"], visible[112]);
    assert_eq!(second["unread_count"], 125);
    client
        .expect(
            "POST",
            "/api/notifications/read-all",
            json!({}),
            StatusCode::OK,
        )
        .await;
    let first = client
        .expect(
            "GET",
            "/api/notifications?limit=100",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(first["notifications"].as_array().unwrap().len(), 100);
    assert_eq!(first["notifications"][99]["id"], visible[15]);
    assert_eq!(first["unread_count"], 0);
    let cursor = first["next_cursor"].as_str().unwrap();
    let page = notifications_model(&client, "/notifications").await;
    assert!(page["older"]
        .as_str()
        .unwrap()
        .contains(&format!("before={cursor}")));
    assert!(!page.to_string().contains("HIDDEN_HISTORY"));
    // Neither deletion of the cursor row nor a newer insert shifts the next page.
    f.execute("DELETE FROM notifications WHERE id=$1", &[visible[15]])
        .await;
    let newer = f.notify(user, None, "new-history").await;
    f.execute(
        "UPDATE notifications SET created_at='2026-01-02T00:00:00.000000Z' WHERE id=$1",
        &[newer],
    )
    .await;
    db::repositories::access::revoke_repo_access(f.pool(), user, repo)
        .await
        .unwrap();
    let older = client
        .expect(
            "GET",
            &format!("/api/notifications?limit=100&before={cursor}"),
            json!(null),
            StatusCode::OK,
        )
        .await;
    let older_ids: Vec<i64> = older["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_i64().unwrap())
        .collect();
    assert_eq!(
        older_ids,
        visible[..15].iter().rev().copied().collect::<Vec<_>>()
    );
    assert!(older["next_cursor"].is_null());
    assert_eq!(older["unread_count"], 1);
    let page = notifications_model(&client, &format!("/notifications?before={cursor}")).await;
    let text = page.to_string();
    assert!(text.contains("history-0"));
    assert!(!text.contains("REVOKED_HISTORY"));
    assert!(!text.contains("HIDDEN_HISTORY"));
    assert!(page["older"].is_null());
    for path in [
        "/api/notifications?before=invalid",
        "/notifications?before=invalid",
    ] {
        let (status, _) = client.send("GET", path, "text/html", String::new()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    f.finish().await;
}

// Acknowledging a row from an older page returns to that same page, not the newest one.
async fn mark_read_keeps_history_position(pg: bool) {
    let mut f = Fixture::new(pg).await;
    f.complete_setup().await;
    let user = f.actor("position_reader").await;
    for index in 0..101 {
        f.notify(user, None, &format!("position-{index}")).await;
    }
    let root = tempfile::tempdir().unwrap();
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let router = create_router(state);
    let client = Client::login(&router, f.pool(), user).await;
    // The first page is full, so it links to an older page.
    let first = notifications_model(&client, "/notifications").await;
    let older = first["older"].as_str().unwrap().to_owned();
    let cursor = older.split("before=").nth(1).unwrap().to_owned();
    let second = notifications_model(&client, &older).await;
    assert_eq!(second["before"], cursor.as_str());
    let id = second["notifications"][0]["id"].as_i64().unwrap();
    let reply = client
        .exchange(
            "POST",
            &format!("/notifications/{id}/read"),
            "application/x-www-form-urlencoded",
            format!("csrf_token={}&unread_only=0&before={cursor}", client.csrf).into_bytes(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.location.as_deref(),
        Some(format!("/notifications?before={cursor}").as_str())
    );
    f.finish().await;
}

#[tokio::test]
async fn postgres_mark_read_keeps_history_position() {
    mark_read_keeps_history_position(true).await;
}
#[tokio::test]
async fn sqlite_mark_read_keeps_history_position() {
    mark_read_keeps_history_position(false).await;
}

#[tokio::test]
async fn postgres_notification_history_pages() {
    notification_history_pages(true).await;
}
#[tokio::test]
async fn sqlite_notification_history_pages() {
    notification_history_pages(false).await;
}

#[tokio::test]
async fn postgres_notification_authority() {
    authority_contract(true).await;
}
#[tokio::test]
async fn sqlite_notification_authority() {
    authority_contract(false).await;
}
#[tokio::test]
async fn postgres_notification_bounded_projection() {
    bounded_projection(true).await;
}
#[tokio::test]
async fn sqlite_notification_bounded_projection() {
    bounded_projection(false).await;
}
#[tokio::test]
async fn postgres_notification_corrupt_row_privacy() {
    corrupt_row_privacy(true).await;
}
#[tokio::test]
async fn sqlite_notification_corrupt_row_privacy() {
    corrupt_row_privacy(false).await;
}
#[tokio::test]
async fn postgres_idempotent_relationship_notifications() {
    idempotent_relationship_notifications(true).await;
}
#[tokio::test]
async fn sqlite_idempotent_relationship_notifications() {
    idempotent_relationship_notifications(false).await;
}
