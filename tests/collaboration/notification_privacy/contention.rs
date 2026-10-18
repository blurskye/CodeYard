use super::{db, Fixture};
use std::{str::FromStr, time::Duration};

#[derive(Clone, Copy, Debug)]
enum Operation {
    List,
    Count,
    One,
    All,
    Delivery,
}
const OPERATIONS: [Operation; 5] = [
    Operation::List,
    Operation::Count,
    Operation::One,
    Operation::All,
    Operation::Delivery,
];

async fn wait_for_lock(raw: &sqlx::PgPool, blocker: i32) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid)))",
            )
            .bind(blocker)
            .fetch_one(raw)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observe real lock waiter, no sleep-based inference");
}

async fn wait_for_lock_chain(raw: &sqlx::PgPool, blocker: i32, waiters: i64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "WITH RECURSIVE waiting(pid) AS (
                    SELECT pid FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid))
                    UNION
                    SELECT a.pid FROM pg_stat_activity a JOIN waiting w ON w.pid=ANY(pg_blocking_pids(a.pid))
                ) SELECT COUNT(*) FROM waiting",
            )
            .bind(blocker)
            .fetch_one(raw)
            .await
            .unwrap();
            if waiting >= waiters {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual transitive notification lock wait edges");
}

async fn invisible(
    pool: &db::DbPool,
    recipient: i64,
    notification: i64,
    op: Operation,
) -> anyhow::Result<bool> {
    Ok(match op {
        Operation::List => {
            db::notifications::records::list_notifications_for_user(pool, recipient, 100, false)
                .await?
                .is_empty()
        }
        Operation::Count => {
            db::notifications::records::count_unread_notifications(pool, recipient).await? == 0
        }
        Operation::One => {
            db::notifications::records::mark_notification_read(pool, recipient, notification)
                .await?
                .is_none()
        }
        Operation::All => {
            db::notifications::records::mark_all_notifications_read(pool, recipient).await? == 0
        }
        Operation::Delivery => db::notifications::records::notification_delivery(
            pool,
            &"00".repeat(32),
            recipient,
            Some(notification),
        )
        .await?
        .is_none_or(|p| p.notification.is_none()),
    })
}

#[tokio::test]
async fn postgres_notification_operations_wait_for_recipient_row_then_recheck() {
    let mut fixture = Fixture::new(true).await;
    let owner = fixture.actor("notification_lock_owner").await;
    let recipient = fixture.actor("notification_lock_recipient").await;
    let repo = fixture.repo(owner, None, "notification_lock_repo").await;
    let notice = fixture
        .notify(recipient, Some(repo), "lock-private-payload")
        .await;
    db::users::sessions::create_session(
        fixture.pool(),
        &"00".repeat(32),
        recipient,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let Fixture::Postgres(f) = &fixture else {
        unreachable!()
    };
    for op in OPERATIONS {
        db::repositories::access::grant_repo_access(
            &f.pool,
            recipient,
            repo,
            db::models::repositories::AccessLevel::Read,
        )
        .await
        .unwrap();
        let before = fixture.history(recipient).await;
        let mut revoker = f.raw.begin().await.unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        // The recipient's account row is the first thing every notification
        // operation locks.
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
            .bind(recipient)
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        let pool = f.pool.clone();
        let task = tokio::spawn(async move { invisible(&pool, recipient, notice, op).await });
        wait_for_lock(&f.raw, blocker).await;
        assert!(!task.is_finished(), "operation must wait: {op:?}");
        // Prove no repository or notification row was acquired while waiting.
        for (table, id) in [
            ("users", recipient),
            ("repositories", repo),
            ("notifications", notice),
        ] {
            sqlx::query(&format!(
                "SELECT id FROM {table} WHERE id=$1 FOR UPDATE NOWAIT"
            ))
            .bind(id)
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        }
        sqlx::query("DELETE FROM repo_access WHERE repo_id=$1 AND user_id=$2")
            .bind(repo)
            .bind(recipient)
            .execute(&mut *revoker)
            .await
            .unwrap();
        revoker.commit().await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap());
        assert_eq!(
            fixture.history(recipient).await,
            before,
            "revoked {op:?} cannot ack history"
        );
    }
    fixture.close().await;
}

