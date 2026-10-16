//! The public executable remains codeyard despite Cargo's identifier constraint.
#[path = "support/native_database.rs"]
mod native;
use codeyard::backend::db;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn canonical_help_is_available_without_configuration_or_state_creation() {
    let directory = tempfile::tempdir().unwrap();
    for arguments in [
        vec!["--help"],
        vec!["serve", "--help"],
        vec!["repo", "create", "--help"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_codeyard"))
            .args(arguments)
            .env_clear()
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(help.contains("Usage: codeyard "), "{help}");
    }
    assert_eq!(directory.path().read_dir().unwrap().count(), 0);
}

#[test]
fn version_is_available_without_configuration_or_state_creation() {
    let directory = tempfile::tempdir().unwrap();
    for argument in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_codeyard"))
            .arg(argument)
            .env_clear()
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("codeyard {}\n", env!("CARGO_PKG_VERSION"))
        );
        assert!(output.stderr.is_empty(), "{output:?}");
    }
    assert_eq!(directory.path().read_dir().unwrap().count(), 0);
}

#[test]
fn db_copy_help_hides_database_url_values() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_codeyard"))
        .args(["db", "copy", "--help"])
        .env_clear()
        .env(
            "COPY_FROM_DATABASE_URL",
            "postgres://source_user:source-secret@source.example:5432/app",
        )
        .env(
            "COPY_TO_DATABASE_URL",
            "postgres://target_user:target-secret@target.example:5432/app",
        )
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("Usage: codeyard db copy"), "{help}");
    for secret in [
        "source-secret",
        "target-secret",
        "source.example",
        "target.example",
    ] {
        assert!(!help.contains(secret), "{help}");
    }
}

fn run(directory: &std::path::Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codeyard"))
        .args(arguments)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(directory)
        .output()
        .unwrap()
}

#[tokio::test]
async fn user_create_requires_setup_without_blocking_web_bootstrap() {
    let installation = tempfile::tempdir().unwrap();
    assert!(run(installation.path(), &["init"]).status.success());

    let refused = run(installation.path(), &["user", "create", "settings"]);
    assert!(!refused.status.success(), "{refused:?}");
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(stderr.contains("That username is reserved"), "{stderr}");

    let refused = run(installation.path(), &["user", "create", "alice"]);
    assert!(!refused.status.success(), "{refused:?}");
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(stderr.contains("Complete /setup first"), "{stderr}");
    assert!(stderr.contains("user bootstrap"), "{stderr}");
    let url = format!(
        "sqlite://{}",
        installation.path().join("data/gitserv.db").display()
    );
    let pool = db::init_db(&url).await.unwrap();
    assert!(db::users::accounts::list_users(&pool)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        db::users::transitions::complete_setup(
            &pool,
            "root_admin",
            None,
            "fixture",
            "public",
            None
        )
        .await
        .unwrap(),
        db::users::transitions::SetupClaim::Completed(_)
    ));
    let created = run(installation.path(), &["user", "create", "alice"]);
    assert!(created.status.success(), "{created:?}");
    let list = run(installation.path(), &["user", "list"]);
    let listed = String::from_utf8(list.stdout).unwrap();
    assert!(listed.contains("alice"), "{listed}");
    assert!(!listed.contains("settings"), "{listed}");
    pool.close().await;
}

