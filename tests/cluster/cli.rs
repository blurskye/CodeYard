//! The `cluster` subcommands as the executable runs them: what works without a database,
//! what must be refused on a SQLite installation, and (the first test) what the repository
//! commands need on PostgreSQL.
use codeyard_support::node_identity::NodeKey;
use std::{
    path::Path,
    process::{Command, Output},
};

fn run(directory: &Path, arguments: &[&str], environment: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codeyard"))
        .args(arguments)
        .env_clear()
        .envs(environment.iter().copied())
        .current_dir(directory)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn cluster_commands_require_postgres_without_an_environment_gate() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join(".env"), "").unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("cell.db").display()
    );
    for arguments in [vec!["cluster", "enable"], vec!["serve", "--cluster"]] {
        let output = run(directory.path(), &arguments, &[("DATABASE_URL", &url)]);
        assert!(!output.status.success());
        assert!(
            text(&output.stderr).contains("PostgreSQL"),
            "{}",
            text(&output.stderr)
        );
    }
}

#[tokio::test]
async fn single_node_mode_refuses_cluster_roles_until_cluster_enable() {
    use crate::support::native_database::PostgresDatabase;
    let database = PostgresDatabase::new().await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join(".env"), "").unwrap();
    let url = database.connection_url();
    let environment = [("DATABASE_URL", url.as_str())];
    for arguments in [
        &["cluster", "invite", "--roles", "storage"][..],
        &["serve", "--roles", "web"][..],
        &["serve", "--roles", "storage"][..],
    ] {
        let output = run(directory.path(), arguments, &environment);
        assert!(!output.status.success(), "{arguments:?}");
        let error = text(&output.stderr);
        assert!(
            error.contains("single-node mode") && error.contains("codeyard cluster enable"),
            "{arguments:?}: {error}"
        );
    }
    let output = run(directory.path(), &["cluster", "enable"], &environment);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(text(&output.stdout).contains("Cluster mode is on"));
    let output = run(directory.path(), &["cluster", "enable"], &environment);
    assert!(text(&output.stdout).contains("already on"));
    // The invite now gets past the mode check and stops at the missing node key.
    let output = run(
        directory.path(),
        &["cluster", "invite", "--roles", "storage"],
        &environment,
    );
    let error = text(&output.stderr);
    assert!(!error.contains("single-node mode"), "{error}");
    assert!(error.contains("no key yet"), "{error}");
}

#[tokio::test]
async fn repository_cli_requires_web_or_ssh_identity_before_inserting() {
    use crate::support::native_database::PostgresDatabase;

    let database = PostgresDatabase::new().await;
    crate::support::native_database::enable_cluster(&database.pool)
        .await
        .unwrap();
    let user = codeyard_db_identity::users::accounts::create_user(&database.pool, "owner", None)
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join(".env"), "").unwrap();
    let key = NodeKey::generate();
    let node = codeyard_db_cluster::cluster::register_own_node(
        &database.pool,
        &key.public_hex(),
        "operator",
        &["storage".into()],
        "test",
    )
    .await
    .unwrap();
    let url = database.connection_url();
    let secret = key.secret_hex();
    let environment = [
        ("DATABASE_URL", url.as_str()),
        ("NODE_KEY", secret.as_str()),
        ("NODE_NAME", "operator"),
    ];
    for role in ["storage", "web", "ssh"] {
        let pool = &database.raw;
        sqlx::query("UPDATE nodes SET roles = $1 WHERE id = $2")
            .bind(vec![role.to_owned()])
            .bind(node.id)
            .execute(pool)
            .await
            .unwrap();
        let output = run(
            directory.path(),
            &["repo", "create", "owner", "refused"],
            &environment,
        );
        assert!(
            !output.status.success(),
            "no storage node is online: {role}"
        );
        let error = text(&output.stderr);
        if role == "storage" {
            assert!(
                error.contains("require an active web or ssh node identity"),
                "{error}"
            );
        } else {
            assert!(
                !error.contains("require an active web or ssh node identity"),
                "{error}"
            );
            assert!(error.contains("storage node unavailable"), "{error}");
        }
        assert!(
            codeyard_db_repos::repositories::records::list_user_repositories(
                &database.pool,
                user.id
            )
            .await
            .unwrap()
            .is_empty()
        );
    }
    let stored =
        codeyard_db_cluster::cluster::get_node_by_public_key(&database.pool, &key.public_hex())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(stored.id, node.id);
}

#[test]
fn keygen_prints_a_matching_key_pair_and_writes_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let mut seen = Vec::new();
    for _ in 0..2 {
        let output = run(directory.path(), &["cluster", "keygen"], &[]);
        assert!(output.status.success(), "{output:?}");
        let stdout = text(&output.stdout);
        let secret = stdout
            .lines()
            .find_map(|line| line.strip_prefix("NODE_KEY="))
            .unwrap();
        let public = stdout
            .lines()
            .find_map(|line| line.strip_prefix("NODE_PUBLIC_KEY="))
            .unwrap();
        assert_eq!(secret.len(), 64);
        assert_eq!(NodeKey::parse(secret).unwrap().public_hex(), public);
        // The hints go to stderr so that stdout can be sourced or parsed.
        assert!(text(&output.stderr).contains("CLUSTER_NODES"));
        seen.push(secret.to_owned());
    }
    assert_ne!(seen[0], seen[1], "every run makes a new key");
    // No state directory and no database appear for a command that needs neither.
    assert_eq!(directory.path().read_dir().unwrap().count(), 0);
}