#[tokio::test]
async fn postgres_notification_operations_wait_for_uncommitted_grant_revocation() {
    let mut fixture = Fixture::new(true).await;
    let owner = fixture.actor("notification_grant_owner").await;
    let recipient = fixture.actor("notification_grant_recipient").await;
    let repo = fixture.repo(owner, None, "notification_grant_repo").await;
    let notice = fixture
        .notify(recipient, Some(repo), "grant-private-payload")
        .await;
    let Fixture::Postgres(f) = &fixture else {
        unreachable!()
    };
    // Delivery returns early without a live session, so the probe needs one.
    db::users::sessions::create_session(
        &f.pool,
        &"00".repeat(32),
        recipient,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();

    for op in OPERATIONS {
        db::repositories::access::grant_repo_access(
            &f.pool,
            recipient,
            repo,
            db::models::repositories::AccessLevel::Read,
        )
        .await
        .unwrap();
        fixture
            .execute(
                "UPDATE notifications SET read_at=NULL WHERE id=$1 AND user_id=$2",
                &[notice, recipient],
            )
            .await;
        let before = fixture.history(recipient).await;
        let mut revoke = f.raw.begin().await.unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *revoke)
            .await
            .unwrap();
        sqlx::query("DELETE FROM repo_access WHERE repo_id=$1 AND user_id=$2")
            .bind(repo)
            .bind(recipient)
            .execute(&mut *revoke)
            .await
            .unwrap();
        let pool = f.pool.clone();
        let task = tokio::spawn(async move { invisible(&pool, recipient, notice, op).await });
        wait_for_lock(&f.raw, blocker).await;
        revoke.commit().await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap());
        assert_eq!(fixture.history(recipient).await, before);
    }
    fixture.close().await;
}

#[tokio::test]
async fn postgres_notification_write_keeps_its_grant_until_commit() {
    let mut fixture = Fixture::new(true).await;
    let owner = fixture.actor("notification_commit_owner").await;
    let recipient = fixture.actor("notification_commit_recipient").await;
    let repo = fixture.repo(owner, None, "notification_commit_repo").await;
    db::repositories::access::grant_repo_access(
        fixture.pool(),
        recipient,
        repo,
        db::models::repositories::AccessLevel::Read,
    )
    .await
    .unwrap();
    let notice = fixture
        .notify(recipient, Some(repo), "commit-private-payload")
        .await;
    let Fixture::Postgres(f) = &fixture else {
        unreachable!()
    };

    let mut notification = f.raw.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *notification)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM notifications WHERE id=$1 FOR UPDATE")
        .bind(notice)
        .fetch_one(&mut *notification)
        .await
        .unwrap();
    let pool = f.pool.clone();
    let effect = tokio::spawn(async move {
        db::notifications::records::mark_notification_read(&pool, recipient, notice).await
    });
    wait_for_lock(&f.raw, blocker).await;

    let pool = f.pool.clone();
    let revocation = tokio::spawn(async move {
        db::repositories::access::revoke_repo_access(&pool, recipient, repo).await
    });
    wait_for_lock_chain(&f.raw, blocker, 2).await;
    assert!(!revocation.is_finished());
    notification.commit().await.unwrap();

    assert_eq!(effect.await.unwrap().unwrap().unwrap().id, notice);
    assert!(revocation.await.unwrap().unwrap());
    assert!(db::notifications::records::list_notifications_for_user(
        fixture.pool(),
        recipient,
        100,
        false,
    )
    .await
    .unwrap()
    .is_empty());
    assert!(fixture.history(recipient).await[0]["read_at"].is_string());
    fixture.close().await;
}

