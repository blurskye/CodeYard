use crate::support::database::Database;
use crate::support::http_client::Client;
use axum::http::StatusCode;
use base64::Engine;
use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
use codeyard_git_storage::repositories::lifecycle;
use serde_json::json;
use std::path::Path;

fn bytes(path: &Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            total += bytes(&entry.path());
        } else if kind.is_file() {
            total += entry.metadata().unwrap().len();
        }
    }
    total
}

/// Text that Git cannot compress much, so a whole copy is easy to tell from a shared one.
fn noise(length: usize) -> String {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b"0123456789abcdefghijklmnopqrstuvwxyz"[(state % 36) as usize])
        })
        .collect()
}

async fn objects(f: &Database, id: i64) -> std::path::PathBuf {
    let repository = codeyard_db_repos::repositories::records::get_repository_by_id(f.pool(), id)
        .await
        .unwrap()
        .unwrap();
    Path::new(&repository.disk_path).join("objects")
}

#[tokio::test]
async fn forks_of_a_public_repository_share_one_object_pool() {
    let f = Database::new(false).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let base = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    let first = alice.save("main", "README.md", "unborn").await;
    let content = noise(400 * 1024);
    alice.save_text("main", "data.txt", &content, &first).await;
    let source = bytes(&objects(&f, base.id).await);
    assert!(source > 200 * 1024, "{source}");
    let mut forks = Vec::new();
    for number in 0..100 {
        let id = f.actor(&format!("forker{number:03}")).await;
        let client = Client::login(&router, f.pool(), id).await;
        let fork = client
            .expect(
                "POST",
                "/api/repos/alice/proj/forks",
                json!({}),
                StatusCode::CREATED,
            )
            .await;
        forks.push((client, fork["repo"]["id"].as_i64().unwrap(), number));
    }
    let pool = root.path().join(format!("pools/{}.git", base.id));
    let pooled = bytes(&pool.join("objects"));
    assert!(
        pooled >= source / 2 && pooled <= source * 2,
        "{pooled} {source}"
    );
    let mut members = 0;
    for (_, id, _) in &forks {
        let objects = objects(&f, *id).await;
        assert_eq!(
            std::fs::read_to_string(objects.join("info/alternates")).unwrap(),
            format!("../../pools/{}.git/objects\n", base.id)
        );
        members += bytes(&objects);
    }
    // All 100 forks together take less room than one more copy of the source.
    assert!(members < source, "{members} {source}");
    let (client, id, number) = &forks[17];
    let shown = client
        .expect(
            "GET",
            &format!("/api/v3/repos/forker{number:03}/proj/contents/data.txt"),
            json!(null),
            StatusCode::OK,
        )
        .await;
    let shown = base64::engine::general_purpose::STANDARD
        .decode(shown["content"].as_str().unwrap().replace('\n', ""))
        .unwrap();
    assert_eq!(shown, content.as_bytes());
    // A push to one fork adds only its own new objects there, and nothing to the pool.
    let before = bytes(&objects(&f, *id).await);
    let tip = client
        .expect(
            "GET",
            &format!("/api/v3/repos/forker{number:03}/proj/commits/main"),
            json!(null),
            StatusCode::OK,
        )
        .await["sha"]
        .as_str()
        .unwrap()
        .to_owned();
    client
        .expect(
            "POST",
            &format!("/api/repos/forker{number:03}/proj/contents/fork.txt"),
            json!({"content":base64::engine::general_purpose::STANDARD.encode("fork only\n"),"message":"Fork only","branch":"main","expected_revision":tip}),
            StatusCode::CREATED,
        )
        .await;
    let after = bytes(&objects(&f, *id).await);
    assert!(
        after > before && after - before < 16 * 1024,
        "{before} {after}"
    );
    assert_eq!(bytes(&pool.join("objects")), pooled);
}

#[tokio::test]
async fn forks_of_a_private_repository_keep_whole_copies() {
    let f = Database::new(false).await;
    f.complete_setup().await;
    let alice_id = f.actor("alice").await;
    let bob_id = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let base = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        RepositoryCreationOwner::personal(alice_id),
        "proj",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let alice = Client::login(&router, f.pool(), alice_id).await;
    // Forking is allowed by default.
    alice.save("main", "README.md", "unborn").await;
    codeyard_db_repos::repositories::access::grant_repo_access(
        f.pool(),
        bob_id,
        base.id,
        codeyard_database::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let bob = Client::login(&router, f.pool(), bob_id).await;
    let fork = bob
        .expect(
            "POST",
            "/api/repos/alice/proj/forks",
            json!({}),
            StatusCode::CREATED,
        )
        .await;
    let objects = objects(&f, fork["repo"]["id"].as_i64().unwrap()).await;
    assert!(!objects.join("info/alternates").exists());
    assert!(!root.path().join("pools").exists());
}
