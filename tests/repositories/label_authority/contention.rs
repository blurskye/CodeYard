use super::{db, mutate, refusal, seed, snapshot, Fixture, LabelWriteRefusal, MUTATIONS};
use crate::support::database::wait_until_blocked;
use std::{str::FromStr, time::Duration};

#[tokio::test]
async fn postgres_every_label_write_waits_for_actor_row_then_rechecks_revocation() {
    let mut fixture = Fixture::new(true).await;
    let owner = fixture.actor("lock_owner").await;
    let actor = fixture.actor("lock_admin").await;
    let repo = fixture.label_repo(owner, None).await;
    let (issue, label) = seed(fixture.pool(), owner, repo).await;
    let raw = fixture.pg();
    for mutation in MUTATIONS {
        db::repositories::access::grant_repo_access(
            fixture.pool(),
            actor,
            repo,
            db::models::repositories::AccessLevel::Admin,
        )
        .await
        .unwrap();
        let before = snapshot(fixture.pool(), &[repo]).await;
        let mut revoker = raw.begin().await.unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        // The actor's account row is the first thing every label write locks.
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
            .bind(actor)
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        let pool = fixture.pool().clone();
        let task =
            tokio::spawn(async move { mutate(&pool, actor, repo, issue, label, mutation).await });
        wait_until_blocked(raw, blocker, 1).await;
        assert!(!task.is_finished(), "must wait before effect: {mutation:?}");
        // These NOWAIT locks prove the waiting write did not acquire the
        // repository, label or issue rows first. No lock-order claim is inferred
        // from sleeps.
        sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE NOWAIT")
            .bind(repo)
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM issues WHERE id=$1 FOR UPDATE NOWAIT")
            .bind(issue)
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM labels WHERE id=$1 FOR UPDATE NOWAIT")
            .bind(label)
            .fetch_one(&mut *revoker)
            .await
            .unwrap();
        sqlx::query("DELETE FROM repo_access WHERE repo_id=$1 AND user_id=$2")
            .bind(repo)
            .bind(actor)
            .execute(&mut *revoker)
            .await
            .unwrap();
        revoker.commit().await.unwrap();
        refusal(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap(),
            LabelWriteRefusal::Denied,
        );
        assert_eq!(
            snapshot(fixture.pool(), &[repo]).await,
            before,
            "revoked {mutation:?}"
        );
    }
    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_every_label_write_contends_for_reserved_writer_then_rechecks() {
    let mut fixture = Fixture::new(false).await;
    let owner = fixture.actor("reserved_owner").await;
    let actor = fixture.actor("reserved_admin").await;
    let repo = fixture.label_repo(owner, None).await;
    let (issue, label) = seed(fixture.pool(), owner, repo).await;
    db::repositories::access::grant_repo_access(
        fixture.pool(),
        actor,
        repo,
        db::models::repositories::AccessLevel::Admin,
    )
    .await
    .unwrap();
    let Fixture::Sqlite(f) = &fixture else {
        unreachable!()
    };
    // Independent native handle with zero busy timeout: observed SQLITE_BUSY,
    // not an unfinished task or scheduling/timing assumption, proves contention.
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(&f.url)
        .unwrap()
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(Duration::ZERO);
    // One connection: a refused write rolls back in the background, and with no
    // busy timeout the next write would see that lock still held on another one.
    let raw = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let second: db::DbPool = raw.clone().into();
    let before = snapshot(&second, &[repo]).await;
    let mut revoker = f.raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
    for mutation in MUTATIONS {
        let error = mutate(&second, actor, repo, issue, label, mutation)
            .await
            .expect_err("writer reservation must contend");
        let driver = error
            .downcast_ref::<sqlx::Error>()
            .expect("actual native busy refusal");
        assert_eq!(
            driver.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("5"),
            "must observe SQLITE_BUSY"
        );
        assert_eq!(snapshot(&second, &[repo]).await, before);
    }
    sqlx::query("DELETE FROM repo_access WHERE repo_id=?1 AND user_id=?2")
        .bind(repo)
        .bind(actor)
        .execute(&mut *revoker)
        .await
        .unwrap();
    revoker.commit().await.unwrap();
    for mutation in MUTATIONS {
        refusal(
            mutate(&second, actor, repo, issue, label, mutation).await,
            LabelWriteRefusal::Denied,
        );
        assert_eq!(snapshot(&second, &[repo]).await, before);
    }
    second.close().await;
    raw.close().await;
    fixture.finish().await;
}