fn provision(
    directory: &std::path::Path,
    url: &str,
    arguments: &[&str],
    password: Option<&str>,
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codeyard"))
        .args(arguments)
        .env_clear()
        .env("DATABASE_URL", url)
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(password) = password {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(password.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

async fn cli_account_contract(pool: &db::DbPool, url: &str) {
    use db::users::{accounts, transitions};
    let installation = tempfile::tempdir().unwrap();
    let refused = provision(
        installation.path(),
        url,
        &["user", "create", "before_setup"],
        None,
    );
    assert!(!refused.status.success(), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("Complete /setup first"));
    assert!(accounts::list_users(pool).await.unwrap().is_empty());
    // A historical CLI account cannot be adopted through public setup or local recovery.
    let existing = accounts::create_user(pool, "alice", None).await.unwrap();
    assert!(matches!(
        transitions::complete_setup(pool, "alice", None, "fixture", "public", None)
            .await
            .unwrap(),
        transitions::SetupClaim::ExistingAccounts
    ));
    for (username, password) in [("alice", "bootstrap-password\n"), ("root_admin", "short\n")] {
        let output = provision(
            installation.path(),
            url,
            &["user", "bootstrap", username, "--password-stdin"],
            Some(password),
        );
        assert!(!output.status.success(), "{output:?}");
        assert_eq!(accounts::list_users(pool).await.unwrap().len(), 1);
        assert!(db::node_config::get_node_configuration(pool)
            .await
            .unwrap()
            .is_none_or(|config| !config.setup_completed));
    }
    let output = provision(
        installation.path(),
        url,
        &[
            "user",
            "bootstrap",
            "root_admin",
            "--password-stdin",
            "--mode",
            "private",
        ],
        Some("bootstrap-password\n"),
    );
    assert!(output.status.success(), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("bootstrap-password"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("bootstrap-password"));
    let config = db::node_config::get_node_configuration(pool)
        .await
        .unwrap()
        .unwrap();
    assert!(config.setup_completed);
    assert_eq!(config.node_mode, "private");
    let admin = accounts::get_user_by_username(pool, "root_admin")
        .await
        .unwrap()
        .unwrap();
    assert!(admin.is_admin);
    assert!(
        codeyard::backend::identity::passwords::verify_password(
            "bootstrap-password",
            admin.password_hash.as_deref()
        )
        .await
        .unwrap()
        .valid
    );
    let alice = accounts::get_user_by_id(pool, existing.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!alice.is_admin);
    assert!(alice.password_hash.is_none());
    let repeated = provision(
        installation.path(),
        url,
        &["user", "bootstrap", "another_admin", "--password-stdin"],
        Some("bootstrap-password\n"),
    );
    assert!(!repeated.status.success(), "{repeated:?}");
    for username in [
        "".to_owned(),
        "ab".to_owned(),
        "a/b".to_owned(),
        "alice/bob".to_owned(),
        "a b".to_owned(),
        "a.b".to_owned(),
        "settings".to_owned(),
        "a".repeat(256),
        "é".repeat(256),
        "𐐀".repeat(256),
    ] {
        let output = provision(
            installation.path(),
            url,
            &["user", "create", &username],
            None,
        );
        assert!(!output.status.success(), "{username:?}: {output:?}");
    }
    assert_eq!(accounts::list_users(pool).await.unwrap().len(), 2);
    for username in [
        "abc".to_owned(),
        "éé".to_owned(),
        "a".repeat(255),
        "é".repeat(255),
        "𐐀".repeat(255),
    ] {
        let output = provision(
            installation.path(),
            url,
            &["user", "create", &username],
            None,
        );
        assert!(output.status.success(), "{username:?}: {output:?}");
        let user = accounts::get_user_by_username(pool, &username)
            .await
            .unwrap()
            .unwrap();
        assert!(!user.is_admin);
        assert!(user.password_hash.is_none());
    }
}

#[tokio::test]
async fn cli_account_validation_and_local_recovery_sqlite() {
    let f = native::NativeDatabase::new().await;
    cli_account_contract(&f.pool, &f.url).await;
}

#[tokio::test]
async fn cli_account_validation_and_local_recovery_postgres() {
    let f = native::PostgresDatabase::new().await;
    cli_account_contract(&f.pool, &f.connection_url()).await;
}

#[tokio::test]
async fn local_bootstrap_rolls_back_and_serializes_with_public_setup_on_both_engines() {
    use db::users::transitions::{self, SetupClaim};
    let postgres = native::PostgresDatabase::new().await;
    let sqlite = native::NativeDatabase::new().await;
    for pool in [&postgres.pool, &sqlite.pool] {
        assert!(
            transitions::bootstrap_admin(pool, "root_admin", "fixture", "invalid-mode")
                .await
                .is_err()
        );
        assert!(db::users::accounts::list_users(pool)
            .await
            .unwrap()
            .is_empty());
        assert!(db::node_config::get_node_configuration(pool)
            .await
            .unwrap()
            .is_none());
        let (local, public) = tokio::join!(
            transitions::bootstrap_admin(pool, "local_admin", "fixture", "private"),
            transitions::complete_setup(pool, "web_admin", None, "fixture", "public", None),
        );
        let (winner, mode) = match (local.unwrap(), public.unwrap()) {
            (SetupClaim::Completed(user), SetupClaim::AlreadyCompleted) => (user, "private"),
            (SetupClaim::AlreadyCompleted, SetupClaim::Completed(user)) => (user, "public"),
            other => panic!("unexpected competing setup results: {other:?}"),
        };
        assert!(winner.is_admin);
        assert_eq!(
            db::users::accounts::list_users(pool).await.unwrap().len(),
            1
        );
        let config = db::node_config::get_node_configuration(pool)
            .await
            .unwrap()
            .unwrap();
        assert!(config.setup_completed);
        assert_eq!(config.node_mode, mode);
    }
}