#[test]
fn help_lists_every_cluster_command() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(directory.path(), &["cluster", "--help"], &[]);
    assert!(output.status.success(), "{output:?}");
    let help = text(&output.stdout);
    for command in [
        "keygen",
        "invite",
        "invites",
        "cancel-invite",
        "join",
        "list",
        "approve",
        "revoke",
        "adopt",
    ] {
        assert!(help.contains(command), "missing {command}: {help}");
    }
    let output = run(directory.path(), &["cluster", "invite", "--help"], &[]);
    let help = text(&output.stdout);
    for flag in ["--roles", "--reusable", "--expires", "--approve"] {
        assert!(help.contains(flag), "missing {flag}: {help}");
    }
    assert_eq!(directory.path().read_dir().unwrap().count(), 0);
}

#[test]
fn invite_arguments_are_checked_before_anything_else_happens() {
    let directory = tempfile::tempdir().unwrap();
    for arguments in [
        vec!["cluster", "invite"],
        vec!["cluster", "invite", "--roles", "cook"],
        vec![
            "cluster",
            "invite",
            "--roles",
            "storage",
            "--expires",
            "366d",
        ],
        vec![
            "cluster",
            "invite",
            "--roles",
            "storage",
            "--expires",
            "soon",
        ],
        vec!["cluster", "invite", "--roles", "storage", "--expires", "0h"],
    ] {
        let output = run(directory.path(), &arguments, &[]);
        assert!(!output.status.success(), "{arguments:?} was accepted");
    }
    let output = run(
        directory.path(),
        &[
            "cluster",
            "invite",
            "--roles",
            "storage",
            "--expires",
            "366d",
        ],
        &[],
    );
    assert!(
        text(&output.stderr).contains("365d"),
        "{}",
        text(&output.stderr)
    );
    assert_eq!(directory.path().read_dir().unwrap().count(), 0);
}

#[test]
fn joining_with_a_bad_ticket_fails_before_it_touches_the_disk() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let output = run(
        directory.path(),
        &["cluster", "join", "not-a-ticket"],
        &[("GITSERV_STATE_DIR", state.to_str().unwrap())],
    );
    assert!(!output.status.success());
    assert!(
        text(&output.stderr).contains("not a Codeyard join ticket"),
        "{}",
        text(&output.stderr)
    );
    // No state directory, database or key was created.
    assert!(!state.exists());
}

#[test]
fn membership_commands_refuse_a_sqlite_installation() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let environment = [("GITSERV_STATE_DIR", state.to_str().unwrap())];
    for arguments in [
        vec!["cluster", "list"],
        vec!["cluster", "invites"],
        vec!["cluster", "cancel-invite", "1"],
        vec!["cluster", "approve", "node-1"],
        vec!["cluster", "revoke", "node-1"],
        vec!["cluster", "adopt"],
        vec!["cluster", "enable"],
        vec!["cluster", "invite", "--roles", "storage"],
    ] {
        let output = run(directory.path(), &arguments, &environment);
        assert!(!output.status.success(), "{arguments:?} ran on SQLite");
        let message = text(&output.stderr);
        assert!(
            message.contains("needs PostgreSQL"),
            "{arguments:?}: {message}"
        );
    }
    // Joining from the same installation is refused as well.
    let ticket = "codeyard-join-v1.e30";
    let output = run(directory.path(), &["cluster", "join", ticket], &environment);
    assert!(!output.status.success());
    assert!(
        text(&output.stderr).contains("needs PostgreSQL"),
        "{}",
        text(&output.stderr)
    );
    let output = run(
        directory.path(),
        &["cluster", "join", ticket],
        &[
            ("DATABASE_URL", "sqlite://elsewhere.db"),
            ("GITSERV_STATE_DIR", state.to_str().unwrap()),
        ],
    );
    assert!(
        text(&output.stderr).contains("needs PostgreSQL"),
        "{}",
        text(&output.stderr)
    );
}

#[test]
fn serving_with_cluster_settings_on_sqlite_stops_before_listening() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let repos = directory.path().join("repos");
    let output = run(
        directory.path(),
        &["serve"],
        &[
            ("GITSERV_STATE_DIR", state.to_str().unwrap()),
            ("REPOS_PATH", repos.to_str().unwrap()),
            ("HTTP_ADDR", "127.0.0.1:0"),
            ("SSH_MODE", "disabled"),
            ("IROH_BIND", "127.0.0.1:0"),
            ("NODE_NAME", "test-node"),
        ],
    );
    assert!(!output.status.success());
    assert!(
        text(&output.stderr).contains("require DATABASE_URL (PostgreSQL for a cluster"),
        "{}",
        text(&output.stderr)
    );
    // It stopped before creating a key for the cluster.
    assert!(!state.join("node.key").exists());
}

#[test]
fn a_storage_only_node_is_refused_in_single_node_mode() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let repos = directory.path().join("repos");
    let database = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("node.db").display()
    );
    let output = run(
        directory.path(),
        &["serve", "--roles", "storage"],
        &[
            ("DATABASE_URL", database.as_str()),
            ("GITSERV_STATE_DIR", state.to_str().unwrap()),
            ("REPOS_PATH", repos.to_str().unwrap()),
            ("SSH_MODE", "disabled"),
        ],
    );
    assert!(!output.status.success());
    assert!(
        text(&output.stderr).contains("run `codeyard cluster enable`"),
        "{}",
        text(&output.stderr)
    );
}
