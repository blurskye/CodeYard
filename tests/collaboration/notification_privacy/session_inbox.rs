//! Original-session persisted inbox contracts on real native stores.
use super::{db, ids, Fixture};
use chrono::{Duration as ChronoDuration, Utc};
use db::notifications::records::{
    NotificationFilter, NotificationInbox, NotificationInboxAction as Action,
};

fn actions(notification_id: i64) -> [Action; 3] {
    [
        Action::List {
            limit: 100,
            filter: NotificationFilter::inbox(false),
            before: None,
        },
        Action::Read { notification_id },
        Action::ReadAll,
    ]
}

pub(super) async fn session(pool: &db::DbPool, user: i64) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string().repeat(2);
    db::users::sessions::create_session(
        pool,
        &id,
        user,
        None,
        None,
        Utc::now() + ChronoDuration::hours(1),
    )
    .await
    .unwrap();
    id
}

async fn inbox(f: &Fixture, session: &str, user: i64, action: Action) -> NotificationInbox {
    db::notifications::records::notification_inbox(f.pool(), session, user, action)
        .await
        .unwrap()
        .unwrap()
}

async fn denied(f: &Fixture, session: &str, user: i64, notice: i64) {
    let before = f.history(user).await;
    for action in actions(notice) {
        assert!(
            db::notifications::records::notification_inbox(f.pool(), session, user, action)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(f.history(user).await, before, "invalid session cannot ack");
    }
}

// Each transition exercises list/one/all with a live original session. Hidden
// history (including payload/provenance/read_at) remains byte-identical.
async fn visible(f: &Fixture, session: &str, user: i64, expected: &[i64]) {
    let before = f.history(user).await;
    let mut expected = expected.to_vec();
    expected.sort_unstable_by(|a, b| b.cmp(a));
    let list = inbox(
        f,
        session,
        user,
        Action::List {
            limit: 100,
            filter: NotificationFilter::inbox(false),
            before: None,
        },
    )
    .await;
    assert_eq!(ids(&list.notifications), expected);
    assert_eq!(list.unread_count, expected.len() as i64);
    assert_eq!(list.updated, 0);
    for row in before
        .iter()
        .filter(|r| !expected.contains(&r["id"].as_i64().unwrap()))
    {
        let read = inbox(
            f,
            session,
            user,
            Action::Read {
                notification_id: row["id"].as_i64().unwrap(),
            },
        )
        .await;
        assert!(read.notifications.is_empty());
        assert_eq!(read.updated, 0);
        assert_eq!(read.unread_count, expected.len() as i64);
    }
    if let Some(id) = expected.first() {
        let one = inbox(
            f,
            session,
            user,
            Action::Read {
                notification_id: *id,
            },
        )
        .await;
        assert_eq!(ids(&one.notifications), vec![*id]);
        assert_eq!(one.updated, 1);
        assert_eq!(one.unread_count, expected.len() as i64 - 1);
        let again = inbox(
            f,
            session,
            user,
            Action::Read {
                notification_id: *id,
            },
        )
        .await;
        assert_eq!(again.updated, 0);
        assert_eq!(again.unread_count, one.unread_count);
        assert_eq!(again.notifications[0].read_at, one.notifications[0].read_at);
    }
    let all = inbox(f, session, user, Action::ReadAll).await;
    assert!(all.notifications.is_empty());
    assert_eq!(all.updated, expected.len().saturating_sub(1) as u64);
    assert_eq!(all.unread_count, 0);
    let again = inbox(f, session, user, Action::ReadAll).await;
    assert_eq!((again.updated, again.unread_count), (0, 0));
    let list = inbox(
        f,
        session,
        user,
        Action::List {
            limit: 100,
            filter: NotificationFilter::inbox(true),
            before: None,
        },
    )
    .await;
    assert!(list.notifications.is_empty());
    assert_eq!((list.updated, list.unread_count), (0, 0));
    let after = f.history(user).await;
    for (old, new) in before.iter().zip(&after) {
        if !expected.contains(&old["id"].as_i64().unwrap()) {
            assert_eq!(old, new, "hidden history cannot be acknowledged or adopted");
        } else {
            let mut old = old.clone();
            let mut new = new.clone();
            old.as_object_mut().unwrap().remove("read_at");
            new.as_object_mut().unwrap().remove("read_at");
            assert_eq!(old, new, "ack only changes read_at");
        }
    }
    for id in expected {
        f.execute("UPDATE notifications SET read_at=NULL WHERE id=$1", &[id])
            .await;
    }
}

async fn contract(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("inbox_owner").await;
    let user = f.actor("inbox_recipient").await;
    let foreign = f.actor("inbox_foreign").await;
    let original = session(f.pool(), user).await;
    let foreign_session = session(f.pool(), foreign).await;
    // Valid empty inbox is Some, not session denial.
    visible(&f, &original, user, &[]).await;
    let account = f.notify(user, None, "owned-inbox-account").await;
    let repo = f.repo(owner, None, "inbox_private").await;
    let notice = f.notify(user, Some(repo), "private-inbox-payload").await;
    let foreign_notice = f.notify(foreign, None, "FOREIGN_INBOX_PAYLOAD").await;
    visible(&f, &original, user, &[account]).await;
    denied(&f, &foreign_session, user, account).await;
    denied(&f, &original, foreign, foreign_notice).await;
    let before = f.history(foreign).await;
    let result = inbox(
        &f,
        &original,
        user,
        Action::Read {
            notification_id: foreign_notice,
        },
    )
    .await;
    assert!(result.notifications.is_empty());
    assert_eq!((result.updated, result.unread_count), (0, 1));
    assert_eq!(f.history(foreign).await, before);
    db::repositories::access::grant_repo_access(
        f.pool(),
        user,
        repo,
        db::models::repositories::AccessLevel::Read,
    )
    .await
    .unwrap();
    visible(&f, &original, user, &[account, notice]).await;
    db::repositories::access::revoke_repo_access(f.pool(), user, repo)
        .await
        .unwrap();
    visible(&f, &original, user, &[account]).await;
    db::repositories::access::grant_repo_access(
        f.pool(),
        user,
        repo,
        db::models::repositories::AccessLevel::Read,
    )
    .await
    .unwrap();
    f.execute("DELETE FROM repositories WHERE id=$1", &[repo])
        .await;
    let history = f.history(user).await;
    let deleted = history.iter().find(|r| r["id"] == notice).unwrap();
    assert!(deleted["repo"].is_null());
    assert_eq!(deleted["scope"], repo);
    visible(&f, &original, user, &[account]).await;
    let missing_repo = f.repo(owner, None, "inbox_missing_origin").await;
    let missing = f
        .notify(user, Some(missing_repo), "missing-origin-private")
        .await;
    // A dangling current FK cannot erase or replace a missing original scope.
    f.corrupt("DELETE FROM repositories WHERE id=$1", &[missing_repo])
        .await;
    f.corrupt(
        "UPDATE notifications SET repo_id=900000 WHERE id=$1",
        &[missing],
    )
    .await;
    visible(&f, &original, user, &[account]).await;
    let org = f
        .scalar(
            "INSERT INTO organizations(name) VALUES('inbox_org') RETURNING id",
            &[],
        )
        .await;
    f.execute(
        "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
        &[org, owner],
    )
    .await;
    let tenant = f.repo(owner, Some(org), "inbox_tenant").await;
    f.notify(user, Some(tenant), "site-admin-cannot-read-tenant")
        .await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[user])
        .await;
    visible(&f, &original, user, &[account]).await;
    for restriction in ["is_banned", "must_change_password"] {
        f.execute(
            &format!("UPDATE users SET {restriction}=TRUE WHERE id=$1"),
            &[user],
        )
        .await;
        denied(&f, &original, user, account).await;
        f.execute(
            &format!("UPDATE users SET {restriction}=FALSE WHERE id=$1"),
            &[user],
        )
        .await;
    }
    f.execute(
        "UPDATE sessions SET expires_at='2000-01-01T00:00:00.000000Z' WHERE user_id=$1",
        &[user],
    )
    .await;
    denied(&f, &original, user, account).await;
    db::users::sessions::delete_session(f.pool(), &original)
        .await
        .unwrap();
    denied(&f, &original, user, account).await;
    let replacement = session(f.pool(), user).await;
    denied(&f, &original, user, account).await;
    visible(&f, &replacement, user, &[account]).await;
    // SQL failures remain unavailable, never valid empty/zero success.
    f.execute("DROP TABLE notifications", &[]).await;
    for action in actions(account) {
        let error =
            db::notifications::records::notification_inbox(f.pool(), &replacement, user, action)
                .await
                .err()
                .expect("native SQL failure");
        assert!(error.downcast_ref::<sqlx::Error>().is_some());
    }
    f.finish().await;
}

