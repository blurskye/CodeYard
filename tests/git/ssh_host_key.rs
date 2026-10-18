#![cfg(not(target_arch = "wasm32"))]

use crate::support::native_database as support;

use codeyard::backend::{
    ci::{CiConfig, CiRuns},
    db,
    notifications::NotificationBroadcaster,
    ssh::SshServer,
};
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{ssh_key::LineEnding, PrivateKey};
use std::sync::Arc;
use support::{NativeDatabase, PostgresDatabase};

const ENCRYPTED_HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABDTiQtTib\n4EhblcgWulXp73AAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAID47r0UUpoMOSTWG\n/uAinUaImF8QTKuDZeHRQWk+5a2XAAAAkIbn9mDw5RqlIgZCb76umHQs9eu2/uYuG8/nZv\noYCLUV0c9QTCZPtphoXzyt8BPWPJSMG0QOyiq71OUwpVLlIbrJHdNxaFoD3Rfe3c4urdwl\nTmQbNZY9TF5d3k69HaBRdckdluVDoV+tiWcJ3omBcw5OysWNe1ZDEbs5Q+dTOgEMnAEcaK\nVZWBAXGeVqb/tCXg==\n-----END OPENSSH PRIVATE KEY-----\n";

fn host_key(seed: u8) -> String {
    PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]))
        .to_openssh(LineEnding::LF)
        .unwrap()
        .to_string()
}

fn ci_service(pool: &db::DbPool) -> Arc<CiRuns> {
    Arc::new(CiRuns::new(
        pool.clone(),
        Arc::new(CiConfig::default()),
        Arc::new(tokio::sync::Notify::new()),
        None,
        Arc::new(NotificationBroadcaster::new(pool)),
    ))
}

async fn concurrent_inserts_keep_one_key(pool: &db::DbPool, other: &db::DbPool) {
    assert!(db::node_config::get_cluster_ssh_host_key(pool)
        .await
        .unwrap()
        .is_none());
    assert!(db::node_config::get_cluster_ssh_host_key(other)
        .await
        .unwrap()
        .is_none());
    let first_key = host_key(1);
    let second_key = host_key(2);
    let (first, second) = tokio::join!(
        db::node_config::insert_cluster_ssh_host_key(pool, &first_key),
        db::node_config::insert_cluster_ssh_host_key(other, &second_key),
    );
    first.unwrap();
    second.unwrap();

    let winner = db::node_config::get_cluster_ssh_host_key(pool)
        .await
        .unwrap()
        .unwrap();
    assert!(winner == first_key || winner == second_key);
    assert_eq!(
        db::node_config::get_cluster_ssh_host_key(other)
            .await
            .unwrap()
            .unwrap(),
        winner
    );
    let loser = if winner == first_key {
        &second_key
    } else {
        &first_key
    };
    db::node_config::insert_cluster_ssh_host_key(other, loser)
        .await
        .unwrap();
    assert_eq!(
        db::node_config::get_cluster_ssh_host_key(pool)
            .await
            .unwrap()
            .unwrap(),
        winner
    );
}

async fn invalid_bootstrap_does_not_poison_database(pool: &db::DbPool) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("host_key");
    for invalid in ["not a private key", ENCRYPTED_HOST_KEY] {
        std::fs::write(&path, invalid).unwrap();
        let result =
            SshServer::new(pool.clone(), Some(path.to_str().unwrap()), ci_service(pool)).await;
        assert!(result.is_err());
        assert!(db::node_config::get_cluster_ssh_host_key(pool)
            .await
            .unwrap()
            .is_none());
    }

    let valid = host_key(9);
    std::fs::write(&path, &valid).unwrap();
    let server = SshServer::new(pool.clone(), Some(path.to_str().unwrap()), ci_service(pool))
        .await
        .unwrap();
    assert_eq!(
        db::node_config::get_cluster_ssh_host_key(pool)
            .await
            .unwrap()
            .unwrap(),
        valid
    );
    drop(server);
}

