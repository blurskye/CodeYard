//! Original-session persisted inbox contracts on real native stores.
use super::{db, ids, Fixture};
use chrono::{Duration as ChronoDuration, Utc};
use db::notifications::records::{
    NotificationFilter, NotificationInbox, NotificationInboxAction as Action,
};
use std::{
    ffi::{c_int, c_void},
    str::FromStr,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

const DEADLINE: Duration = Duration::from_secs(10);

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
    f.close().await;
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
    f.close().await;
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

#[tokio::test]
async fn postgres_inbox_locks_source_before_newer_fork_notifications() {
    let mut fixture = Fixture::new(true).await;
    let owner = fixture.actor("privacy_order_owner").await;
    let user = fixture.actor("privacy_order_reader").await;
    let original = session(fixture.pool(), user).await;
    for (index, action) in actions(0).into_iter().enumerate() {
        let source = fixture
            .repo(owner, None, &format!("privacy_source_{index}"))
            .await;
        let fork = fixture
            .repo(owner, None, &format!("privacy_fork_{index}"))
            .await;
        fixture
            .execute(
                "UPDATE repositories SET is_private=FALSE WHERE id=$1 OR id=$2",
                &[source, fork],
            )
            .await;
        fixture
            .execute(
                "UPDATE repositories SET is_fork=TRUE,forked_from_id=$1 WHERE id=$2",
                &[source, fork],
            )
            .await;
        fixture.notify(user, Some(source), "older-source").await;
        let notice = fixture.notify(user, Some(fork), "newer-fork").await;
        let action = match action {
            Action::Read { .. } => Action::Read {
                notification_id: notice,
            },
            other => other,
        };
        let Fixture::Postgres(f) = &fixture else {
            unreachable!()
        };
        let mut privacy = f.raw.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *privacy)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
            .bind(source)
            .fetch_one(&mut *privacy)
            .await
            .unwrap();
        let pool = f.pool.clone();
        let original = original.clone();
        let task = tokio::spawn(async move {
            db::notifications::records::notification_inbox(&pool, &original, user, action).await
        });
        pg_waiter(&f.raw, pid, "FROM repositories").await;
        // The visibility trigger locks the fork while the inbox waits on the source.
        // Chronological locking instead holds the fork and forms a real deadlock.
        sqlx::query("UPDATE repositories SET is_private=TRUE WHERE id=$1")
            .bind(source)
            .execute(&mut *privacy)
            .await
            .unwrap();
        privacy.commit().await.unwrap();
        let result = tokio::time::timeout(DEADLINE, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(result.notifications.is_empty());
        assert_eq!((result.unread_count, result.updated), (0, 0));
    }
    fixture.close().await;
}

// Poll the store's own clock, not host time or sleep-based ordering.
async fn expired(f: &Fixture, user: i64, pg: bool) {
    let sql = if pg {
        "SELECT COUNT(*) FROM sessions WHERE user_id=$1 AND expires_at<=clock_timestamp()"
    } else {
        "SELECT COUNT(*) FROM sessions WHERE user_id=$1 AND expires_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now')"
    };
    tokio::time::timeout(DEADLINE, async {
        while f.scalar(sql, &[user]).await == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native original session expiry");
}

async fn pg_waiter(raw: &sqlx::PgPool, blocker: i32, table: &str) {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid)) AND query LIKE $2)")
                .bind(blocker).bind(format!("%{table}%")).fetch_one(raw).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("observable native PostgreSQL lock waiter");
}