#[tokio::test]
async fn sqlite_notification_operations_require_reserved_writer_then_recheck() {
    let mut fixture = Fixture::new(false).await;
    let owner = fixture.actor("notification_reserved_owner").await;
    let recipient = fixture.actor("notification_reserved_recipient").await;
    let repo = fixture
        .repo(owner, None, "notification_reserved_repo")
        .await;
    let notice = fixture
        .notify(recipient, Some(repo), "reserved-private-payload")
        .await;
    db::users::sessions::create_session(
        fixture.pool(),
        &"00".repeat(32),
        recipient,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        fixture.pool(),
        recipient,
        repo,
        db::models::repositories::AccessLevel::Read,
    )
    .await
    .unwrap();
    let Fixture::Sqlite(f) = &fixture else {
        unreachable!()
    };
    // One connection: a refused operation rolls back in the background, and with
    // no busy timeout the next one would see that lock still held on another one.
    let raw = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::from_str(&f.url)
                .unwrap()
                .with_regexp()
                .foreign_keys(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .busy_timeout(Duration::ZERO),
        )
        .await
        .unwrap();
    let second: db::DbPool = raw.clone().into();
    let before = fixture.history(recipient).await;
    let mut revoker = f.raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
    for op in OPERATIONS {
        let error = invisible(&second, recipient, notice, op)
            .await
            .expect_err("must actually contend");
        let driver = error
            .downcast_ref::<sqlx::Error>()
            .expect("native SQLite error");
        assert_eq!(
            driver.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("5"),
            "observed SQLITE_BUSY for {op:?}"
        );
        assert_eq!(fixture.history(recipient).await, before);
    }
    sqlx::query("DELETE FROM repo_access WHERE repo_id=?1 AND user_id=?2")
        .bind(repo)
        .bind(recipient)
        .execute(&mut *revoker)
        .await
        .unwrap();
    revoker.commit().await.unwrap();
    for op in OPERATIONS {
        assert!(invisible(&second, recipient, notice, op).await.unwrap());
        assert_eq!(fixture.history(recipient).await, before);
    }
    second.close().await;
    raw.close().await;
    fixture.close().await;
}

