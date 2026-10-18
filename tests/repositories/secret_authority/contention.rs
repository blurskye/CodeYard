use super::{
    applied, database::Database, db, denied, orgs, put, secrets, AccessLevel, SecretsError, NAME,
};
use std::{str::FromStr, time::Duration};

async fn attempt(
    pool: &db::DbPool,
    actor: i64,
    repo: i64,
    operation: u8,
) -> Result<(), SecretsError> {
    match operation {
        0 => put(pool, repo, Some(actor)).await,
        1 => secrets::list_secrets(pool, actor, repo).await.map(|_| ()),
        _ => secrets::delete_secret(pool, actor, repo, NAME)
            .await
            .map(|_| ()),
    }
}

// An observed server lock edge, not sleep or task non-completion as evidence.
async fn wait_for_lock(raw: &sqlx::PgPool, blocker: i32) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)) AND wait_event_type='Lock')")
                .bind(blocker).fetch_one(raw).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("actual secret lock wait edge required");
}

async fn wait_for_lock_chain(raw: &sqlx::PgPool, blocker: i32, waiters: i64) {
    tokio::time::timeout(Duration::from_secs(10), async {
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
    .expect("actual transitive secret lock wait edges");
}

#[tokio::test]
async fn postgres_secrets_wait_for_uncommitted_grant_revocation() {
    let mut d = Database::new(true).await;
    let owner = d.actor("secret_wait_owner").await;
    let actor = d.actor("secret_wait_admin").await;
    let org = applied(
        orgs::create_organization(
            d.pool(),
            owner,
            &orgs::NewOrganization::new(
                "secret_wait_org",
                orgs::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    let personal = d.repo(owner, None, "secret_wait_personal", true).await;
    let organization = d
        .repo(owner, Some(org), "secret_wait_organization", true)
        .await;
    let Database::Postgres(f) = &d else {
        unreachable!()
    };
    for repo in [personal, organization] {
        put(d.pool(), repo, Some(owner)).await.unwrap();
        for operation in 0..3 {
            db::repositories::access::grant_repo_access(d.pool(), actor, repo, AccessLevel::Admin)
                .await
                .unwrap();
            // Simulate an ingress projection before revocation. It is no permit.
            assert_eq!(
                db::repositories::access::get_user_repo_access(d.pool(), actor, repo)
                    .await
                    .unwrap(),
                AccessLevel::Admin
            );
            let before = d.snapshot(repo).await;
            let mut revoke = f.raw.begin().await.unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *revoke)
                .await
                .unwrap();
            // The uncommitted delete remains visible to an ordinary projection.
            // An effectful authority read must instead wait for and recheck it.
            sqlx::query("DELETE FROM repo_access WHERE repo_id=$1 AND user_id=$2")
                .bind(repo)
                .bind(actor)
                .execute(&mut *revoke)
                .await
                .unwrap();
            let pool = d.pool().clone();
            let task = tokio::spawn(async move { attempt(&pool, actor, repo, operation).await });
            wait_for_lock(&f.raw, pid).await;
            revoke.commit().await.unwrap();
            denied(
                tokio::time::timeout(Duration::from_secs(10), task)
                    .await
                    .unwrap()
                    .unwrap(),
            );
            assert_eq!(d.snapshot(repo).await, before);
        }
    }
    d.finish().await;
}

#[tokio::test]
async fn postgres_secret_write_keeps_its_grant_until_commit() {
    let mut d = Database::new(true).await;
    let owner = d.actor("secret_commit_owner").await;
    let actor = d.actor("secret_commit_admin").await;
    let repo = d.repo(owner, None, "secret_commit_repo", true).await;
    put(d.pool(), repo, Some(owner)).await.unwrap();
    db::repositories::access::grant_repo_access(d.pool(), actor, repo, AccessLevel::Admin)
        .await
        .unwrap();
    let Database::Postgres(f) = &d else {
        unreachable!()
    };

    let mut secret = f.raw.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *secret)
        .await
        .unwrap();
    sqlx::query("SELECT name FROM repo_secrets WHERE repo_id=$1 AND name=$2 FOR UPDATE")
        .bind(repo)
        .bind(NAME)
        .fetch_one(&mut *secret)
        .await
        .unwrap();
    let pool = d.pool().clone();
    let effect = tokio::spawn(async move { put(&pool, repo, Some(actor)).await });
    wait_for_lock(&f.raw, blocker).await;

    let pool = d.pool().clone();
    let revocation = tokio::spawn(async move {
        db::repositories::access::revoke_repo_access(&pool, actor, repo).await
    });
    wait_for_lock_chain(&f.raw, blocker, 2).await;
    assert!(!revocation.is_finished());
    secret.commit().await.unwrap();

    effect.await.unwrap().unwrap();
    assert!(revocation.await.unwrap().unwrap());
    denied(secrets::list_secrets(d.pool(), actor, repo).await);
    assert_eq!(
        secrets::list_secrets(d.pool(), owner, repo)
            .await
            .unwrap()
            .len(),
        1
    );
    d.finish().await;
}

#[tokio::test]
async fn sqlite_secrets_require_reserved_writer_even_for_metadata() {
    let mut d = Database::new(false).await;
    let owner = d.actor("secret_reserved_owner").await;
    let actor = d.actor("secret_reserved_admin").await;
    let repo = d.repo(owner, None, "secret_reserved_repo", true).await;
    put(d.pool(), repo, Some(owner)).await.unwrap();
    db::repositories::access::grant_repo_access(d.pool(), actor, repo, AccessLevel::Admin)
        .await
        .unwrap();
    let before = d.snapshot(repo).await;
    let Database::Sqlite(f) = &d else {
        unreachable!()
    };
    // A zero busy timeout makes actual reserved-writer contention observable as
    // SQLITE_BUSY, including list (which would succeed as a plain pool read).
    let raw = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::from_str(&f.url)
                .unwrap()
                .foreign_keys(true)
                .busy_timeout(Duration::ZERO),
        )
        .await
        .expect("required independent SQLite contention connection");
    let second: db::DbPool = raw.clone().into();
    let mut revoke = f.raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
    for operation in 0..3 {
        let error = attempt(&second, actor, repo, operation)
            .await
            .expect_err("reserved writer must block secret operation");
        let SecretsError::Database(error) = error else {
            panic!("expected native SQLite contention")
        };
        let code = error
            .as_database_error()
            .and_then(|e| e.code())
            .and_then(|code| code.parse::<i32>().ok())
            .expect("SQLite busy code");
        assert_eq!(
            code & 0xff,
            5,
            "expected observed SQLITE_BUSY, not timing evidence"
        );
    }
    sqlx::query("DELETE FROM repo_access WHERE repo_id=?1 AND user_id=?2")
        .bind(repo)
        .bind(actor)
        .execute(&mut *revoke)
        .await
        .unwrap();
    revoke.commit().await.unwrap();
    for operation in 0..3 {
        denied(attempt(&second, actor, repo, operation).await);
    }
    assert_eq!(d.snapshot(repo).await, before);
    second.close().await;
    d.finish().await;
}