#[tokio::test]
async fn postgres_concurrent_acknowledgments_serialize_without_lock_upgrades() {
    let mut fixture = Fixture::new(true).await;
    let owner = fixture.actor("ack_owner").await;
    let user = fixture.actor("ack_recipient").await;
    let repo = fixture.repo(owner, None, "ack_repo").await;
    db::repositories::access::grant_repo_access(
        fixture.pool(),
        user,
        repo,
        db::models::repositories::AccessLevel::Read,
    )
    .await
    .unwrap();
    let original = session(fixture.pool(), user).await;
    let Fixture::Postgres(f) = &fixture else {
        unreachable!()
    };
    for read_all in [false, true] {
        let notice = fixture.notify(user, Some(repo), "ack-payload").await;
        let mut blocker = f.raw.begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout='3s'")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
            .bind(repo)
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        let tasks: Vec<_> = (0..2)
            .map(|_| {
                let pool = f.pool.clone();
                let original = original.clone();
                tokio::spawn(async move {
                    db::notifications::records::notification_inbox(
                        &pool,
                        &original,
                        user,
                        if read_all {
                            Action::ReadAll
                        } else {
                            Action::Read {
                                notification_id: notice,
                            }
                        },
                    )
                    .await
                })
            })
            .collect();
        tokio::time::timeout(DEADLINE, async {
            loop {
                let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid)) AND query LIKE '%repositories%'")
                    .bind(pid).fetch_one(&f.raw).await.unwrap();
                if waiting >= 2 { break; }
                assert!(tasks.iter().all(|task| !task.is_finished()), "both acks must wait behind the repository");
                tokio::task::yield_now().await;
            }
        }).await.expect("both acknowledgments wait without locking the notice first");
        blocker.commit().await.unwrap();
        let mut updates = 0;
        for task in tasks {
            let result = tokio::time::timeout(DEADLINE, task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .unwrap();
            updates += result.updated;
            assert_eq!(result.unread_count, 0);
            if !read_all {
                assert_eq!(ids(&result.notifications), vec![notice]);
            }
        }
        assert_eq!(updates, 1, "only the first ack changes read_at");
        assert!(fixture
            .history(user)
            .await
            .iter()
            .any(|row| row["id"] == notice && !row["read_at"].is_null()));
    }
    fixture.close().await;
}

#[derive(Clone, Copy, Debug)]
enum ForkDeleteRead {
    List,
    Count,
    One,
    All,
    Delivery,
    InboxList,
    InboxOne,
    InboxAll,
}

