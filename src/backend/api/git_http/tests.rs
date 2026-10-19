use super::*;
use crate::backend::{ci::CiConfig, repositories::store::RepoStore, storage::ObjectStore};

async fn fixture() -> (tempfile::TempDir, Arc<AppState>, Repository, i64) {
    let root = tempfile::tempdir().unwrap();
    let pool = db::init_db(&format!(
        "sqlite://{}",
        root.path().join("test.db").display()
    ))
    .await
    .unwrap();
    db::run_migrations(&pool).await.unwrap();
    db::node_config::upsert_node_configuration(&pool, "public", true)
        .await
        .unwrap();
    let owner = db::users::accounts::create_user(&pool, "owner", None)
        .await
        .unwrap();
    let store = RepoStore::create(root.path(), "main", None).await.unwrap();
    let repo = db::repositories::records::create_repository(
        &pool,
        owner.id,
        "repo",
        None,
        false,
        store.location(),
    )
    .await
    .unwrap();
    let state = AppState::new(
        pool,
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    (root, Arc::new(state), repo, owner.id)
}

#[tokio::test]
async fn personal_administrator_git_access_keeps_token_scopes_and_org_restrictions() {
    let (_root, state, mut repo, _) = fixture().await;
    let actor = db::users::accounts::create_user(&state.pool, "admin", None)
        .await
        .unwrap();
    let db::DbPool::Sqlite(raw) = &state.pool else {
        unreachable!()
    };
    sqlx::query("UPDATE users SET is_admin=1 WHERE id=?")
        .bind(actor.id)
        .execute(raw)
        .await
        .unwrap();
    sqlx::query("UPDATE repositories SET is_private=1 WHERE id=?")
        .bind(repo.id)
        .execute(raw)
        .await
        .unwrap();
    repo.is_private = true;
    let session = "a".repeat(64);
    db::users::sessions::create_session(
        &state.pool,
        &session,
        actor.id,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    for (scope, secret) in [("read", "read-secret"), ("write", "write-secret")] {
        let token = db::users::tokens::create_token(
            &state.pool,
            actor.id,
            &session,
            scope,
            &token_hash(secret),
            scope,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("admin:{secret}")))
                .parse()
                .unwrap(),
        );
        assert!(
            authorize_repository(&state, Some(repo.clone()), &headers, false)
                .await
                .is_ok()
        );
        let write = authorize_repository(&state, Some(repo.clone()), &headers, true).await;
        if scope == "write" {
            assert!(write.is_ok());
            assert!(ReceiveCredential::Token(token.id)
                .authorize(&state.pool, &repo, true)
                .await
                .is_ok());
        } else {
            assert_eq!(write.unwrap_err().status(), StatusCode::FORBIDDEN);
            assert!(ReceiveCredential::Token(token.id)
                .authorize(&state.pool, &repo, true)
                .await
                .is_err());
        }
        let mut org_repo = repo.clone();
        org_repo.owner_kind = "org".into();
        assert!(ReceiveCredential::Token(token.id)
            .authorize(&state.pool, &org_repo, false)
            .await
            .is_err());
        db::users::tokens::delete_token(&state.pool, actor.id, token.id)
            .await
            .unwrap();
        assert!(
            authorize_repository(&state, Some(repo.clone()), &headers, false)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn advertisements_hold_download_admission_until_delivery_or_send_timeout() {
    let (_root, state, repo, _) = fixture().await;
    let bare = git2::Repository::open_bare(&repo.disk_path).unwrap();
    let tree = bare
        .find_tree(bare.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    let id = bare
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "refs",
            &tree,
            &[],
        )
        .unwrap();
    let prefix = format!("{}/", "x".repeat(200)).repeat(4);
    for index in 0..300 {
        bare.reference(&format!("refs/heads/{prefix}{index}"), id, false, "")
            .unwrap();
    }
    let response = info_refs(
        State(state.clone()),
        Path(("owner".into(), "repo".into())),
        Query(ServiceQuery {
            service: Some("git-upload-pack".into()),
        }),
        HeaderMap::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(state.repository_downloads.available_permits(), 3);
    tokio::time::pause();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    tokio::task::yield_now().await;
    assert_eq!(state.repository_downloads.available_permits(), 4);
    drop(response);
}