async fn simultaneous_startups_share_the_saved_key(pool: &db::DbPool, other: &db::DbPool) {
    let directory = tempfile::tempdir().unwrap();
    let first_path = directory.path().join("first_host_key");
    let second_path = directory.path().join("second_host_key");
    let first_key = host_key(3);
    let second_key = host_key(4);
    std::fs::write(&first_path, &first_key).unwrap();
    std::fs::write(&second_path, &second_key).unwrap();
    let ci = ci_service(pool);
    let (first, second) = tokio::join!(
        SshServer::new(pool.clone(), Some(first_path.to_str().unwrap()), ci.clone()),
        SshServer::new(
            other.clone(),
            Some(second_path.to_str().unwrap()),
            ci.clone()
        ),
    );
    let first = first.unwrap();
    let second = second.unwrap();
    let winner = db::node_config::get_cluster_ssh_host_key(pool)
        .await
        .unwrap()
        .unwrap();
    assert!(winner == first_key || winner == second_key);
    assert_eq!(std::fs::read_to_string(&first_path).unwrap(), winner);
    assert_eq!(std::fs::read_to_string(&second_path).unwrap(), winner);
    drop((first, second));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&first_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let restarted =
            SshServer::new(pool.clone(), Some(first_path.to_str().unwrap()), ci.clone())
                .await
                .unwrap();
        assert_eq!(
            std::fs::metadata(&first_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_to_string(&first_path).unwrap(), winner);
        drop(restarted);
    }

    std::fs::write(&first_path, host_key(5)).unwrap();
    let restarted = SshServer::new(pool.clone(), Some(first_path.to_str().unwrap()), ci)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(first_path).unwrap(), winner);
    assert_eq!(
        db::node_config::get_cluster_ssh_host_key(other)
            .await
            .unwrap()
            .unwrap(),
        winner
    );
    drop(restarted);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_host_key_inserts_do_not_replace_the_winner() {
    let fixture = NativeDatabase::new().await;
    let other = fixture.second_handle().await;
    concurrent_inserts_keep_one_key(&fixture.pool, &other).await;
    other.close().await;
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_host_key_inserts_do_not_replace_the_winner() {
    let fixture = PostgresDatabase::new().await;
    concurrent_inserts_keep_one_key(&fixture.pool, &fixture.pool).await;
}

#[tokio::test]
async fn sqlite_invalid_bootstrap_leaves_database_empty_then_valid_startup_succeeds() {
    let fixture = NativeDatabase::new().await;
    invalid_bootstrap_does_not_poison_database(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_invalid_bootstrap_leaves_database_empty_then_valid_startup_succeeds() {
    let fixture = PostgresDatabase::new().await;
    invalid_bootstrap_does_not_poison_database(&fixture.pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_simultaneous_startups_use_the_saved_host_key() {
    let fixture = NativeDatabase::new().await;
    let other = fixture.second_handle().await;
    simultaneous_startups_share_the_saved_key(&fixture.pool, &other).await;
    other.close().await;
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_simultaneous_startups_use_the_saved_host_key() {
    let fixture = PostgresDatabase::new().await;
    simultaneous_startups_share_the_saved_key(&fixture.pool, &fixture.pool).await;
}

#[tokio::test]
async fn startup_returns_configured_host_key_file_errors() {
    let fixture = NativeDatabase::new().await;
    let key = host_key(6);
    db::node_config::insert_cluster_ssh_host_key(&fixture.pool, &key)
        .await
        .unwrap();
    let result = SshServer::new(
        fixture.pool.clone(),
        Some(fixture.installation_directory().to_str().unwrap()),
        ci_service(&fixture.pool),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        db::node_config::get_cluster_ssh_host_key(&fixture.pool)
            .await
            .unwrap()
            .unwrap(),
        key
    );
    fixture.close().await;
}
