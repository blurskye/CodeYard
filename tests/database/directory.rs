//! Separate-directory operator commands and atomic claims across two cells.
use crate::support::native_database::PostgresDatabase;
use codeyard_db_directory::{Config, Directory};
use codeyard_model::cells::{Cell, Key, KeyKind, Object, ObjectKind, State};
use codeyard_support::cells::Prefix;
use std::{path::Path, process::Output, time::Duration};

fn cell(prefix: u16, key: char) -> Cell {
    Cell {
        id: Prefix::new(prefix),
        name: format!("cell-{prefix}"),
        region: "a".into(),
        state: State::Open,
        web_urls: vec![format!("http://127.0.0.1:{}/", 10000 + u32::from(prefix))],
        ssh_addresses: vec![format!("127.0.0.1:{}", 20000 + u32::from(prefix))],
        node_keys: vec![key.to_string().repeat(64)],
        database_bytes: 0,
        write_rate: 0,
        maximum_database_bytes: 1000,
        maximum_write_rate: 100,
    }
}

async fn cli(root: &Path, url: &str, arguments: &[&str]) -> Output {
    tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_codeyard"))
            .args(["cell", "directory"])
            .args(arguments)
            .current_dir(root)
            .env_clear()
            .env("DIRECTORY_DATABASE_URL", url)
            // The directory command must neither open the cell DB nor prepare local state.
            .env("DATABASE_URL", "refused://must-not-open")
            .env("GITSERV_STATE_DIR", root.join("missing/state"))
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn directory_cli_owns_a_separate_baseline_and_bounds_pages_and_placement() {
    let mut database = PostgresDatabase::unmigrated().await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "").unwrap();
    let output = cli(root.path(), &database.connection_url(), &["initialize"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.path().join("missing").exists());
    let cell_table: Option<String> = sqlx::query_scalar("SELECT to_regclass('users')::text")
        .fetch_one(&database.raw)
        .await
        .unwrap();
    assert_eq!(cell_table, None);
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    for (prefix, name, key) in [("0", "first", a.as_str()), ("1", "second", b.as_str())] {
        let output = cli(
            root.path(),
            &database.connection_url(),
            &[
                "register",
                "--prefix",
                prefix,
                "--name",
                name,
                "--region",
                "a",
                "--web-url",
                "http://127.0.0.1:3000/",
                "--node-key",
                key,
                "--maximum-database-bytes",
                "1000",
                "--maximum-write-rate",
                "100",
                "--open",
            ],
        )
        .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = cli(
        root.path(),
        &database.connection_url(),
        &["list", "--after", "0", "--limit", "1"],
    )
    .await;
    assert!(output.status.success());
    let listed: Vec<Cell> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, Prefix::new(1));
    assert!(!cli(
        root.path(),
        &database.connection_url(),
        &["list", "--limit", "101"]
    )
    .await
    .status
    .success());
    let directory = Directory::connect(&database.connection_url(), Config::default())
        .await
        .unwrap();
    assert_eq!(
        directory.choose_cell("a").await.unwrap().unwrap().id,
        Prefix::ZERO
    );
    directory.report_load(&a, 500, 0).await.unwrap();
    assert_eq!(
        directory.choose_cell("a").await.unwrap().unwrap().id,
        Prefix::new(1)
    );
    directory.report_load(&b, 0, 50).await.unwrap();
    assert!(directory.choose_cell("a").await.unwrap().is_none());
    let mut collision = cell(2, 'b');
    collision.name = "collision".into();
    assert!(directory.register_cell(collision).await.is_err());
    assert!(
        directory.cell(Prefix::new(2)).await.unwrap().is_none(),
        "key conflict rolls back the cell too"
    );
    directory.close().await;
    database.finish().await;
}

#[tokio::test]
async fn pending_directory_keys_activate_together_and_revocation_is_checked_again() {
    let mut database = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&database.connection_url(), Config::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    for (prefix, key) in [(0, 'a'), (1, 'b')] {
        directory.register_cell(cell(prefix, key)).await.unwrap();
    }
    let peer = "a".repeat(64);
    let other_peer = "b".repeat(64);
    let nonce = "c".repeat(64);
    let object = Object {
        kind: ObjectKind::User,
        id: 1,
    };
    let name = Key::new(KeyKind::Name, "Alice").unwrap();
    let mail = Key::new(KeyKind::Email, "Alice@example.com").unwrap();
    let keys = vec![name.clone(), mail.clone()];
    directory
        .reserve(&peer, object, &keys, &nonce)
        .await
        .unwrap();
    assert!(directory.lookup(&name).await.unwrap().is_none());
    assert!(directory.lookup(&mail).await.unwrap().is_none());
    // A partial retry may not publish a subset of the original object.
    assert!(directory
        .confirm(&peer, object, std::slice::from_ref(&name), &nonce)
        .await
        .is_err());
    assert!(directory.lookup(&name).await.unwrap().is_none());
    assert!(directory
        .confirm(&other_peer, object, &keys, &nonce)
        .await
        .is_err());
    let mut revoked = cell(0, 'd');
    revoked.state = State::Closed;
    directory.register_cell(revoked).await.unwrap();
    assert!(directory
        .confirm(&peer, object, &keys, &nonce)
        .await
        .is_err());
    assert!(directory.report_load(&peer, 0, 0).await.is_err());
    assert!(directory.lookup(&name).await.unwrap().is_none());
    let replacement = "d".repeat(64);
    directory
        .confirm(&replacement, object, &keys, &nonce)
        .await
        .unwrap();
    directory
        .confirm(&replacement, object, &keys, &nonce)
        .await
        .unwrap();
    let location = directory
        .lookup(&Key::new(KeyKind::Name, "aLiCe").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(location.object, object);
    assert_eq!(location.cell, Prefix::ZERO);
    // A foreign cell cannot claim the same global name; all its other reservations roll back.
    let foreign = Object {
        kind: ObjectKind::User,
        id: *Prefix::new(1).ids().start(),
    };
    let unique_mail = Key::new(KeyKind::Email, "other@example.com").unwrap();
    assert!(directory
        .reserve(
            &other_peer,
            foreign,
            &[name.clone(), unique_mail.clone()],
            &nonce
        )
        .await
        .is_err());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM directory_claims WHERE object_id=$1")
        .bind(foreign.id)
        .fetch_one(&database.raw)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(directory.lookup(&unique_mail).await.unwrap().is_none());
    assert!(directory
        .reserve(
            &replacement,
            Object {
                kind: ObjectKind::User,
                id: 2
            },
            &keys,
            "hostile"
        )
        .await
        .is_err());
    assert!(Key::new(KeyKind::Ssh, "SHA256:../../keys").is_err());
    assert!(Key::new(KeyKind::Name, "../alice").is_err());
    directory.close().await;
    database.finish().await;
}

#[tokio::test]
async fn expired_directory_claims_do_not_publish_and_cleanup_releases_the_name() {
    let mut database = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&database.connection_url(), Config::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    directory.register_cell(cell(0, 'a')).await.unwrap();
    let peer = "a".repeat(64);
    let nonce = "e".repeat(64);
    let old = Object {
        kind: ObjectKind::User,
        id: 1,
    };
    let keys = vec![Key::new(KeyKind::Name, "interrupted").unwrap()];
    directory.reserve(&peer, old, &keys, &nonce).await.unwrap();
    sqlx::query(
        "UPDATE directory_objects SET expires_at=clock_timestamp()-INTERVAL '1 second' WHERE id=$1",
    )
    .bind(old.id)
    .execute(&database.raw)
    .await
    .unwrap();
    assert!(directory.confirm(&peer, old, &keys, &nonce).await.is_err());
    directory.collect().await.unwrap();
    let new = Object {
        kind: ObjectKind::User,
        id: 2,
    };
    directory.reserve(&peer, new, &keys, &nonce).await.unwrap();
    directory.confirm(&peer, new, &keys, &nonce).await.unwrap();
    assert_eq!(
        directory.lookup(&keys[0]).await.unwrap().unwrap().object,
        new
    );
    directory.close().await;
    database.finish().await;
}
