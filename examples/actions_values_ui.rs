//! Browser fixture runner: build the real UI, then `cargo run --example actions_values_ui`.
//! This is not an automatic test target: ordinary native tests need no browser bundle.
use codeyard_api_rest::create_router;
use codeyard_app::app::AppState;
use codeyard_ci::config::CiConfig;
use codeyard_db_actions::environment_values as environments;
use codeyard_db_actions::environment_values::Save;
use codeyard_db_orgs::organization_values::Kind;
use codeyard_repos_core::repositories::lifecycle;
use codeyard_storage::ObjectStore;
use codeyard_support::secrets::SecretEncryptor;
use std::{path::Path, sync::Arc, time::Duration};

#[tokio::main]
async fn main() {
    let dist = std::env::var("WEBUI_DIST").expect("WEBUI_DIST must contain the real browser build");
    assert!(Path::new(&dist).join("index.html").is_file());
    let python =
        std::env::var("CODEYARD_BROWSER_PYTHON").expect("the pinned browser Python is required");
    let root = tempfile::tempdir().unwrap();
    let pool =
        codeyard_db_core::init_db(&format!("sqlite://{}", root.path().join("ui.db").display()))
            .await
            .unwrap();
    codeyard_db_core::run_migrations(&pool).await.unwrap();
    codeyard_db_cluster::node_config::upsert_node_configuration(&pool, "public", true)
        .await
        .unwrap();
    let owner = codeyard_db_identity::users::accounts::create_user(&pool, "alice", None)
        .await
        .unwrap()
        .id;
    let repo = lifecycle::provision(
        (&pool, None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner),
        "project",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let session = uuid::Uuid::new_v4().simple().to_string().repeat(2);
    codeyard_db_identity::users::sessions::create_session(
        &pool,
        &session,
        owner,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let credential =
        codeyard_db_core::queries::users::accounts::AccountCredential::Session(&session);
    let encryptor = Arc::new(SecretEncryptor::new(&[0x63; 32]));
    codeyard_db_actions::secrets::create_or_update_secret(
        &pool,
        &encryptor,
        repo.id,
        "REPOSITORY_TOKEN",
        "repository-browser-private",
        Some(owner),
        &credential,
    )
    .await
    .unwrap();
    codeyard_db_actions::variables::save(
        &pool,
        owner,
        &credential,
        repo.id,
        None,
        "REGION",
        Some("repo-visible"),
    )
    .await
    .unwrap();
    for name in ["Production".to_owned(), "L".repeat(255)] {
        environments::create(&pool, owner, repo.id, &name)
            .await
            .unwrap();
    }
    for (kind, name, value) in [
        (
            Kind::Secret,
            "ENVIRONMENT_TOKEN",
            "environment-browser-private",
        ),
        (Kind::Variable, "REGION", "env-visible"),
    ] {
        environments::save(
            &pool,
            Some(&encryptor),
            owner,
            repo.id,
            "Production",
            &Save {
                kind,
                previous: None,
                name,
                value: Some(value),
            },
        )
        .await
        .unwrap();
    }
    let org = codeyard_db_orgs::organizations::profiles::create_organization(
        &pool,
        owner,
        &codeyard_db_orgs::organizations::profiles::NewOrganization::new(
            "browser_org",
            codeyard_db_orgs::organizations::profiles::OrganizationProfile::new(None, None)
                .unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(org) = org else {
        panic!("browser organization must be created")
    };
    for (kind, name, value) in [
        (
            Kind::Secret,
            "ORGANIZATION_TOKEN",
            "organization-browser-private",
        ),
        (Kind::Variable, "ORGANIZATION_REGION", "org-visible"),
    ] {
        codeyard_db_orgs::organization_values::save(
            &pool,
            Some(&encryptor),
            owner,
            &credential,
            org.id,
            &codeyard_db_orgs::organization_values::Save {
                kind,
                previous: None,
                name,
                value: Some(value),
                visibility: Some("all"),
                repositories: None,
            },
        )
        .await
        .unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::env::set_var("PUBLIC_URL", &base);
    let router = create_router(AppState::new(
        pool.clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        Some(encryptor),
        CiConfig::default(),
        None,
    ));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut command = tokio::process::Command::new(python);
    command
        .args([
            "scripts/test-actions-values-ui.py",
            &base,
            "/alice/project",
            &format!("/settings/organizations/{}?tab=actions", org.id),
        ])
        .env("CODEYARD_TEST_SESSION", &session)
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(180), command.output()).await;
    server.abort();
    let output = result.expect("the browser check must finish").unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
    pool.close().await;
}
