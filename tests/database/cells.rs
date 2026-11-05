//! The operator CLI reserves ranges atomically and preserves existing cell-zero IDs.
use crate::support::native_database::{NativeDatabase, PostgresDatabase};
use codeyard_support::cells::{Prefix, IDS_PER_PREFIX};
use std::{path::Path, process::Output, time::Duration};

fn command(root: &Path, url: &str, prefix: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_codeyard"));
    command
        .args(["cell", "initialize", "--prefix", prefix])
        .current_dir(root)
        .env_clear()
        .env("DATABASE_URL", url)
        .env("GITSERV_STATE_DIR", root.join("state"))
        .kill_on_drop(true);
    command
}

async fn initialize(root: &Path, url: &str, prefix: &str) -> Output {
    tokio::time::timeout(Duration::from_secs(30), command(root, url, prefix).output())
        .await
        .expect("cell CLI has an owned timeout")
        .unwrap()
}

fn root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "").unwrap();
    root
}

#[tokio::test]
async fn cell_one_issues_exact_bounded_ids_and_initialization_never_rewinds_them() {
    let mut database = PostgresDatabase::new().await;
    let root = root();
    let url = database.connection_url();
    let output = initialize(root.path(), &url, "1").await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    codeyard_db_core::cells::load(&database.pool).await.unwrap();
    assert_eq!(database.pool.cell_prefix(), Prefix::new(1));
    let initialized: bool =
        sqlx::query_scalar("SELECT setup_completed FROM node_configuration WHERE id=1")
            .fetch_one(&database.raw)
            .await
            .unwrap();
    assert!(
        initialized,
        "a new cell must not offer a second public first-admin setup"
    );
    let a = codeyard_db_identity::users::accounts::create_user(&database.pool, "first", None)
        .await
        .unwrap();
    assert_eq!(a.id, IDS_PER_PREFIX + 1);
    let output = initialize(root.path(), &url, "1").await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let b = codeyard_db_identity::users::accounts::create_user(&database.pool, "second", None)
        .await
        .unwrap();
    assert_eq!(b.id, a.id + 1);
    assert_eq!((b.id as f64) as i64, b.id);
    let bounds: (i64,i64) = sqlx::query_as("SELECT min_value,max_value FROM pg_sequences WHERE schemaname=current_schema() AND sequencename='users_id_seq'")
        .fetch_one(&database.raw).await.unwrap();
    assert_eq!(bounds, (IDS_PER_PREFIX + 1, 2 * IDS_PER_PREFIX - 1));
    for prefix in ["0", "2", "65536", "01", "1; DROP TABLE users"] {
        assert!(!initialize(root.path(), &url, prefix).await.status.success());
    }
    assert_eq!(
        codeyard_db_identity::users::accounts::count_users(&database.pool)
            .await
            .unwrap(),
        2
    );
    database.finish().await;
}

#[tokio::test]
async fn cell_zero_keeps_accounts_and_sqlite_stays_standalone() {
    let mut postgres = PostgresDatabase::new().await;
    let sqlite = NativeDatabase::new().await;
    for (pool, url) in [
        (&postgres.pool, postgres.connection_url()),
        (&sqlite.pool, sqlite.url.clone()),
    ] {
        let owner = codeyard_db_identity::users::accounts::create_user(pool, "existing", None)
            .await
            .unwrap();
        let root = root();
        let output = initialize(root.path(), &url, "0").await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let after = codeyard_db_identity::users::accounts::create_user(pool, "after", None)
            .await
            .unwrap();
        assert_eq!(after.id, owner.id + 1);
    }
    let root = root();
    let refused = initialize(root.path(), &sqlite.url, "1").await;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("PostgreSQL"));
    postgres.finish().await;
}

#[tokio::test]
async fn pending_account_insert_prevents_a_range_switch_and_all_sequence_changes_roll_back() {
    let mut database = PostgresDatabase::new().await;
    let before: (i64,i64,Option<i64>) = sqlx::query_as("SELECT min_value,max_value,last_value FROM pg_sequences WHERE schemaname=current_schema() AND sequencename='actions_jobs_id_seq'")
        .fetch_one(&database.raw).await.unwrap();
    let mut writer = database.raw.begin().await.unwrap();
    sqlx::query("INSERT INTO users(username) VALUES('pending')")
        .execute(&mut *writer)
        .await
        .unwrap();
    let root = root();
    let mut child = command(root.path(), &database.connection_url(), "1");
    child
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = child.spawn().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        loop {
            tick.tick().await;
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation='users'::regclass AND mode='AccessExclusiveLock' AND NOT granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))")
                .fetch_one(&database.raw).await.unwrap();
            if waiting { break; }
        }
    }).await.expect("range initialization must retain the users table before checking it");
    writer.commit().await.unwrap();
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("empty cell"));
    let after: (i64,i64,Option<i64>) = sqlx::query_as("SELECT min_value,max_value,last_value FROM pg_sequences WHERE schemaname=current_schema() AND sequencename='actions_jobs_id_seq'")
        .fetch_one(&database.raw).await.unwrap();
    assert_eq!(before, after);
    assert_eq!(
        codeyard_db_identity::users::accounts::count_users(&database.pool)
            .await
            .unwrap(),
        1
    );
    database.finish().await;
}