async fn missing_table_is_error(pg: bool) {
    let mut fixture = Fixture::new(pg).await;
    let recipient = fixture.actor("notification_error_recipient").await;
    let notice = fixture.notify(recipient, None, "owned-error-fixture").await;
    db::users::sessions::create_session(
        fixture.pool(),
        &"00".repeat(32),
        recipient,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    fixture.execute("DROP TABLE notifications", &[]).await;
    for op in OPERATIONS {
        let error = invisible(fixture.pool(), recipient, notice, op)
            .await
            .expect_err("unknown SQL failure must propagate");
        assert!(
            error.downcast_ref::<sqlx::Error>().is_some(),
            "no fabricated zero/success"
        );
    }
    fixture.close().await;
}

#[tokio::test]
async fn postgres_notification_sql_failure_is_not_empty_success() {
    missing_table_is_error(true).await;
}
#[tokio::test]
async fn sqlite_notification_sql_failure_is_not_empty_success() {
    missing_table_is_error(false).await;
}

/// Deletes `fork` while a notification read of `reader` runs, and returns the deleted id and
/// the read's answer. The test holds a row lock on the fork, so the deletion queues behind it,
/// and the read starts only once the deletion is queued. Releasing the holder then lets both
/// finish, or deadlocks if the read holds a source lock that the deletion needs.
async fn delete_fork_during_read(
    pool: &db::DbPool,
    raw: &sqlx::PgPool,
    owner: i64,
    reader: i64,
    fork: i64,
    acknowledge: bool,
) -> (i64, i64) {
    let mut holder = raw.begin().await.unwrap();
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR NO KEY UPDATE")
        .bind(fork)
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    let deletion_pool = pool.clone();
    let deletion = tokio::spawn(async move {
        db::repositories::records::delete_repository(&deletion_pool, owner, fork).await
    });
    wait_for_lock(raw, holder_pid).await;
    let deletion_pid: i32 = sqlx::query_scalar(
        "SELECT pid FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid))",
    )
    .bind(holder_pid)
    .fetch_one(raw)
    .await
    .unwrap();
    let read_pool = pool.clone();
    let read = tokio::spawn(async move {
        if acknowledge {
            db::notifications::records::mark_all_notifications_read(&read_pool, reader)
                .await
                .map(|count| count as i64)
        } else {
            db::notifications::records::count_unread_notifications(&read_pool, reader).await
        }
    });
    wait_for_lock(raw, deletion_pid).await;
    holder.commit().await.unwrap();
    let deleted = tokio::time::timeout(Duration::from_secs(5), deletion)
        .await
        .expect("fork deletion finishes")
        .unwrap()
        .expect("fork deletion must not abort");
    let answer = tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("notification read finishes")
        .unwrap()
        .expect("notification read must not abort");
    (deleted.id, answer)
}

/// Fork deletion and notification reads lock a source before its fork, in every path. Each
/// round forks a source, gives the reader an unread notification on the source and on the
/// fork, and deletes the fork while the reader counts or acknowledges. Reading the source's
/// notifications first is the order that deadlocked with a deletion that locked the fork first.
/// A deadlock aborts the round, and the answer is exact: only the source's notice remains.
#[tokio::test]
async fn postgres_fork_deletion_and_notification_reads_take_the_source_first() {
    let mut fixture = Fixture::new(true).await;
    let owner = fixture.actor("fork_order_owner").await;
    let Fixture::Postgres(f) = &fixture else {
        unreachable!()
    };
    for round in 0..100 {
        for acknowledge in [false, true] {
            // A reader per case: notices that an earlier case left unread would be counted.
            let reader = fixture
                .actor(&format!("fork_order_reader_{round}_{acknowledge}"))
                .await;
            let source = fixture
                .repo(owner, None, &format!("order_source_{round}_{acknowledge}"))
                .await;
            let fork = db::repositories::discovery::create_forked_repository(
                fixture.pool(),
                db::repositories::discovery::NewForkedRepository {
                    owner_id: owner,
                    name: &format!("order_fork_{round}_{acknowledge}"),
                    description: None,
                    is_private: true,
                    disk_path: "/owned/not-executed.git",
                    forked_from: source,
                    default_branch: "main",
                },
            )
            .await
            .unwrap()
            .id;
            for repo in [source, fork] {
                db::repositories::access::grant_repo_access(
                    fixture.pool(),
                    reader,
                    repo,
                    db::models::repositories::AccessLevel::Read,
                )
                .await
                .unwrap();
            }
            fixture
                .notify(reader, Some(source), "order-source-notice")
                .await;
            fixture
                .notify(reader, Some(fork), "order-fork-notice")
                .await;
            let (deleted, answer) =
                delete_fork_during_read(fixture.pool(), &f.raw, owner, reader, fork, acknowledge)
                    .await;
            assert_eq!(deleted, fork, "round {round} acknowledge={acknowledge}");
            assert_eq!(answer, 1, "round {round} acknowledge={acknowledge}");
        }
    }
    fixture.close().await;
}

/// A personal fork's rename locks the owner's account, then the organization that owns the
/// fork's source. A ban locks that organization, then the account. The rename has to take the
/// membership lock first, or each can hold the row the other waits for. The ban takes the
/// membership lock and the organization as `change_ban` does, and takes the account only once
/// the rename is waiting.
#[tokio::test]
async fn postgres_fork_rename_and_owner_ban_do_not_deadlock() {
    let mut fixture = Fixture::new(true).await;
    let owner = fixture.actor("fork_ban_owner").await;
    let org = match db::organizations::create_organization(
        fixture.pool(),
        owner,
        &db::organizations::NewOrganization::new(
            "fork_ban_org",
            db::organizations::OrganizationProfile::new(None, None).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        db::organizations::OrganizationOutcome::Applied(org) => org.id,
        other => panic!("organization must be created: {other:?}"),
    };
    let source = fixture.repo(owner, Some(org), "fork_ban_source").await;
    let fork = db::repositories::discovery::create_forked_repository(
        fixture.pool(),
        db::repositories::discovery::NewForkedRepository {
            owner_id: owner,
            name: "fork_ban_fork",
            description: None,
            is_private: true,
            disk_path: "/owned/not-executed.git",
            forked_from: source,
            default_branch: "main",
        },
    )
    .await
    .unwrap()
    .id;
    let Fixture::Postgres(f) = &fixture else {
        unreachable!()
    };
    let mut ban = f.raw.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *ban)
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()), 37002)")
        .execute(&mut *ban)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(org)
        .fetch_one(&mut *ban)
        .await
        .unwrap();
    let pool = f.pool.clone();
    let rename = tokio::spawn(async move {
        db::repositories::records::rename_repository(&pool, owner, fork, "fork_ban_renamed").await
    });
    wait_for_lock(&f.raw, blocker).await;
    // The ban's next lock is the owner's account. A rename that already holds it while it
    // waits for the organization is the cycle this test covers.
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR NO KEY UPDATE")
        .bind(owner)
        .fetch_one(&mut *ban)
        .await
        .expect("the ban must not deadlock with the fork rename");
    ban.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), rename)
        .await
        .expect("fork rename finishes once the ban commits")
        .unwrap()
        .expect("fork rename must not deadlock with the ban");
    fixture.close().await;
}
