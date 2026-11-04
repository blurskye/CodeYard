//! The migration ledger refuses a database whose recorded history differs from this build.
use crate::support::native_database::{NativeDatabase, PostgresDatabase};
use codeyard::backend::db;
use codeyard_db_core::DbPool;

async fn execute(pool: &DbPool, sql: &str) {
    match pool {
        codeyard_db_core::DbPool::Sqlite(raw) => {
            sqlx::raw_sql(sql).execute(raw).await.unwrap();
        }
        codeyard_db_core::DbPool::Postgres(raw) => {
            sqlx::raw_sql(sql).execute(raw).await.unwrap();
        }
    };
}

async fn strings(pool: &DbPool, sql: &str) -> Vec<String> {
    match pool {
        codeyard_db_core::DbPool::Sqlite(raw) => {
            sqlx::query_scalar(sql).fetch_all(raw).await.unwrap()
        }
        codeyard_db_core::DbPool::Postgres(raw) => {
            sqlx::query_scalar(sql).fetch_all(raw).await.unwrap()
        }
    }
}

fn ledger(pool: &DbPool) -> &'static str {
    match pool {
        codeyard_db_core::DbPool::Sqlite(_) => "_gitserv_sqlite_migrations",
        codeyard_db_core::DbPool::Postgres(_) => "_gitserv_migrations",
    }
}

/// Changes the ledger of a migrated database, then checks that startup refuses it and
/// leaves the ledger and the rows as they were.
async fn check_refused(pool: &DbPool, change: &str, message: &str) {
    db::users::accounts::create_user(pool, "kept", None)
        .await
        .unwrap();
    let ledger = ledger(pool);
    execute(pool, &change.replace("{ledger}", ledger)).await;
    let history =
        format!("SELECT CAST(version AS TEXT)||':'||sha256 FROM {ledger} ORDER BY version");
    let before = strings(pool, &history).await;
    let error = codeyard_db_core::run_migrations(pool).await.unwrap_err();
    assert!(format!("{error:#}").contains(message), "{error:#}");
    assert_eq!(strings(pool, &history).await, before);
    assert_eq!(db::users::accounts::count_users(pool).await.unwrap(), 1);
}

const WRONG_CHECKSUM: &str = "UPDATE {ledger} SET sha256='wrong' WHERE version=1";
const NEWER_VERSION: &str =
    "INSERT INTO {ledger}(version,name,sha256) SELECT max(version)+1,'later','unknown' FROM {ledger}";

#[tokio::test]
async fn sqlite_wrong_checksum_is_refused_without_changes() {
    let fixture = NativeDatabase::new().await;
    check_refused(&fixture.pool, WRONG_CHECKSUM, "history/checksum mismatch").await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_wrong_checksum_is_refused_without_changes() {
    let mut fixture = PostgresDatabase::new().await;
    check_refused(&fixture.pool, WRONG_CHECKSUM, "history/checksum mismatch").await;
    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_newer_database_is_refused_without_changes() {
    let fixture = NativeDatabase::new().await;
    check_refused(&fixture.pool, NEWER_VERSION, "downgrade refused").await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_newer_database_is_refused_without_changes() {
    let mut fixture = PostgresDatabase::new().await;
    check_refused(&fixture.pool, NEWER_VERSION, "downgrade refused").await;
    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_in_memory_migrations_keep_the_connection_and_restore_foreign_keys() {
    let pool = codeyard_db_core::init_db("sqlite::memory:").await.unwrap();
    codeyard_db_core::run_migrations(&pool).await.unwrap();
    let user = db::users::accounts::create_user(&pool, "kept", None)
        .await
        .unwrap();
    execute(
        &pool,
        "UPDATE _gitserv_sqlite_migrations SET sha256='wrong' WHERE version=1",
    )
    .await;
    assert!(codeyard_db_core::run_migrations(&pool).await.is_err());
    assert_eq!(
        db::users::accounts::get_user_by_id(&pool, user.id)
            .await
            .unwrap()
            .unwrap()
            .username,
        "kept"
    );
    let codeyard_db_core::DbPool::Sqlite(raw) = &pool else {
        unreachable!()
    };
    let enabled: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(raw)
        .await
        .unwrap();
    assert_eq!(enabled, 1);
    pool.close().await;
}
