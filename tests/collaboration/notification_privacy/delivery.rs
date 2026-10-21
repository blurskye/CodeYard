//! Original live-session and stale/forged wakeup projection contracts.
use super::{db, Fixture};
use crate::support::database::wait_until_blocked;
use chrono::{Duration, Utc};
use codeyard::backend::notifications::{current_event, NotificationEvent};

fn hint(user_id: i64, notification_id: i64) -> NotificationEvent {
    NotificationEvent::Created {
        user_id,
        notification_id,
        kind: "FORGED_KIND".into(),
        title: "FORGED_PRIVATE_PAYLOAD".into(),
        body: Some("FORGED_BODY".into()),
        link: Some("FORGED_LINK".into()),
        created_at: -1,
        unread_count: i64::MAX,
    }
}

async fn contract(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("delivery_owner").await;
    let recipient = f.actor("delivery_recipient").await;
    let foreign = f.actor("delivery_foreign").await;
    let repo = f.repo(owner, None, "delivery_private").await;
    let account = f.notify(recipient, None, "owned-account").await;
    let notice = f.notify(recipient, Some(repo), "owned-repository").await;
    let foreign_notice = f.notify(foreign, None, "FOREIGN_PAYLOAD").await;
    let session = "12".repeat(32);
    let replacement = "34".repeat(32);
    let pool = f.pool();
    let broadcaster = codeyard::backend::notifications::NotificationBroadcaster::new(pool);
    let mut receiver = broadcaster.subscribe(recipient);
    broadcaster.send(hint(recipient, account)).await;
    let local = serde_json::to_value(receiver.recv().await.unwrap()).unwrap();
    assert!(
        !local.to_string().contains("FORGED"),
        "local broadcasts carry only ids"
    );
    assert_eq!(local["notification_id"], account);
    db::users::sessions::create_session(
        pool,
        &session,
        recipient,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    assert!(
        current_event(pool, &session, recipient, &hint(recipient, notice))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        current_event(pool, &session, recipient, &hint(recipient, foreign_notice))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        current_event(pool, &session, recipient, &hint(foreign, foreign_notice))
            .await
            .unwrap()
            .is_none()
    );
    let actual = current_event(pool, &session, recipient, &hint(recipient, account))
        .await
        .unwrap()
        .unwrap();
    let NotificationEvent::Created {
        title,
        body,
        link,
        unread_count,
        created_at,
        kind,
        ..
    } = actual
    else {
        panic!()
    };
    assert_eq!(title, "owned-account");
    assert_eq!(body.as_deref(), Some("owned-account"));
    assert_eq!(link.as_deref(), Some("owned-account"));
    assert_eq!(kind, "account");
    assert_eq!(unread_count, 1);
    assert!(created_at > 0);
    db::repositories::access::grant_repo_access(
        pool,
        recipient,
        repo,
        db::models::repositories::AccessLevel::Read,
    )
    .await
    .unwrap();
    let value = serde_json::to_value(
        current_event(pool, &session, recipient, &hint(recipient, notice))
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["title"], "owned-repository");
    assert_eq!(value["unread_count"], 2);
    assert!(!value.to_string().contains("FORGED"));
    let read = NotificationEvent::Read {
        user_id: recipient,
        notification_id: notice,
        unread_count: i64::MAX,
    };
    assert!(current_event(pool, &session, recipient, &read)
        .await
        .unwrap()
        .is_none());
    db::notifications::records::mark_notification_read(pool, recipient, notice)
        .await
        .unwrap()
        .unwrap();
    let value = serde_json::to_value(
        current_event(pool, &session, recipient, &read)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["unread_count"], 1);
    db::repositories::access::revoke_repo_access(pool, recipient, repo)
        .await
        .unwrap();
    assert!(current_event(pool, &session, recipient, &read)
        .await
        .unwrap()
        .is_none());
    let all = NotificationEvent::ReadAll {
        user_id: recipient,
        unread_count: i64::MAX,
    };
    assert_eq!(
        serde_json::to_value(
            current_event(pool, &session, recipient, &all)
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap()["unread_count"],
        1
    );
    for restriction in ["is_banned", "must_change_password"] {
        f.execute(
            &format!("UPDATE users SET {restriction}=TRUE WHERE id=$1"),
            &[recipient],
        )
        .await;
        assert!(
            current_event(pool, &session, recipient, &hint(recipient, account))
                .await
                .is_err()
        );
        assert!(
            db::notifications::records::notification_delivery(
                pool,
                &session,
                recipient,
                Some(account)
            )
            .await
            .unwrap()
            .is_none(),
            "native delivery retains the same recipient restriction"
        );
        f.execute(
            &format!("UPDATE users SET {restriction}=FALSE WHERE id=$1"),
            &[recipient],
        )
        .await;
    }
    assert!(
        current_event(pool, &session, foreign, &hint(foreign, foreign_notice))
            .await
            .is_err()
    );
    assert!(
        db::notifications::records::notification_delivery(
            pool,
            &session,
            foreign,
            Some(foreign_notice)
        )
        .await
        .unwrap()
        .is_none(),
        "native delivery cannot substitute a foreign session recipient"
    );
    f.execute(
        "UPDATE sessions SET expires_at='2000-01-01T00:00:00.000000Z' WHERE user_id=$1",
        &[recipient],
    )
    .await;
    assert!(current_event(pool, &session, recipient, &all)
        .await
        .is_err());
    assert!(
        db::notifications::records::notification_delivery(pool, &session, recipient, Some(account))
            .await
            .unwrap()
            .is_none(),
        "native delivery rejects expired original sessions"
    );
    db::users::sessions::delete_session(pool, &session)
        .await
        .unwrap();
    db::users::sessions::create_session(
        pool,
        &replacement,
        recipient,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    assert!(
        current_event(pool, &session, recipient, &all)
            .await
            .is_err(),
        "replacement login never revives the original socket"
    );
    assert!(
        db::notifications::records::notification_delivery(pool, &session, recipient, Some(account))
            .await
            .unwrap()
            .is_none(),
        "native delivery never adopts the replacement login"
    );
    assert!(current_event(pool, &replacement, recipient, &all)
        .await
        .unwrap()
        .is_some());
    f.execute("DROP TABLE notifications", &[]).await;
    assert!(
        current_event(pool, &replacement, recipient, &all)
            .await
            .is_err(),
        "database outage never forwards a cached hint"
    );
    f.finish().await;
}

async fn canonical_links(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("link_owner").await;
    let recipient = f.actor("link_reader").await;
    let repo = f.repo(owner, None, "original").await;
    db::repositories::access::grant_repo_access(
        f.pool(),
        recipient,
        repo,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let session = "ab".repeat(32);
    db::users::sessions::create_session(
        f.pool(),
        &session,
        recipient,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    let mut ids = Vec::new();
    for (kind, suffix) in [
        ("ci_run_failed", "actions?job=9#log"),
        ("release_published", "releases/tag/v1?download=1#assets"),
    ] {
        let id = f.notify(recipient, Some(repo), kind).await;
        f.execute(&format!("UPDATE notifications SET kind='{kind}',link='/link_owner/original/{suffix}' WHERE id=$1"),&[id]).await;
        ids.push((id, suffix));
    }
    f.execute(
        "UPDATE repositories SET name='renamed' WHERE id=$1",
        &[repo],
    )
    .await;
    for (id, suffix) in &ids {
        let projection = db::notifications::records::notification_delivery(
            f.pool(),
            &session,
            recipient,
            Some(*id),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            projection.notification.unwrap().link.unwrap(),
            format!("/link_owner/renamed/{suffix}")
        );
    }
    let inbox = db::notifications::records::notification_inbox(
        f.pool(),
        &session,
        recipient,
        db::notifications::records::NotificationInboxAction::List {
            filter: Default::default(),
            limit: 100,
            before: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(inbox.notifications.len(), 2);
    assert!(inbox.notifications.iter().all(|n| n
        .link
        .as_deref()
        .unwrap()
        .starts_with("/link_owner/renamed/")));
    db::repositories::access::revoke_repo_access(f.pool(), recipient, repo)
        .await
        .unwrap();
    f.execute(
        "UPDATE repositories SET name='private_namespace' WHERE id=$1",
        &[repo],
    )
    .await;
    // A returned projection stays at its authorized snapshot; the next read discloses no link.
    assert!(!serde_json::to_string(&inbox.notifications)
        .unwrap()
        .contains("private_namespace"));
    let projection = db::notifications::records::notification_delivery(
        f.pool(),
        &session,
        recipient,
        Some(ids[0].0),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(projection.notification.is_none());
    f.finish().await;
}

#[tokio::test]
async fn postgres_notification_links_are_projected_under_authorization() {
    canonical_links(true).await;
}
#[tokio::test]
async fn sqlite_notification_links_are_projected_under_authorization() {
    canonical_links(false).await;
}

#[tokio::test]
async fn postgres_original_live_session_delivery() {
    contract(true).await;
}
#[tokio::test]
async fn sqlite_original_live_session_delivery() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_delivery_waits_for_recipient_row_and_observes_session_revocation() {
    let mut fixture = Fixture::new(true).await;
    let recipient = fixture.actor("delivery_waiter").await;
    let notice = fixture.notify(recipient, None, "waiter-account").await;
    let session = "ab".repeat(32);
    db::users::sessions::create_session(
        fixture.pool(),
        &session,
        recipient,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    let raw = fixture.pg();
    let mut revoker = raw.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *revoker)
        .await
        .unwrap();
    // The recipient's account row is the first thing delivery locks.
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(recipient)
        .fetch_one(&mut *revoker)
        .await
        .unwrap();
    let pool = fixture.pool().clone();
    let original = session.clone();
    let task = tokio::spawn(async move {
        current_event(&pool, &original, recipient, &hint(recipient, notice)).await
    });
    wait_until_blocked(raw, blocker, 1).await;
    assert!(!task.is_finished());
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR UPDATE NOWAIT")
        .bind(&session)
        .fetch_one(&mut *revoker)
        .await
        .unwrap();
    sqlx::query("DELETE FROM sessions WHERE id=$1")
        .bind(session)
        .execute(&mut *revoker)
        .await
        .unwrap();
    revoker.commit().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    fixture.finish().await;
}