#[tokio::test]
async fn postgres_notification_reads_do_not_lock_fork_notifications_before_the_fork() {
    let mut fixture = Fixture::new(true).await;
    let owner = fixture.actor("delete_owner").await;
    let user = fixture.actor("delete_recipient").await;
    let base = fixture.repo(owner, None, "delete_base").await;
    let original = session(fixture.pool(), user).await;
    let Fixture::Postgres(f) = &fixture else {
        unreachable!()
    };
    for (number, operation) in [
        ForkDeleteRead::List,
        ForkDeleteRead::Count,
        ForkDeleteRead::One,
        ForkDeleteRead::All,
        ForkDeleteRead::Delivery,
        ForkDeleteRead::InboxList,
        ForkDeleteRead::InboxOne,
        ForkDeleteRead::InboxAll,
    ]
    .into_iter()
    .enumerate()
    {
        let fork = fixture
            .repo(owner, None, &format!("delete_fork_{number}"))
            .await;
        fixture.execute("UPDATE repositories SET is_fork=TRUE,forked_from_id=$1,visibility_locked=TRUE WHERE id=$2", &[base, fork]).await;
        db::repositories::access::grant_repo_access(
            fixture.pool(),
            user,
            fork,
            db::models::repositories::AccessLevel::Read,
        )
        .await
        .unwrap();
        let notice = fixture
            .notify(user, Some(fork), &format!("fork-secret-{number}"))
            .await;
        assert_eq!(
            db::notifications::records::count_unread_notifications(fixture.pool(), user)
                .await
                .unwrap(),
            1
        );
        let before = fixture
            .history(user)
            .await
            .into_iter()
            .find(|row| row["id"] == notice)
            .unwrap();
        let mut deletion = f.raw.begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout='3s'")
            .execute(&mut *deletion)
            .await
            .unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *deletion)
            .await
            .unwrap();
        // Fork deletion locks the source before the fork: its forks_count update needs the
        // source, and a read that held the source first would otherwise deadlock with it.
        sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR NO KEY UPDATE")
            .bind(base)
            .fetch_one(&mut *deletion)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
            .bind(fork)
            .fetch_one(&mut *deletion)
            .await
            .unwrap();
        let pool = f.pool.clone();
        let original = original.clone();
        let task = tokio::spawn(async move {
            let hidden = match operation {
                ForkDeleteRead::List => {
                    db::notifications::records::list_notifications_for_user(&pool, user, 100, false)
                        .await?
                        .is_empty()
                }
                ForkDeleteRead::Count => {
                    db::notifications::records::count_unread_notifications(&pool, user).await? == 0
                }
                ForkDeleteRead::One => {
                    db::notifications::records::mark_notification_read(&pool, user, notice)
                        .await?
                        .is_none()
                }
                ForkDeleteRead::All => {
                    db::notifications::records::mark_all_notifications_read(&pool, user).await? == 0
                }
                ForkDeleteRead::Delivery => db::notifications::records::notification_delivery(
                    &pool,
                    &original,
                    user,
                    Some(notice),
                )
                .await?
                .is_none_or(|delivery| {
                    delivery.notification.is_none() && delivery.unread_count == 0
                }),
                ForkDeleteRead::InboxList | ForkDeleteRead::InboxOne | ForkDeleteRead::InboxAll => {
                    let action = match operation {
                        ForkDeleteRead::InboxList => Action::List {
                            limit: 100,
                            filter: NotificationFilter::inbox(false),
                            before: None,
                        },
                        ForkDeleteRead::InboxOne => Action::Read {
                            notification_id: notice,
                        },
                        _ => Action::ReadAll,
                    };
                    let inbox = db::notifications::records::notification_inbox(
                        &pool, &original, user, action,
                    )
                    .await?
                    .unwrap();
                    inbox.notifications.is_empty() && inbox.unread_count == 0 && inbox.updated == 0
                }
            };
            Ok::<_, anyhow::Error>(hidden)
        });
        pg_waiter(&f.raw, pid, "FROM repositories").await;
        assert!(
            !task.is_finished(),
            "{operation:?} must wait behind fork deletion"
        );
        sqlx::query("DELETE FROM repositories WHERE id=$1")
            .bind(fork)
            .execute(&mut *deletion)
            .await
            .unwrap();
        deletion.commit().await.unwrap();
        assert!(
            tokio::time::timeout(DEADLINE, task)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            "{operation:?} must hide a deleted fork"
        );
        let after = fixture
            .history(user)
            .await
            .into_iter()
            .find(|row| row["id"] == notice)
            .unwrap();
        assert_eq!(after["repo"], serde_json::Value::Null);
        assert_eq!(after["scope"], fork);
        assert_eq!(after["read_at"], serde_json::Value::Null);
        for field in [
            "title",
            "body",
            "link",
            "metadata",
            "recipient",
            "created_at",
        ] {
            assert_eq!(
                after[field], before[field],
                "{operation:?} changed stored {field}"
            );
        }
        assert!(
            db::notifications::records::mark_notification_read(fixture.pool(), user, notice)
                .await
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(
        db::notifications::records::count_unread_notifications(fixture.pool(), user)
            .await
            .unwrap(),
        0
    );
    fixture.close().await;
}

#[tokio::test]
async fn postgres_all_inbox_actions_wait_for_recipient_then_observe_original_session_deletion() {
    let mut fixture = Fixture::new(true).await;
    let user = fixture.actor("inbox_pg_waiter").await;
    let notice = fixture.notify(user, None, "pg-waiter-account").await;
    for action in actions(notice) {
        let original = session(fixture.pool(), user).await;
        let replacement = session(fixture.pool(), user).await;
        let before = fixture.history(user).await;
        let Fixture::Postgres(f) = &fixture else {
            unreachable!()
        };
        let mut revoker = f.raw.begin().await.unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        // The recipient's account row is the first thing every inbox action locks.
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
            .bind(user)
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        let pool = f.pool.clone();
        let request_session = original.clone();
        let task = tokio::spawn(async move {
            db::notifications::records::notification_inbox(&pool, &request_session, user, action)
                .await
        });
        pg_waiter(&f.raw, blocker, "FROM users").await;
        assert!(!task.is_finished());
        // While waiting for the recipient, no notification or session lock is held.
        sqlx::query("SELECT id FROM notifications WHERE id=$1 FOR UPDATE NOWAIT")
            .bind(notice)
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR UPDATE NOWAIT")
            .bind(&original)
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        sqlx::query("DELETE FROM sessions WHERE id=$1")
            .bind(&original)
            .execute(&mut *revoker)
            .await
            .unwrap();
        revoker.commit().await.unwrap();
        assert!(tokio::time::timeout(DEADLINE, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_none());
        assert_eq!(fixture.history(user).await, before);
        assert!(
            db::users::sessions::get_session(fixture.pool(), &replacement)
                .await
                .unwrap()
                .is_some()
        );
    }
    fixture.close().await;
}

#[tokio::test]
async fn postgres_inbox_final_expiry_rolls_back_staged_one_and_all_acknowledgments() {
    let mut fixture = Fixture::new(true).await;
    let user = fixture.actor("inbox_pg_expiry").await;
    let notice = fixture.notify(user, None, "pg-expiry-account").await;
    let original = session(fixture.pool(), user).await;
    let before = fixture.history(user).await;
    let Fixture::Postgres(f) = &fixture else {
        unreachable!()
    };
    // Fixture-only AFTER UPDATE barrier proves read_at has been staged before
    // clock expiry, rather than merely delaying initial session admission.
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&f.raw)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE FUNCTION inbox_ack_pause() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock({blocker},NEW.id::integer); RETURN NEW; END; $$"))
        .execute(&f.raw).await.unwrap();
    sqlx::query("CREATE TRIGGER inbox_ack_pause AFTER UPDATE ON notifications FOR EACH ROW EXECUTE FUNCTION inbox_ack_pause()")
        .execute(&f.raw).await.unwrap();
    for action in [
        Action::Read {
            notification_id: notice,
        },
        Action::ReadAll,
    ] {
        let mut blocker_tx = f.raw.begin().await.unwrap();
        // The actual backend may differ on reacquisition; the advisory key is
        // fixed to this fixture's earlier owned backend ID, not global test data.
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *blocker_tx)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1,$2)")
            .bind(blocker)
            .bind(i32::try_from(notice).unwrap())
            .execute(&mut *blocker_tx)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE sessions SET expires_at=clock_timestamp()+interval '5 seconds' WHERE id=$1",
        )
        .bind(&original)
        .execute(&f.raw)
        .await
        .unwrap();
        let pool = f.pool.clone();
        let request_session = original.clone();
        let task = tokio::spawn(async move {
            db::notifications::records::notification_inbox(&pool, &request_session, user, action)
                .await
        });
        pg_waiter(&f.raw, pid, "UPDATE notifications").await;
        assert!(!task.is_finished());
        // Original session remains locked FOR SHARE through the staged ack.
        let mut probe = f.raw.begin().await.unwrap();
        let error = sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR UPDATE NOWAIT")
            .bind(&original)
            .fetch_one(&mut *probe)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("55P03")
        );
        probe.rollback().await.unwrap();
        expired(&fixture, user, true).await;
        blocker_tx.commit().await.unwrap();
        assert!(tokio::time::timeout(DEADLINE, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_none());
        assert_eq!(
            fixture.history(user).await,
            before,
            "expired staged acknowledgment rolls back"
        );
    }
    fixture.close().await;
}