async fn bounded_list(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("inbox_page_owner").await;
    let user = f.actor("inbox_page_user").await;
    let original = session(f.pool(), user).await;
    let repo = f.repo(owner, None, "inbox_page_private").await;
    let first = f.notify(user, None, "first-visible").await;
    let second = f.notify(user, None, "second-visible").await;
    for _ in 0..105 {
        f.notify(user, Some(repo), "HIDDEN_PAGE_PAYLOAD").await;
    }
    for limit in [i64::MIN, 0, 1, 2, i64::MAX] {
        for unread_only in [false, true] {
            let result = inbox(
                &f,
                &original,
                user,
                Action::List {
                    limit,
                    filter: NotificationFilter::inbox(unread_only),
                    before: None,
                },
            )
            .await;
            let expected = if limit <= 1 {
                vec![second]
            } else {
                vec![second, first]
            };
            assert_eq!(ids(&result.notifications), expected);
            assert_eq!((result.updated, result.unread_count), (0, 2));
            assert!(!serde_json::to_string(&result.notifications)
                .unwrap()
                .contains("HIDDEN_PAGE_PAYLOAD"));
        }
    }
    visible(&f, &original, user, &[second, first]).await;
    f.finish().await;
}

#[tokio::test]
async fn postgres_original_session_inbox_bounded_list() {
    bounded_list(true).await;
}
#[tokio::test]
async fn sqlite_original_session_inbox_bounded_list() {
    bounded_list(false).await;
}

