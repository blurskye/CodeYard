use crate::support::native_database::{NativeDatabase, PostgresDatabase};
use codeyard::backend::db::{self, DbPool};

async fn long_branch_activity(pool: &DbPool) {
    let user = db::users::accounts::create_user(pool, "branch-owner", None)
        .await
        .unwrap();
    let repo = db::repositories::records::create_repository(
        pool,
        user.id,
        "branches",
        None,
        false,
        "branches.git",
    )
    .await
    .unwrap();
    let branch = format!(
        "{}/{}/{}/{}/end",
        "a".repeat(254),
        "b".repeat(254),
        "c".repeat(254),
        "d".repeat(254)
    );
    assert_eq!(branch.len(), 1023);
    for branch in [branch.clone(), format!("{branch}x")] {
        db::activity::record_repository_push(pool, repo.id, user.id, &branch, 1)
            .await
            .unwrap();
        let rows = db::activity::list_user_activity(pool, user.id, Some(user.id), 20, 0)
            .await
            .unwrap();
        assert!(rows
            .rows
            .iter()
            .any(|row| row.branch.as_deref() == Some(&branch)));
    }
}

#[tokio::test]
async fn sqlite_records_supported_long_branches() {
    let database = NativeDatabase::new().await;
    long_branch_activity(&database.pool).await;
}

#[tokio::test]
async fn postgres_records_supported_long_branches() {
    let database = PostgresDatabase::new().await;
    long_branch_activity(&database.pool).await;
}

#[tokio::test]
async fn sqlite_repository_notification_deletion_uses_child_key_index() {
    let database = NativeDatabase::new().await;
    let plan: Vec<(i64, i64, i64, String)> =
        sqlx::query_as("EXPLAIN QUERY PLAN SELECT rowid FROM notifications WHERE repo_id = 42")
            .fetch_all(&database.raw)
            .await
            .unwrap();
    assert!(
        plan.iter()
            .any(|row| row.3.contains("idx_notifications_repo_id")),
        "{plan:?}"
    );
}

#[tokio::test]
async fn postgres_repository_notification_child_key_is_indexed() {
    let database = PostgresDatabase::new().await;
    let pool = &database.raw;
    let definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes WHERE schemaname = current_schema() AND indexname = 'idx_notifications_repo_id'",
    ).fetch_one(pool).await.unwrap();
    assert!(definition.ends_with("(repo_id)"), "{definition}");
}

#[tokio::test]
async fn cli_escapes_emails_descriptions_and_key_labels() {
    let database = NativeDatabase::new().await;
    let value = "Hi\u{1b}[2J\u{1b}[H\nforged\r\t\u{7f}\u{85} café";
    let escaped = "Hi\\u{1b}[2J\\u{1b}[H\\nforged\\r\\t\\u{7f}\\u{85} café";
    let user = db::users::accounts::create_user(&database.pool, "operator", Some(value))
        .await
        .unwrap();
    db::repositories::records::create_repository(
        &database.pool,
        user.id,
        "repo",
        Some(value),
        false,
        "repo.git",
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO ssh_keys(user_id, name, fingerprint, public_key, key_type) VALUES (?1, ?2, 'fingerprint', 'fixture', 'ssh-ed25519')")
        .bind(user.id).bind(value).execute(&database.raw).await.unwrap();
    for arguments in [
        vec!["user", "list"],
        vec!["repo", "list", "operator"],
        vec!["repo", "info", "operator", "repo"],
        vec!["key", "list", "operator"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_codeyard"))
            .args(&arguments)
            .env_clear()
            .env("DATABASE_URL", &database.url)
            .current_dir(database.installation_directory())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains(escaped), "{arguments:?}: {text}");
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{85}'),
            "{text}"
        );
    }
}
