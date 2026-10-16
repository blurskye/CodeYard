//! Who may create repositories (personal, organization, forks), on SQLite and
//! PostgreSQL, plus creating a real bare repository on disk.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
#[path = "support/fake_webui_bundle.rs"]
mod bundle;
#[path = "support/database.rs"]
mod database;
#[path = "org_repository_creation/fixture.rs"]
mod fixture;
#[path = "support/pull_requests.rs"]
mod http;
#[path = "support/native_database.rs"]
mod native;
#[path = "support/production_ci.rs"]
mod production;

use codeyard::backend::{
    db::{
        self,
        models::{AccessLevel, OrgRole, Repository},
        organizations::{self as orgs, OrganizationOutcome},
        repositories::creation::{
            self as creation, NewRepository, RepositoryCreationOwner, RepositoryStorageRefusal,
            RepositoryTarget,
        },
    },
    repositories::lifecycle,
};
use database::Database;

fn owner(actor_id: i64, target: RepositoryTarget) -> RepositoryCreationOwner {
    RepositoryCreationOwner { actor_id, target }
}
fn applied<T: std::fmt::Debug>(outcome: OrganizationOutcome<T>) -> T {
    match outcome {
        OrganizationOutcome::Applied(value) => value,
        other => panic!("expected Applied: {other:?}"),
    }
}
async fn actor(database: &Database, name: &str) -> i64 {
    db::users::accounts::create_user(database.pool(), name, None)
        .await
        .unwrap()
        .id
}
async fn organization(database: &Database, actor: i64, slug: &str) -> i64 {
    applied(
        orgs::create_organization(
            database.pool(),
            actor,
            &orgs::NewOrganization::new(slug, orgs::OrganizationProfile::new(None, None).unwrap())
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id
}
async fn member(database: &Database, actor: i64, org: i64, user: i64, role: OrgRole) {
    applied(
        orgs::add_organization_member(database.pool(), actor, org, user, role)
            .await
            .unwrap(),
    );
}
async fn create(
    database: &Database,
    creation_owner: RepositoryCreationOwner,
    name: &str,
    private: bool,
    source: Option<i64>,
) -> anyhow::Result<Option<Repository>> {
    creation::insert_repository(
        database.pool(),
        NewRepository {
            owner: creation_owner,
            name,
            description: Some("Fixture"),
            is_private: private,
            disk_path: &format!("/fixture/{name}.git"),
            storage_node_id: None,
            source_id: source,
        },
    )
    .await
}
fn denied<T: std::fmt::Debug>(result: anyhow::Result<T>, refusal: RepositoryStorageRefusal) {
    let error = result.expect_err("must refuse");
    assert_eq!(
        error.downcast_ref::<RepositoryStorageRefusal>(),
        Some(&refusal)
    );
}

async fn creation_contract(mut database: Database) {
    let _unrelated = actor(&database, "unrelated").await;
    let own = actor(&database, "tenant_owner").await;
    let admin = actor(&database, "tenant_admin").await;
    let ordinary = actor(&database, "tenant_member").await;
    let direct_writer = actor(&database, "direct_writer").await;
    let siteadmin = actor(&database, "siteadmin").await;
    let unknown = actor(&database, "unknown_role").await;
    let org = organization(&database, own, "creation_org").await;
    member(&database, own, org, admin, OrgRole::Admin).await;
    member(&database, own, org, ordinary, OrgRole::Member).await;
    member(&database, own, org, unknown, OrgRole::Member).await;
    database
        .execute(
            "UPDATE org_memberships SET role='future_role' WHERE org_id=$1 AND user_id=$2",
            &[org, unknown],
        )
        .await;
    database
        .execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[siteadmin])
        .await;
    // A user whose id equals the org id must not count as the org's owner.
    let collision = db::users::accounts::get_user_by_id(database.pool(), org)
        .await
        .unwrap()
        .unwrap()
        .id;
    assert_ne!(collision, own);

    let personal = create(
        &database,
        owner(own, RepositoryTarget::Personal(own)),
        "self",
        false,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(personal.owner_kind, "user");
    assert_eq!(personal.owner_user_id, Some(own));
    assert_eq!(personal.owner_org_id, None);

    for (index, creator) in [own, admin].into_iter().enumerate() {
        let name = format!("authorized_{index}");
        let created = create(
            &database,
            owner(creator, RepositoryTarget::Organization(org)),
            &name,
            false,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(created.owner_kind, "org");
        assert_eq!(created.owner_user_id, None);
        assert_eq!(created.owner_org_id, Some(org));
        assert!(!created.is_fork);
        assert_eq!(created.default_branch, "main");
        // The same name again is a conflict, not a second row.
        let count = database.count().await;
        assert!(create(
            &database,
            owner(creator, RepositoryTarget::Organization(org)),
            &name,
            false,
            None
        )
        .await
        .unwrap()
        .is_none());
        assert_eq!(database.count().await, count);
        // Write access to one org repository doesn't allow creating others.
        db::repositories::access::grant_repo_access(
            database.pool(),
            direct_writer,
            created.id,
            AccessLevel::Write,
        )
        .await
        .unwrap();
    }
    for (index, creation_owner) in [
        owner(own, RepositoryTarget::Personal(admin)),
        owner(siteadmin, RepositoryTarget::Personal(own)),
        owner(ordinary, RepositoryTarget::Organization(org)),
        owner(direct_writer, RepositoryTarget::Organization(org)),
        owner(siteadmin, RepositoryTarget::Organization(org)),
        owner(collision, RepositoryTarget::Organization(org)),
        owner(unknown, RepositoryTarget::Organization(org)),
        owner(own, RepositoryTarget::Organization(i64::MAX)),
    ]
    .into_iter()
    .enumerate()
    {
        let count = database.count().await;
        denied(
            create(
                &database,
                creation_owner,
                &format!("denied_{index}"),
                false,
                None,
            )
            .await,
            RepositoryStorageRefusal::Denied,
        );
        assert_eq!(database.count().await, count);
    }
    for creator in [own, admin] {
        denied(
            create(
                &database,
                owner(creator, RepositoryTarget::Organization(org)),
                "org_fork",
                false,
                Some(personal.id),
            )
            .await,
            RepositoryStorageRefusal::UnsupportedOrganizationFork,
        );
    }
    // Banned or password-reset users, and demoted or removed admins, can't create.
    for column in ["is_banned", "must_change_password"] {
        database
            .execute(
                &format!("UPDATE users SET {column}=TRUE WHERE id=$1"),
                &[admin],
            )
            .await;
        for target in [
            RepositoryTarget::Personal(admin),
            RepositoryTarget::Organization(org),
        ] {
            denied(
                create(&database, owner(admin, target), "restricted", false, None).await,
                RepositoryStorageRefusal::Denied,
            );
        }
        database
            .execute(
                &format!("UPDATE users SET {column}=FALSE WHERE id=$1"),
                &[admin],
            )
            .await;
    }
    applied(
        orgs::update_organization_member_role(database.pool(), own, org, admin, OrgRole::Member)
            .await
            .unwrap(),
    );
    denied(
        create(
            &database,
            owner(admin, RepositoryTarget::Organization(org)),
            "demoted",
            false,
            None,
        )
        .await,
        RepositoryStorageRefusal::Denied,
    );
    applied(
        orgs::remove_organization_member(database.pool(), own, org, admin)
            .await
            .unwrap(),
    );
    denied(
        create(
            &database,
            owner(admin, RepositoryTarget::Organization(org)),
            "removed",
            false,
            None,
        )
        .await,
        RepositoryStorageRefusal::Denied,
    );
    database.finish().await;
}

async fn personal_fork_contract(mut database: Database) {
    let source_owner = actor(&database, "fork_source_owner").await;
    let creator = actor(&database, "fork_creator").await;
    let private_source = create(
        &database,
        owner(source_owner, RepositoryTarget::Personal(source_owner)),
        "private_source",
        true,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    // No read access to a private source: no fork.
    denied(
        create(
            &database,
            owner(creator, RepositoryTarget::Personal(creator)),
            "denied_fork",
            false,
            Some(private_source.id),
        )
        .await,
        RepositoryStorageRefusal::Denied,
    );
    db::repositories::access::grant_repo_access(
        database.pool(),
        creator,
        private_source.id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    let fork = create(
        &database,
        owner(creator, RepositoryTarget::Personal(creator)),
        "private_fork",
        false,
        Some(private_source.id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        fork.is_private,
        "a fork of a private repository stays private"
    );
    assert!(fork.is_fork);
    assert_eq!(fork.forked_from_id, Some(private_source.id));
    // One fork per user and source, even under another name.
    assert!(create(
        &database,
        owner(creator, RepositoryTarget::Personal(creator)),
        "second_fork",
        false,
        Some(private_source.id)
    )
    .await
    .unwrap()
    .is_none());
    let public_source = create(
        &database,
        owner(source_owner, RepositoryTarget::Personal(source_owner)),
        "public_source",
        false,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let public_fork = create(
        &database,
        owner(creator, RepositoryTarget::Personal(creator)),
        "public_fork",
        false,
        Some(public_source.id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!public_fork.is_private);
    // Making the source private narrows existing forks too.
    database
        .execute(
            "UPDATE repositories SET is_private=TRUE WHERE id=$1",
            &[public_source.id],
        )
        .await;
    assert!(database.repository(public_fork.id).await.is_private);
    database.finish().await;
}

async fn provision_contract(mut database: Database) {
    let own = actor(&database, "domain_owner").await;
    let org = organization(&database, own, "domain_org").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (database.pool(), None),
        root.path(),
        owner(own, RepositoryTarget::Organization(org)),
        "real",
        Some("Real repository"),
        false,
        None,
    )
    .await
    .expect("provision a bare repository");
    assert_eq!(repo.storage_node_id, None);
    let path = std::path::Path::new(&repo.disk_path);
    assert!(path.starts_with(root.path()));
    let git = git2::Repository::open_bare(path).expect("a real bare repository");
    assert_eq!(
        git.find_reference("HEAD").unwrap().symbolic_target(),
        Some("refs/heads/main")
    );
    // A duplicate name leaves no directory behind.
    let entries = std::fs::read_dir(root.path()).unwrap().count();
    let duplicate = lifecycle::provision(
        (database.pool(), None),
        root.path(),
        owner(own, RepositoryTarget::Organization(org)),
        "real",
        None,
        false,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(
        duplicate.kind,
        codeyard::backend::repositories::lifecycle::LifecycleFailure::Conflict
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), entries);
    database.finish().await;
}

async fn initialization_cleanup_contract(mut database: Database) {
    let source_owner = actor(&database, "cleanup_source_owner").await;
    let creator = actor(&database, "cleanup_owner").await;
    let root = tempfile::tempdir().unwrap();
    let source = lifecycle::provision(
        (database.pool(), None),
        root.path(),
        owner(source_owner, RepositoryTarget::Personal(source_owner)),
        "cleanup-source",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let mut unavailable_source = source.clone();
    unavailable_source.disk_path = root
        .path()
        .join("missing-source.git")
        .to_string_lossy()
        .into_owned();
    let original_entries = std::fs::read_dir(root.path()).unwrap().count();

    let cleaned = lifecycle::provision(
        (database.pool(), None),
        root.path(),
        owner(creator, RepositoryTarget::Personal(creator)),
        "cleanup-complete",
        None,
        false,
        Some(&unavailable_source),
    )
    .await
    .unwrap_err();
    assert_eq!(cleaned.kind, lifecycle::LifecycleFailure::Unavailable);
    assert!(db::repositories::records::get_repository(
        database.pool(),
        "cleanup_owner",
        "cleanup-complete"
    )
    .await
    .unwrap()
    .is_none());
    assert_eq!(
        std::fs::read_dir(root.path()).unwrap().count(),
        original_entries
    );

    match &database {
        Database::Postgres(fixture) => {
            sqlx::query(
                r#"CREATE FUNCTION reject_partial_repository_delete() RETURNS trigger LANGUAGE plpgsql AS $$
                BEGIN
                    IF OLD.name = 'cleanup-partial' THEN
                        RAISE EXCEPTION 'injected repository delete failure';
                    END IF;
                    RETURN OLD;
                END;
                $$"#,
            )
            .execute(&fixture.raw)
            .await
            .unwrap();
            sqlx::query(
                "CREATE TRIGGER reject_partial_repository_delete BEFORE DELETE ON repositories FOR EACH ROW EXECUTE FUNCTION reject_partial_repository_delete()",
            )
            .execute(&fixture.raw)
            .await
            .unwrap();
        }
        Database::Sqlite(fixture) => {
            sqlx::query(
                "CREATE TRIGGER reject_partial_repository_delete BEFORE DELETE ON repositories WHEN OLD.name = 'cleanup-partial' BEGIN SELECT RAISE(FAIL, 'injected repository delete failure'); END",
            )
            .execute(&fixture.raw)
            .await
            .unwrap();
        }
    }
    let partial = lifecycle::provision(
        (database.pool(), None),
        root.path(),
        owner(creator, RepositoryTarget::Personal(creator)),
        "cleanup-partial",
        None,
        false,
        Some(&unavailable_source),
    )
    .await
    .unwrap_err();
    assert_eq!(
        partial.kind,
        lifecycle::LifecycleFailure::InitializationCleanupFailed
    );
    let remaining = db::repositories::records::get_repository(
        database.pool(),
        "cleanup_owner",
        "cleanup-partial",
    )
    .await
    .unwrap()
    .expect("failed row deletion must be reported and retained");
    assert!(!std::path::Path::new(&remaining.disk_path).exists());
    assert_eq!(
        std::fs::read_dir(root.path()).unwrap().count(),
        original_entries
    );
    let path = remaining.disk_path.clone();
    let retry = lifecycle::provision(
        (database.pool(), None),
        root.path(),
        owner(creator, RepositoryTarget::Personal(creator)),
        "cleanup-partial",
        None,
        false,
        Some(&unavailable_source),
    )
    .await
    .unwrap_err();
    assert_eq!(retry.kind, lifecycle::LifecycleFailure::Conflict);
    let after_retry = db::repositories::records::get_repository(
        database.pool(),
        "cleanup_owner",
        "cleanup-partial",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(after_retry.id, remaining.id);
    assert_eq!(after_retry.disk_path, path);
    assert!(!std::path::Path::new(&path).exists());
    database.finish().await;
}

async fn shared_fixture_lifecycle(pg: bool) {
    let mut database = Database::new(pg).await;
    let mut other = Database::new(pg).await;
    assert_eq!(database.count().await, i64::from(pg));
    assert_eq!(
        db::users::accounts::count_users(database.pool())
            .await
            .unwrap(),
        i64::from(pg)
    );
    let schema: Option<String> = match &database {
        Database::Postgres(f) => {
            assert!(pg);
            assert!(matches!(database.pool(), db::DbPool::Postgres(_)));
            let user = db::users::accounts::get_user_by_id(database.pool(), f.owner.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(user.username, "ci_owner");
            let repository =
                db::repositories::records::get_repository_by_id(database.pool(), f.repo.id)
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(repository.owner_user_id, Some(user.id));
            assert_eq!(repository.name, "fixture");
            assert!(repository.is_private);
            let git = git2::Repository::open_bare(&repository.disk_path).unwrap();
            assert_eq!(
                git.find_reference("refs/heads/main")
                    .unwrap()
                    .target()
                    .unwrap()
                    .to_string(),
                f.sha
            );
            Some(
                sqlx::query_scalar("SELECT current_schema()")
                    .fetch_one(&f.raw)
                    .await
                    .unwrap(),
            )
        }
        Database::Sqlite(_) => {
            assert!(!pg);
            assert!(matches!(database.pool(), db::DbPool::Sqlite(_)));
            None
        }
    };
    let target = actor(&database, "adapter_target").await;
    let untouched = actor(&database, "adapter_untouched").await;
    database
        .execute("UPDATE users SET display_name='raw fixture'", &[])
        .await;
    database
        .execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[target])
        .await;
    database
        .execute(
            "UPDATE users SET is_banned=TRUE WHERE id=$1 AND id<>$2",
            &[target, untouched],
        )
        .await;
    let target = db::users::accounts::get_user_by_id(database.pool(), target)
        .await
        .unwrap()
        .unwrap();
    let untouched = db::users::accounts::get_user_by_id(database.pool(), untouched)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(target.display_name.as_deref(), Some("raw fixture"));
    assert_eq!(untouched.display_name.as_deref(), Some("raw fixture"));
    assert!(target.is_admin && target.is_banned);
    assert!(!untouched.is_admin && !untouched.is_banned);
    assert!(
        db::users::accounts::get_user_by_username(other.pool(), "adapter_target")
            .await
            .unwrap()
            .is_none()
    );

    let pool = database.pool().clone();
    database.finish().await;
    database.finish().await;
    let error = db::users::accounts::count_users(&pool).await.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<sqlx::Error>(),
        Some(sqlx::Error::PoolClosed)
    ));
    match &database {
        Database::Postgres(f) => assert!(f.raw.is_closed()),
        Database::Sqlite(f) => assert!(f.raw.is_closed()),
    }
    if let Database::Postgres(f) = &other {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname=$1)")
                .bind(schema.unwrap())
                .fetch_one(&f.raw)
                .await
                .unwrap();
        assert!(!exists, "finish must drop the owned PostgreSQL schema");
    }
    assert_eq!(other.count().await, i64::from(pg));
    assert_eq!(
        db::users::accounts::count_users(other.pool())
            .await
            .unwrap(),
        i64::from(pg)
    );
    other.finish().await;
}

#[tokio::test]
async fn sqlite_shared_fixture_lifecycle() {
    shared_fixture_lifecycle(false).await;
}
#[tokio::test]
async fn postgres_shared_fixture_lifecycle() {
    shared_fixture_lifecycle(true).await;
}

async fn stale_fork_placement_contract(mut database: Database) {
    let source_owner = actor(&database, "placement_owner").await;
    let creator = actor(&database, "placement_forker").await;
    let mut nodes = Vec::new();
    for name in ["old-primary", "new-primary"] {
        let key = codeyard::backend::cluster::NodeKey::generate().public_hex();
        nodes.push(
            db::cluster::register_own_node(
                database.pool(),
                &key,
                name,
                &["storage".into()],
                "test",
            )
            .await
            .unwrap()
            .id,
        );
        // A promotion needs its target to be reporting in.
        db::cluster::record_node_heartbeat(database.pool(), &key, "test", "{}", None)
            .await
            .unwrap();
    }
    let source = create(
        &database,
        RepositoryCreationOwner::personal(source_owner),
        "placed",
        false,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    db::cluster::place_repository(database.pool(), source.id, nodes[0])
        .await
        .unwrap();
    let stale = database.repository(source.id).await;
    db::cluster::insert_replica(database.pool(), source.id, nodes[1], nodes[0])
        .await
        .unwrap();
    db::cluster::set_applied_version(database.pool(), source.id, nodes[1], nodes[0], 0)
        .await
        .unwrap();
    assert!(matches!(
        db::cluster::promote_repository(database.pool(), source.id, nodes[0], nodes[1], true)
            .await
            .unwrap(),
        db::cluster::PromoteOutcome::Promoted { .. }
    ));
    let before = database.count().await;
    for placement in [stale.storage_node_id, None, Some(nodes[1])] {
        let result = creation::insert_repository(
            database.pool(),
            NewRepository {
                owner: RepositoryCreationOwner::personal(creator),
                name: "placed-fork",
                description: None,
                is_private: false,
                disk_path: "/fixture/placed-fork.git",
                storage_node_id: placement,
                source_id: Some(source.id),
            },
        )
        .await;
        if placement == Some(nodes[1]) {
            assert_eq!(result.unwrap().unwrap().storage_node_id, placement);
        } else {
            denied(result, RepositoryStorageRefusal::StalePlacement);
            assert_eq!(database.count().await, before);
            assert_eq!(
                database.repository(source.id).await.forks_count,
                stale.forks_count
            );
        }
    }
    database.finish().await;
}

#[tokio::test]
async fn sqlite_fork_refuses_stale_placement() {
    stale_fork_placement_contract(Database::new(false).await).await;
}
#[tokio::test]
async fn postgres_fork_refuses_stale_placement() {
    stale_fork_placement_contract(Database::new(true).await).await;
}

async fn fork_publication_contract(test: &str, pg: bool, remote: bool) {
    use axum::http::StatusCode;
    use codeyard::backend::{
        api::create_router,
        app::AppState,
        ci::CiConfig,
        cluster,
        repositories::store::{repo_store, FileWrite},
        storage::ObjectStore,
    };
    use serde_json::Value;
    use std::sync::Arc;
    let Some(gate) = fixture::fork_process(test).await else {
        return;
    };
    bundle::install();
    let mut database = Database::new(pg).await;
    db::node_config::upsert_node_configuration(database.pool(), "public", true)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let storage_root = tempfile::tempdir().unwrap();
    let mut nodes = Vec::new();
    if remote {
        for (name, roles, disk) in [
            ("fork-web", "web", root.path()),
            ("fork-storage", "storage", storage_root.path()),
        ] {
            let settings = cluster::Settings::from_lookup(&|key| match key {
                "IROH_BIND" => Some("127.0.0.1:0".into()),
                "NODE_NAME" => Some(name.into()),
                _ => None,
            })
            .unwrap()
            .unwrap();
            let node = cluster::start(
                database.pool(),
                &disk.join("state"),
                disk,
                &roles.parse().unwrap(),
                settings,
            )
            .await
            .unwrap();
            node.publish().await.unwrap();
            nodes.push(node);
        }
    }
    let context = nodes.first().map(|node| &node.context);
    let owner = actor(&database, "snapshot_owner").await;
    let forker = actor(&database, "snapshot_forker").await;
    let source = lifecycle::provision(
        (database.pool(), context),
        root.path(),
        RepositoryCreationOwner::personal(owner),
        "snapshot",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let source_store = repo_store(&source, nodes.last().map(|node| &node.context));
    let write = |file: &str, revision: &str| FileWrite {
        branch: "main".into(),
        path: file.into(),
        message: "snapshot".into(),
        expected_revision: revision.into(),
        creating: true,
        deleting: false,
    };
    let initial = source_store
        .save_file_as(
            database.pool(),
            source.id,
            owner,
            &write("initial.txt", "unborn"),
            b"initial",
            "Owner",
            "owner@example.invalid",
        )
        .await
        .unwrap();
    db::repositories::access::grant_repo_access(
        database.pool(),
        forker,
        source.id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        database.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        context.cloned(),
    ));
    let client = http::Client::login(&router, database.pool(), forker).await;
    let creator = http::Client::login(&router, database.pool(), forker).await;
    let pending = tokio::spawn(async move {
        creator
            .api(
                "POST",
                "/api/repos/snapshot_owner/snapshot/forks",
                Value::Null,
            )
            .await
    });
    fixture::wait_for_file(&gate.join("fetching")).await;
    let fork =
        db::repositories::records::get_repository(database.pool(), "snapshot_forker", "snapshot")
            .await
            .unwrap()
            .unwrap();
    assert!(
        db::repositories::access::revoke_repo_access(database.pool(), forker, source.id)
            .await
            .unwrap()
    );
    source_store
        .save_file_as(
            database.pool(),
            source.id,
            owner,
            &write("secret.txt", &initial.sha),
            b"new private content",
            "Owner",
            "owner@example.invalid",
        )
        .await
        .unwrap();
    std::fs::write(gate.join("fetch"), b"").unwrap();
    fixture::wait_for_file(&gate.join("copied")).await;
    assert!(!pending.is_finished());
    let raw = client
        .exchange(
            "GET",
            "/api/repos/snapshot_forker/snapshot/raw/main/secret.txt",
            "",
            Vec::new(),
        )
        .await;
    // Check the response while fetch has finished but its caller cannot recheck access yet.
    std::fs::write(gate.join("finish"), b"").unwrap();
    assert_ne!(
        raw.status,
        StatusCode::OK,
        "unpublished fork served {}",
        raw.text()
    );
    assert!(!raw.text().contains("new private content"));
    assert_eq!(pending.await.unwrap().0, StatusCode::FORBIDDEN);
    assert!(
        db::repositories::records::get_repository_by_id(database.pool(), fork.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !repo_store(&fork, nodes.last().map(|node| &node.context))
            .exists()
            .await
    );
    let disk = if remote {
        storage_root.path()
    } else {
        root.path()
    };
    assert!(!std::fs::read_dir(disk).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".fork-")));

    db::repositories::access::grant_repo_access(
        database.pool(),
        forker,
        source.id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    client
        .expect(
            "POST",
            "/api/repos/snapshot_owner/snapshot/forks",
            Value::Null,
            StatusCode::CREATED,
        )
        .await;
    let raw = client
        .exchange(
            "GET",
            "/api/repos/snapshot_forker/snapshot/raw/main/secret.txt",
            "",
            Vec::new(),
        )
        .await;
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(raw.body, b"new private content");
    for node in nodes {
        node.shutdown().await;
    }
    database.finish().await;
}

#[tokio::test]
async fn sqlite_fork_contents_stay_unpublished_until_authorized() {
    fork_publication_contract(
        "sqlite_fork_contents_stay_unpublished_until_authorized",
        false,
        false,
    )
    .await;
}
#[tokio::test]
async fn postgres_fork_contents_stay_unpublished_until_authorized() {
    fork_publication_contract(
        "postgres_fork_contents_stay_unpublished_until_authorized",
        true,
        false,
    )
    .await;
}
#[tokio::test]
async fn remote_fork_contents_stay_unpublished_until_authorized() {
    fork_publication_contract(
        "remote_fork_contents_stay_unpublished_until_authorized",
        true,
        true,
    )
    .await;
}

#[tokio::test]
async fn sqlite_creation_authority() {
    creation_contract(Database::new(false).await).await;
}
#[tokio::test]
async fn postgres_creation_authority() {
    creation_contract(Database::new(true).await).await;
}
#[tokio::test]
async fn sqlite_personal_forks() {
    personal_fork_contract(Database::new(false).await).await;
}
#[tokio::test]
async fn postgres_personal_forks() {
    personal_fork_contract(Database::new(true).await).await;
}
#[tokio::test]
async fn sqlite_provision_creates_real_bare_repository() {
    provision_contract(Database::new(false).await).await;
}
#[tokio::test]
async fn postgres_provision_creates_real_bare_repository() {
    provision_contract(Database::new(true).await).await;
}
#[tokio::test]
async fn sqlite_initialization_cleanup_reports_what_remains() {
    initialization_cleanup_contract(Database::new(false).await).await;
}
#[tokio::test]
async fn postgres_initialization_cleanup_reports_what_remains() {
    initialization_cleanup_contract(Database::new(true).await).await;
}