#[tokio::test]
async fn postgres_original_session_inbox_contract() {
    contract(true).await;
}
#[tokio::test]
async fn sqlite_original_session_inbox_contract() {
    contract(false).await;
}

// A session that is alive when an acknowledgment starts but expires while the read is staged
// must not record it. A trigger slows each acknowledgment down past the short expiry; a session
// that does not expire shows the same acknowledgment is recorded normally.
async fn expiry_during_acknowledgment_rolls_back(pg: bool) {
    let mut fixture = Fixture::new(pg).await;
    let user = fixture.actor("expiry_reader").await;
    let one = fixture.notify(user, None, "expiry-one").await;
    fixture.notify(user, None, "expiry-all").await;
    if pg {
        fixture
            .execute(
                "CREATE FUNCTION slow_acknowledgment() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(1); RETURN NEW; END; $$",
                &[],
            )
            .await;
        fixture
            .execute(
                "CREATE TRIGGER slow_acknowledgment AFTER UPDATE ON notifications FOR EACH ROW EXECUTE FUNCTION slow_acknowledgment()",
                &[],
            )
            .await;
    } else {
        fixture
            .execute(
                "CREATE TRIGGER slow_acknowledgment AFTER UPDATE ON notifications BEGIN SELECT count(*) FROM (WITH RECURSIVE spin(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM spin WHERE n<3000000) SELECT n FROM spin); END",
                &[],
            )
            .await;
    }
    let expires_soon = if pg {
        "clock_timestamp()+interval '300 milliseconds'"
    } else {
        "strftime('%Y-%m-%dT%H:%M:%f000Z','now','+0.3 seconds')"
    };
    for action in [
        Action::Read {
            notification_id: one,
        },
        Action::ReadAll,
    ] {
        let doomed = session(fixture.pool(), user).await;
        fixture
            .execute(
                &format!("UPDATE sessions SET expires_at={expires_soon} WHERE id='{doomed}'"),
                &[],
            )
            .await;
        let before = fixture.history(user).await;
        assert!(
            db::notifications::records::notification_inbox(
                fixture.pool(),
                &doomed,
                user,
                action.clone()
            )
            .await
            .unwrap()
            .is_none(),
            "an acknowledgment finishing after the session expired is refused"
        );
        assert_eq!(
            fixture.history(user).await,
            before,
            "the staged acknowledgment rolls back"
        );
        let live = session(fixture.pool(), user).await;
        assert_eq!(inbox(&fixture, &live, user, action).await.updated, 1);
    }
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_expiry_during_acknowledgment_rolls_back() {
    expiry_during_acknowledgment_rolls_back(true).await;
}
#[tokio::test]
async fn sqlite_expiry_during_acknowledgment_rolls_back() {
    expiry_during_acknowledgment_rolls_back(false).await;
}

/// Marking a notification done changes the unread count, so subscribers hear the new count. A
/// mark that changes nothing is not announced.
async fn done_marks_are_announced(pg: bool) {
    let mut fixture = Fixture::new(pg).await;
    let user = fixture.actor("done_reader").await;
    let repo = fixture.repo(user, None, "done_repo").await;
    let first = fixture.notify(user, Some(repo), "done-first").await;
    fixture.notify(user, Some(repo), "done-second").await;
    let original = session(fixture.pool(), user).await;
    let pool = fixture.pool();
    let broadcaster = codeyard::backend::notifications::NotificationBroadcaster::new(pool);
    let mut events = broadcaster.subscribe(user);
    async fn announced(
        pool: &db::DbPool,
        session: &str,
        user: i64,
        events: &mut tokio::sync::broadcast::Receiver<
            codeyard::backend::notifications::NotificationEvent,
        >,
    ) -> (i64, i64) {
        let hint = events.try_recv().unwrap();
        match codeyard::backend::notifications::current_event(pool, session, user, &hint)
            .await
            .unwrap()
            .unwrap()
        {
            codeyard::backend::notifications::NotificationEvent::UnreadCount {
                user_id,
                unread_count,
            } => (user_id, unread_count),
            other => panic!("expected an unread count, got {other:?}"),
        }
    }

    let done_first = Action::Done {
        notification_id: first,
        done: true,
    };
    codeyard::backend::notifications::inbox::inbox(pool, &broadcaster, &original, user, done_first)
        .await
        .unwrap();
    assert_eq!(
        announced(pool, &original, user, &mut events).await,
        (user, 1)
    );

    let again = Action::Done {
        notification_id: first,
        done: true,
    };
    codeyard::backend::notifications::inbox::inbox(pool, &broadcaster, &original, user, again)
        .await
        .unwrap();
    assert!(
        events.try_recv().is_err(),
        "a repeated mark changes nothing"
    );

    let all = Action::DoneAll {
        filter: NotificationFilter::inbox(false),
    };
    codeyard::backend::notifications::inbox::inbox(pool, &broadcaster, &original, user, all)
        .await
        .unwrap();
    assert_eq!(
        announced(pool, &original, user, &mut events).await,
        (user, 0)
    );
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_done_marks_are_announced() {
    done_marks_are_announced(true).await;
}
#[tokio::test]
async fn sqlite_done_marks_are_announced() {
    done_marks_are_announced(false).await;
}

// A one-row page walks past a long hidden tail and still returns the newest visible row.
async fn page_walk(pg: bool) {
    let mut fixture = Fixture::new(pg).await;
    let owner = fixture.actor("page_walk_owner").await;
    let user = fixture.actor("page_walk_reader").await;
    let original = session(fixture.pool(), user).await;
    for index in 0..200 {
        let repo = fixture
            .repo(owner, None, &format!("page_walk_read_{index}"))
            .await;
        db::repositories::access::grant_repo_access(
            fixture.pool(),
            user,
            repo,
            db::models::repositories::AccessLevel::Read,
        )
        .await
        .unwrap();
        fixture.notify(user, Some(repo), "read-history").await;
    }
    fixture.notify(user, None, "older-visible").await;
    let hidden = fixture.repo(owner, None, "page_walk_hidden").await;
    for _ in 0..1000 {
        fixture.notify(user, Some(hidden), "hidden-tail").await;
    }
    let newest = fixture.notify(user, None, "newest-visible").await;
    fixture
        .execute(
            "UPDATE notifications SET read_at=created_at WHERE user_id=$1 AND id<>$2",
            &[user, newest],
        )
        .await;
    let result = inbox(
        &fixture,
        &original,
        user,
        Action::List {
            limit: 1,
            filter: NotificationFilter::inbox(false),
            before: None,
        },
    )
    .await;
    assert_eq!(result.notifications.len(), 1);
    assert_eq!(result.notifications[0].id, newest);
    assert!(result.next_cursor.is_some());
    assert_eq!(result.unread_count, 1);
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_notification_page_walk_skips_hidden_history() {
    page_walk(true).await;
}
#[tokio::test]
async fn sqlite_notification_page_walk_skips_hidden_history() {
    page_walk(false).await;
}

// A large backlog in a hidden repository is not counted, listed or acknowledged, and creating
// a notification reports only the visible unread count.
async fn hidden_backlog(pg: bool) {
    let mut fixture = Fixture::new(pg).await;
    let owner = fixture.actor("scope_work_owner").await;
    let user = fixture.actor("scope_work_reader").await;
    let visible = fixture.repo(user, None, "scope_work_visible").await;
    let hidden = fixture.repo(owner, None, "scope_work_hidden").await;
    let original = session(fixture.pool(), user).await;
    for _ in 0..256 {
        fixture.notify(user, Some(hidden), "hidden-backlog").await;
        fixture.notify(user, Some(visible), "visible-backlog").await;
    }
    let pool = fixture.pool();
    assert_eq!(
        db::notifications::records::count_unread_notifications(pool, user)
            .await
            .unwrap(),
        256
    );
    for action in [
        Action::List {
            limit: 1,
            filter: NotificationFilter::inbox(false),
            before: None,
        },
        Action::ReadAll,
    ] {
        let listing = matches!(&action, Action::List { .. });
        let result = inbox(&fixture, &original, user, action).await;
        assert_eq!(result.notifications.len(), usize::from(listing));
        assert_eq!(result.unread_count, if listing { 256 } else { 0 });
        assert_eq!(result.updated, if listing { 0 } else { 256 });
    }
    let broadcaster = codeyard::backend::notifications::NotificationBroadcaster::new(pool);
    let mut events = broadcaster.subscribe(user);
    codeyard::backend::notifications::create::create_and_broadcast_notification(
        pool,
        &broadcaster,
        codeyard::backend::notifications::create::NotificationRequest {
            user_id: user,
            actor_id: None,
            repo_id: Some(visible),
            kind: "issue",
            title: "created-without-recount",
            body: None,
            link: None,
            metadata: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
    let event = events.try_recv().unwrap();
    let current = codeyard::backend::notifications::current_event(pool, &original, user, &event)
        .await
        .unwrap()
        .unwrap();
    let super::NotificationEvent::Created {
        unread_count,
        title,
        ..
    } = current
    else {
        panic!()
    };
    assert_eq!(unread_count, 1);
    assert_eq!(title, "created-without-recount");
    assert_eq!(fixture.scalar("SELECT COUNT(*) FROM notifications WHERE user_id=$1 AND scope_repository_id=$2 AND read_at IS NULL", &[user, hidden]).await, 256);
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_hidden_backlog_is_not_counted_or_acknowledged() {
    hidden_backlog(true).await;
}
#[tokio::test]
async fn sqlite_hidden_backlog_is_not_counted_or_acknowledged() {
    hidden_backlog(false).await;
}