// SQLx 0.8.6 exposes the locked native handle but not sqlite3_busy_handler.
// This fixture-only ABI uses the same linked SQLite as SQLx; no new dependency.
extern "C" {
    fn sqlite3_trace_v2(
        database: *mut c_void,
        mask: u32,
        callback: Option<unsafe extern "C" fn(u32, *mut c_void, *mut c_void, *mut c_void) -> c_int>,
        argument: *mut c_void,
    ) -> c_int;
    fn sqlite3_busy_handler(
        database: *mut c_void,
        callback: Option<unsafe extern "C" fn(*mut c_void, c_int) -> c_int>,
        argument: *mut c_void,
    ) -> c_int;
}

// A real native callback observes failed writer admission (or an actual update),
// then awaits the test's explicit release. Deadlines bound abandoned callbacks.
struct NativePause {
    observed: tokio::sync::Notify,
    released: Mutex<bool>,
    release: Condvar,
}
impl NativePause {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            observed: tokio::sync::Notify::new(),
            released: Mutex::new(false),
            release: Condvar::new(),
        })
    }
    fn pause(&self) -> bool {
        self.observed.notify_one();
        let Ok(released) = self.released.lock() else {
            return false;
        };
        self.release
            .wait_timeout_while(released, DEADLINE, |released| !*released)
            .is_ok_and(|(released, _)| *released)
    }
    fn resume(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}
unsafe extern "C" fn writer_busy(argument: *mut c_void, _: c_int) -> c_int {
    // SAFETY: installed under SQLx's locked handle; its disabled progress handler
    // owns an Arc until connection shutdown. The callback never uses the DB handle.
    let pause = unsafe { &*argument.cast::<NativePause>() };
    c_int::from(pause.pause())
}

async fn sqlite_request_pool(url: &str) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::from_str(url)
                .unwrap()
                .with_regexp()
                .foreign_keys(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .busy_timeout(DEADLINE),
        )
        .await
        .unwrap()
}

