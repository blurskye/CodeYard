use super::*;
use crate::git_http::authorize_repository;
use crate::git_http::info_refs;
use crate::git_http::repository;
use crate::git_http::ServiceQuery;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use codeyard_app::app::AppState;
use codeyard_app::ci::config::CiConfig;
use codeyard_app::repositories::git_sessions::ReceiveCredential;
use codeyard_app::repositories::store::RepoStore;
use codeyard_app::storage::ObjectStore;

async fn fixture() -> (tempfile::TempDir, Arc<AppState>, Repository, i64) {
    let root = tempfile::tempdir().unwrap();
    let pool = codeyard_db_core::init_db(&format!(
        "sqlite://{}",
        root.path().join("test.db").display()
    ))
    .await
    .unwrap();
    codeyard_db_core::run_migrations(&pool).await.unwrap();
    codeyard_db_cluster::node_config::upsert_node_configuration(&pool, "public", true)
        .await
        .unwrap();
    let owner = codeyard_db_identity::users::accounts::create_user(&pool, "owner", None)
        .await
        .unwrap();
    let store = RepoStore::create(root.path(), "main", None).await.unwrap();
    let repo = codeyard_db_repos::repositories::records::create_repository(
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
    let (_root, state, mut repo, owner) = fixture().await;
    let actor = codeyard_db_identity::users::accounts::create_user(&state.pool, "admin", None)
        .await
        .unwrap();
    let codeyard_db_core::DbPool::Sqlite(raw) = &state.pool else {
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
    let organization = match codeyard_db_orgs::organizations::profiles::create_organization(
        &state.pool,
        owner,
        &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
            "acme",
            codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                .unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(organization) => {
            organization
        }
        outcome => panic!("organization setup failed: {outcome:?}"),
    };
    let session = "a".repeat(64);
    codeyard_db_identity::users::sessions::create_session(
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
        let token = codeyard_db_identity::users::tokens::create_token(
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
        // An administrator's access to personal repositories does not extend to an
        // organization's; access is read from the stored repository, so move it there.
        sqlx::query("UPDATE repositories SET owner_kind='org', owner_user_id=NULL, owner_org_id=? WHERE id=?")
            .bind(organization.id)
            .bind(repo.id)
            .execute(raw)
            .await
            .unwrap();
        let mut org_repo = repo.clone();
        org_repo.owner_kind = "org".into();
        org_repo.owner_user_id = None;
        org_repo.owner_org_id = Some(organization.id);
        assert!(ReceiveCredential::Token(token.id)
            .authorize(&state.pool, &org_repo, false)
            .await
            .is_err());
        sqlx::query("UPDATE repositories SET owner_kind='user', owner_org_id=NULL, owner_user_id=? WHERE id=?")
            .bind(owner)
            .bind(repo.id)
            .execute(raw)
            .await
            .unwrap();
        codeyard_db_identity::users::tokens::delete_token(
            &state.pool,
            actor.id,
            &session,
            token.id,
        )
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