// A disabled progress handler owns the counters until connection teardown, including unwind.
struct Trace {
    decisions: std::sync::atomic::AtomicUsize,
    candidates: std::sync::atomic::AtomicUsize,
}

unsafe extern "C" fn sql_trace(
    _: u32,
    argument: *mut c_void,
    _: *mut c_void,
    sql: *mut c_void,
) -> c_int {
    // SQLITE_TRACE_STMT supplies a NUL-terminated SQL string for this callback.
    let sql = unsafe { std::ffi::CStr::from_ptr(sql.cast()) }.to_bytes();
    // SAFETY: the connection's progress handler retains the heap allocation.
    let trace = unsafe { &*argument.cast::<Trace>() };
    if sql.starts_with(b"SELECT s.scope, EXISTS(") {
        trace
            .decisions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    } else if sql.starts_with(b"SELECT id,scope_repository_id,created_at FROM notifications") {
        trace
            .candidates
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    0
}

/// Statements the operation ran on SQLite: the repository decisions of its transaction
/// (`decisions`, one set-based statement) and candidate pages (`candidates`). PostgreSQL
/// repository lookups do not reach sqlx's statement log in this harness, so a PostgreSQL run
/// returns None and still checks every result.
struct Queries {
    decisions: usize,
    candidates: usize,
}

async fn scope_queries(
    pool: &db::DbPool,
    operation: impl std::future::Future<Output = ()>,
) -> Option<Queries> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let db::DbPool::Sqlite(raw) = pool else {
        operation.await;
        return None;
    };
    let trace = Arc::new(Trace {
        decisions: AtomicUsize::new(0),
        candidates: AtomicUsize::new(0),
    });
    {
        let mut connection = raw.acquire().await.unwrap();
        let mut handle = connection.lock_handle().await.unwrap();
        let retained = trace.clone();
        handle.set_progress_handler(0, move || {
            let _ = &retained;
            true
        });
        // SAFETY: the locked connection retains `trace` even if the operation is cancelled.
        assert_eq!(
            unsafe {
                sqlite3_trace_v2(
                    handle.as_raw_handle().as_ptr().cast(),
                    1,
                    Some(sql_trace),
                    Arc::as_ptr(&trace).cast_mut().cast(),
                )
            },
            0
        );
    }
    operation.await;
    let mut connection = raw.acquire().await.unwrap();
    let mut handle = connection.lock_handle().await.unwrap();
    // SAFETY: no operation is active; remove tracing before the counters are read.
    assert_eq!(
        unsafe {
            sqlite3_trace_v2(
                handle.as_raw_handle().as_ptr().cast(),
                0,
                None,
                std::ptr::null_mut(),
            )
        },
        0
    );
    Some(Queries {
        decisions: trace.decisions.load(Ordering::SeqCst),
        candidates: trace.candidates.load(Ordering::SeqCst),
    })
}

#[tokio::test]
async fn sqlite_trace_survives_operation_panic_and_cancellation() {
    for panic in [false, true] {
        let fixture = crate::support::native_database::NativeDatabase::new().await;
        let pool = fixture.pool.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            scope_queries(&pool, async {
                started.send(()).unwrap();
                if panic {
                    panic!("fixture operation panic");
                }
                std::future::pending::<()>().await;
            })
            .await;
        });
        ready.await.unwrap();
        if !panic {
            task.abort();
        }
        assert!(task.await.is_err());
        // The callback is still installed. It must not refer to the dropped future's stack.
        let db::DbPool::Sqlite(raw) = &fixture.pool else {
            unreachable!()
        };
        sqlx::query("SELECT 1").execute(raw).await.unwrap();
        fixture.close().await;
    }
}

async fn grouped_scope_work(pg: bool) {
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
    let pool = match &fixture {
        Fixture::Postgres(f) => f.pool.clone(),
        Fixture::Sqlite(f) => sqlite_request_pool(&f.url).await.into(),
    };
    if let Some(queries) = scope_queries(&pool, async {
        assert_eq!(
            db::notifications::records::count_unread_notifications(&pool, user)
                .await
                .unwrap(),
            256
        );
    })
    .await
    {
        assert_eq!(
            queries.decisions, 1,
            "one decision for the transaction, not one per scope or notification"
        );
    }
    for action in [
        Action::List {
            limit: 1,
            filter: NotificationFilter::inbox(false),
            before: None,
        },
        Action::ReadAll,
    ] {
        let listing = matches!(&action, Action::List { .. });
        if let Some(queries) = scope_queries(&pool, async {
            let result =
                db::notifications::records::notification_inbox(&pool, &original, user, action)
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(result.notifications.len(), usize::from(listing));
            assert_eq!(result.unread_count, if listing { 256 } else { 0 });
            assert_eq!(result.updated, if listing { 0 } else { 256 });
        })
        .await
        {
            assert_eq!(
                queries.decisions, 1,
                "list/ack and count share one decision"
            );
        }
    }
    let broadcaster = codeyard::backend::notifications::NotificationBroadcaster::new(&pool);
    let mut events = broadcaster.subscribe(user);
    if let Some(queries) = scope_queries(&pool, async {
        codeyard::backend::notifications::create::create_and_broadcast_notification(
            &pool,
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
    })
    .await
    {
        assert_eq!(
            queries.decisions, 0,
            "creation must not authorize/count the backlog"
        );
    }
    let event = events.try_recv().unwrap();
    let current = codeyard::backend::notifications::current_event(&pool, &original, user, &event)
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
    pool.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_notification_work_is_grouped_by_scope() {
    grouped_scope_work(true).await;
}
#[tokio::test]
async fn sqlite_notification_work_is_grouped_by_scope() {
    grouped_scope_work(false).await;
}

// The privacy change locks the source and then one fork. The source has no notification,
// so only the ancestor lock keeps the inbox from holding the other fork while it waits.
#[tokio::test]
async fn postgres_inbox_locks_ancestors_before_forks_without_source_notification() {
    let mut fixture = Fixture::new(true).await;
    let owner = fixture.actor("ancestor_order_owner").await;
    let user = fixture.actor("ancestor_order_reader").await;
    let original = session(fixture.pool(), user).await;
    let source = fixture.repo(owner, None, "ancestor_order_source").await;
    let fork_a = fixture.repo(owner, None, "ancestor_order_fork_a").await;
    let fork_b = fixture.repo(owner, None, "ancestor_order_fork_b").await;
    fixture
        .execute(
            "UPDATE repositories SET is_private=FALSE WHERE id=$1 OR id=$2 OR id=$3",
            &[source, fork_a, fork_b],
        )
        .await;
    fixture
        .execute(
            "UPDATE repositories SET is_fork=TRUE,forked_from_id=$1 WHERE id=$2 OR id=$3",
            &[source, fork_a, fork_b],
        )
        .await;
    fixture.notify(user, Some(fork_a), "fork-a").await;
    fixture.notify(user, Some(fork_b), "fork-b").await;
    let Fixture::Postgres(f) = &fixture else {
        unreachable!()
    };
    let mut privacy = f.raw.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *privacy)
        .await
        .unwrap();
    // The change locks the source first, then the fork the trigger reaches first.
    for repo in [source, fork_b] {
        sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
            .bind(repo)
            .fetch_one(&mut *privacy)
            .await
            .unwrap();
    }
    let pool = f.pool.clone();
    let task = tokio::spawn(async move {
        db::notifications::records::notification_inbox(
            &pool,
            &original,
            user,
            Action::List {
                limit: 100,
                filter: NotificationFilter::inbox(false),
                before: None,
            },
        )
        .await
    });
    pg_waiter(&f.raw, pid, "FROM repositories").await;
    // The trigger then narrows the fork that the inbox has not locked.
    sqlx::query("UPDATE repositories SET is_private=TRUE WHERE id=$1")
        .bind(source)
        .execute(&mut *privacy)
        .await
        .unwrap();
    privacy.commit().await.unwrap();
    let result = tokio::time::timeout(DEADLINE, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(result.notifications.is_empty());
    assert_eq!((result.unread_count, result.updated), (0, 0));
    fixture.close().await;
}

// A one-row page authorizes only what its walk reaches, and a hidden tail is skipped by
// repository instead of being read batch by batch.
async fn page_walk_work(pg: bool) {
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
    let pool = match &fixture {
        Fixture::Postgres(f) => f.pool.clone(),
        Fixture::Sqlite(f) => sqlite_request_pool(&f.url).await.into(),
    };
    let queries = scope_queries(&pool, async {
        let result = db::notifications::records::notification_inbox(
            &pool,
            &original,
            user,
            Action::List {
                limit: 1,
                filter: NotificationFilter::inbox(false),
                before: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.notifications.len(), 1);
        assert_eq!(result.notifications[0].id, newest);
        assert!(result.next_cursor.is_some());
        assert_eq!(result.unread_count, 1);
    })
    .await;
    if let Some(queries) = queries {
        assert_eq!(
            queries.decisions, 1,
            "the page walk decides every repository in one statement"
        );
        assert!(
            queries.candidates <= 3,
            "hidden history is skipped by repository, not batch by batch: {}",
            queries.candidates
        );
    }
    pool.close().await;
    fixture.close().await;
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
    fixture.close().await;
}

#[tokio::test]
async fn postgres_done_marks_are_announced() {
    done_marks_are_announced(true).await;
}
#[tokio::test]
async fn sqlite_done_marks_are_announced() {
    done_marks_are_announced(false).await;
}

#[tokio::test]
async fn postgres_notification_page_walk_authorizes_only_what_it_reads() {
    page_walk_work(true).await;
}
#[tokio::test]
async fn sqlite_notification_page_walk_authorizes_only_what_it_reads() {
    page_walk_work(false).await;
}

// A team change locks its organization, then the repository it changes. The read reaches
// the org-owned source through the personal fork first; it must take the organization
// before the source, or the change waits on the source while the read waits on the
// organization. Each round fails with a deadlock or a timeout rather than hanging.
#[tokio::test]
async fn postgres_inbox_locks_source_organization_before_source_rows() {
    let mut fixture = Fixture::new(true).await;
    let owner = fixture.actor("org_order_owner").await;
    for round in 0..2 {
        // A fresh reader per round, so earlier rounds' notifications stay out of the page.
        let user = fixture.actor(&format!("org_order_reader_{round}")).await;
        let org = fixture
            .scalar(
                &format!(
                    "INSERT INTO organizations(name) VALUES('org_order_{round}') RETURNING id"
                ),
                &[],
            )
            .await;
        fixture
            .execute(
                "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')",
                &[org, owner],
            )
            .await;
        let source = fixture
            .repo(owner, Some(org), &format!("org_order_source_{round}"))
            .await;
        let fork = fixture
            .repo(user, None, &format!("org_order_fork_{round}"))
            .await;
        fixture
            .execute(
                "UPDATE repositories SET is_fork=TRUE,forked_from_id=$1 WHERE id=$2",
                &[source, fork],
            )
            .await;
        fixture.notify(user, Some(source), "source-note").await;
        fixture.notify(user, Some(fork), "fork-note").await;
        let original = session(fixture.pool(), user).await;
        let Fixture::Postgres(f) = &fixture else {
            unreachable!()
        };
        let mut change = f.raw.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *change)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
            .bind(org)
            .fetch_one(&mut *change)
            .await
            .unwrap();
        let pool = f.pool.clone();
        let task = tokio::spawn(async move {
            db::notifications::records::notification_inbox(
                &pool,
                &original,
                user,
                Action::List {
                    limit: 100,
                    filter: NotificationFilter::inbox(false),
                    before: None,
                },
            )
            .await
        });
        pg_waiter(&f.raw, pid, "FROM organizations").await;
        sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
            .bind(source)
            .fetch_one(&mut *change)
            .await
            .unwrap();
        change.commit().await.unwrap();
        let result = tokio::time::timeout(DEADLINE, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.notifications.len(), 1);
        assert_eq!(result.notifications[0].title, "fork-note");
        assert_eq!((result.unread_count, result.updated), (1, 0));
    }
    fixture.close().await;
}

#[tokio::test]
async fn sqlite_all_inbox_actions_wait_reserved_writer_then_observe_original_session_deletion() {
    let mut fixture = Fixture::new(false).await;
    let user = fixture.actor("inbox_sqlite_waiter").await;
    let notice = fixture.notify(user, None, "sqlite-waiter-account").await;
    let Fixture::Sqlite(f) = &fixture else {
        unreachable!()
    };
    let raw = sqlite_request_pool(&f.url).await;
    let pool: db::DbPool = raw.clone().into();
    for action in actions(notice) {
        let original = session(fixture.pool(), user).await;
        let replacement = session(fixture.pool(), user).await;
        let before = fixture.history(user).await;
        let pause = NativePause::new();
        {
            let mut connection = raw.acquire().await.unwrap();
            let mut handle = connection.lock_handle().await.unwrap();
            let keep_alive = pause.clone();
            handle.set_progress_handler(0, move || {
                let _ = &keep_alive;
                true
            });
            // SAFETY: native SQLite handle is exclusively locked, and the
            // connection-owned Arc outlives all callbacks, including cancellation.
            assert_eq!(
                unsafe {
                    sqlite3_busy_handler(
                        handle.as_raw_handle().as_ptr().cast(),
                        Some(writer_busy),
                        Arc::as_ptr(&pause).cast_mut().cast(),
                    )
                },
                0
            );
        }
        let mut revoker = f.raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let request_pool = pool.clone();
        let request_session = original.clone();
        let task = tokio::spawn(async move {
            db::notifications::records::notification_inbox(
                &request_pool,
                &request_session,
                user,
                action,
            )
            .await
        });
        tokio::time::timeout(DEADLINE, pause.observed.notified())
            .await
            .expect("real SQLite busy callback, not a sleep or pool waiter");
        assert!(!task.is_finished());
        sqlx::query("DELETE FROM sessions WHERE id=?1")
            .bind(&original)
            .execute(&mut *revoker)
            .await
            .unwrap();
        revoker.commit().await.unwrap();
        pause.resume();
        assert!(tokio::time::timeout(DEADLINE, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_none());
        assert_eq!(fixture.history(user).await, before);
        assert!(
            db::users::sessions::get_session(fixture.pool(), &replacement)
                .await
                .unwrap()
                .is_some()
        );
        let mut connection = raw.acquire().await.unwrap();
        let mut handle = connection.lock_handle().await.unwrap();
        // SAFETY: no request is active; clear the native callback before releasing
        // its connection-owned lifetime anchor.
        assert_eq!(
            unsafe {
                sqlite3_busy_handler(
                    handle.as_raw_handle().as_ptr().cast(),
                    None,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        handle.remove_progress_handler();
    }
    pool.close().await;
    raw.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn sqlite_inbox_final_expiry_rolls_back_staged_one_and_all_acknowledgments() {
    let mut fixture = Fixture::new(false).await;
    let user = fixture.actor("inbox_sqlite_expiry").await;
    let notice = fixture.notify(user, None, "sqlite-expiry-account").await;
    let original = session(fixture.pool(), user).await;
    let before = fixture.history(user).await;
    let Fixture::Sqlite(f) = &fixture else {
        unreachable!()
    };
    let raw = sqlite_request_pool(&f.url).await;
    let pool: db::DbPool = raw.clone().into();
    for action in [
        Action::Read {
            notification_id: notice,
        },
        Action::ReadAll,
    ] {
        let pause = NativePause::new();
        {
            let mut connection = raw.acquire().await.unwrap();
            let mut handle = connection.lock_handle().await.unwrap();
            let barrier = pause.clone();
            handle.set_update_hook(move |update| {
                if update.table == "notifications"
                    && update.rowid == notice
                    && update.operation == sqlx::sqlite::SqliteOperation::Update
                {
                    // Actual native UPDATE has occurred, but cannot yet commit.
                    let _ = barrier.pause();
                }
            });
        }
        sqlx::query("UPDATE sessions SET expires_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now','+5 seconds') WHERE id=?1")
            .bind(&original).execute(&f.raw).await.unwrap();
        let request_pool = pool.clone();
        let request_session = original.clone();
        let task = tokio::spawn(async move {
            db::notifications::records::notification_inbox(
                &request_pool,
                &request_session,
                user,
                action,
            )
            .await
        });
        tokio::time::timeout(DEADLINE, pause.observed.notified())
            .await
            .expect("actual staged native SQLite ack");
        assert!(!task.is_finished());
        assert_eq!(
            fixture.history(user).await,
            before,
            "WAL reader cannot see provisional read_at"
        );
        expired(&fixture, user, false).await;
        pause.resume();
        assert!(tokio::time::timeout(DEADLINE, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_none());
        assert_eq!(
            fixture.history(user).await,
            before,
            "expired staged acknowledgment rolls back"
        );
        let mut connection = raw.acquire().await.unwrap();
        let mut handle = connection.lock_handle().await.unwrap();
        handle.remove_update_hook();
    }
    pool.close().await;
    raw.close().await;
    fixture.close().await;
}
