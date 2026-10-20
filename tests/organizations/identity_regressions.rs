//! Organization repository discovery, paging, deletion scope and invitation consumption.
use crate::support::{database::Database, fake_webui_bundle, pull_requests::Client};
use axum::http::StatusCode;
use codeyard::{
    backend::{
        api::create_router,
        app::AppState,
        ci::CiConfig,
        db::{
            self,
            models::{OrgRole, Repository},
            organizations::*,
            repositories::creation::*,
        },
        storage::ObjectStore,
    },
    webui::{app::Page, pages::discovery},
};
use std::sync::Arc;

fn applied<T: std::fmt::Debug>(value: OrganizationOutcome<T>) -> T {
    match value {
        OrganizationOutcome::Applied(value) => value,
        other => panic!("{other:?}"),
    }
}
async fn org(f: &Database, owner: i64, name: &str) -> i64 {
    applied(
        create_organization(
            f.pool(),
            owner,
            &NewOrganization::new(name, OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    )
    .id
}
async fn repository(f: &Database, owner: i64, org: i64, name: &str) -> Repository {
    insert_repository(
        f.pool(),
        NewRepository {
            owner: RepositoryCreationOwner {
                actor_id: owner,
                target: RepositoryTarget::Organization(org),
            },
            name,
            description: Some("private description"),
            is_private: true,
            disk_path: &uuid::Uuid::new_v4().to_string(),
            storage_node_id: None,
            source_id: None,
        },
    )
    .await
    .unwrap()
    .unwrap()
}
async fn discovery_and_deletion(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("org_owner").await;
    let member = f.actor("team_reader").await;
    let first_org = org(&f, owner, "first_org").await;
    let second_org = org(&f, owner, "second_org").await;
    applied(
        add_organization_member(f.pool(), owner, first_org, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    let mut repositories = Vec::new();
    for number in 0..31 {
        repositories.push(repository(&f, owner, first_org, &format!("repo-{number:02}")).await);
    }
    let team = applied(
        create_organization_team(
            f.pool(),
            owner,
            first_org,
            &NewTeam::new(
                "readers",
                TeamProfile::new("Readers", None, TeamPrivacy::Visible).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        add_organization_team_member(
            f.pool(),
            owner,
            first_org,
            team.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        grant_organization_team_repository(
            f.pool(),
            owner,
            first_org,
            team.id,
            repositories[0].id,
            TeamRepositoryPermission::new(1).unwrap(),
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let owner_client = Client::login(&router, f.pool(), owner).await;
    let member_client = Client::login(&router, f.pool(), member).await;
    for (client, expected) in [(&owner_client, 31), (&member_client, 1)] {
        let (status, model) = client.model("/").await;
        assert_eq!(status, StatusCode::OK);
        let Page::Discovery(discovery::Page::Home(home)) = model else {
            panic!("home")
        };
        assert_eq!(home.shared_count, expected);
        assert_eq!(home.shared.len(), expected.min(30) as usize);
        let (status, model) = client.model("/issues/new").await;
        assert_eq!(status, StatusCode::OK);
        let Page::IssueRepositories(choices) = model else {
            panic!("chooser")
        };
        assert_eq!(choices.repositories.len(), expected.min(30) as usize);
        assert_eq!(choices.has_older, expected > 30);
        assert!(choices
            .repositories
            .iter()
            .all(|repo| repo.owner == "first_org"));
        if expected > 30 {
            let (status, page) = client.model("/issues/new?page=2").await;
            assert_eq!(status, StatusCode::OK);
            let Page::IssueRepositories(second) = page else {
                panic!("chooser page 2")
            };
            assert_eq!(second.repositories.len(), 1);
            assert!(!second.has_older);
            assert!(!choices.repositories.contains(&second.repositories[0]));
        }
    }
    let (status, model) = owner_client.model("/?page=2").await;
    assert_eq!(status, StatusCode::OK);
    let Page::Discovery(discovery::Page::Home(home)) = model else {
        panic!("home")
    };
    assert_eq!(home.shared.len(), 1);
    assert!(!home.has_older);
    for tab in ["repositories", "people", "teams"] {
        let (status, model) = owner_client.model(&format!("/first_org?tab={tab}")).await;
        assert_eq!(status, StatusCode::OK);
        let Page::OrganizationProfile(profile) = model else {
            panic!("organization")
        };
        assert_eq!(profile.repository_count, 31);
        assert_eq!(
            profile.repositories.len(),
            if tab == "repositories" { 30 } else { 0 }
        );
        if tab == "repositories" {
            assert!(profile.more);
        }
    }
    let (status, model) = owner_client.model("/first_org?after=30").await;
    assert_eq!(status, StatusCode::OK);
    let Page::OrganizationProfile(profile) = model else {
        panic!("organization")
    };
    assert_eq!(profile.repositories.len(), 1);
    assert!(!profile.more);
    assert_eq!(
        list_organization_profile_repositories(
            f.pool(),
            first_org,
            Some(member),
            OrganizationPage::new(0, 30).unwrap()
        )
        .await
        .unwrap()
        .len(),
        1
    );
    for invalid in [
        "UPDATE org_memberships SET role='invalid' WHERE org_id=$1 AND user_id=$2",
        "DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2",
    ] {
        f.execute(invalid, &[first_org, member]).await;
        assert_eq!(
            db::repositories::access::get_user_repo_access(f.pool(), member, repositories[0].id)
                .await
                .unwrap(),
            db::models::AccessLevel::None
        );
        assert_eq!(
            count_organization_profile_repositories(f.pool(), first_org, Some(member))
                .await
                .unwrap(),
            0
        );
        assert!(list_organization_profile_repositories(
            f.pool(),
            first_org,
            Some(member),
            OrganizationPage::new(0, 30).unwrap()
        )
        .await
        .unwrap()
        .is_empty());
    }
    let initial = applied(
        list_organization_deletion_repositories(f.pool(), owner, first_org)
            .await
            .unwrap(),
    );
    let late = repository(&f, owner, first_org, "late-repository").await;
    db::repositories::records::transfer_repository(
        f.pool(),
        owner,
        repositories[0].id,
        RepositoryTarget::Organization(second_org),
    )
    .await
    .unwrap();
    assert!(matches!(
        delete_organization_repository(f.pool(), owner, first_org, repositories[0].id)
            .await
            .unwrap(),
        OrganizationOutcome::Conflict
    ));
    db::repositories::records::set_repository_archived(f.pool(), owner, repositories[1].id, true)
        .await
        .unwrap();
    for id in initial.into_iter().filter(|id| *id != repositories[0].id) {
        applied(
            delete_organization_repository(f.pool(), owner, first_org, id)
                .await
                .unwrap(),
        );
    }
    assert!(matches!(
        delete_organization(f.pool(), owner, first_org)
            .await
            .unwrap(),
        OrganizationOutcome::Conflict
    ));
    assert!(
        db::repositories::records::get_repository_by_id(f.pool(), late.id)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        db::repositories::records::get_repository_by_id(f.pool(), repositories[0].id)
            .await
            .unwrap()
            .unwrap()
            .owner_org_id,
        Some(second_org)
    );
    f.finish().await;
}
#[tokio::test]
async fn sqlite_organization_discovery_paging_and_scoped_deletion() {
    discovery_and_deletion(false).await;
}
#[tokio::test]
async fn postgres_organization_discovery_paging_and_scoped_deletion() {
    discovery_and_deletion(true).await;
}

async fn release_upload_cleanup(pg: bool) {
    let mut f = Database::new(pg).await;
    let owner = f.actor("release_owner").await;
    let org = org(&f, owner, "release_org").await;
    let repo = repository(&f, owner, org, "release_repo").await;
    let release =
        db::releases::create_release(f.pool(), owner, repo.id, "v1", "main", &Default::default())
            .await
            .unwrap();
    let id = uuid::Uuid::new_v4();
    let key = format!("uploads/{id}");
    let insert = "INSERT INTO files(id,user_id,filename,original_name,content_type,file_size,sha256_hex,object_key,reason,visibility,is_active,confirmed_at,upload_expires_at) VALUES($1,$2,'asset.bin','asset.bin','application/octet-stream',12,$3,$4,'release_asset','owner',FALSE,'2026-01-01T00:00:00.000000Z','2099-01-01T00:00:00.000000Z')";
    match f.pool() {
        db::DbPool::Postgres(raw) => {
            sqlx::query(insert)
                .bind(id)
                .bind(owner)
                .bind("aa".repeat(32))
                .bind(&key)
                .execute(raw)
                .await
                .unwrap();
        }
        db::DbPool::Sqlite(raw) => {
            sqlx::query(insert)
                .bind(id)
                .bind(owner)
                .bind("aa".repeat(32))
                .bind(&key)
                .execute(raw)
                .await
                .unwrap();
        }
    }
    db::releases::add_release_asset(
        f.pool(),
        owner,
        repo.id,
        release.id,
        &db::releases::NewReleaseAsset {
            file_id: id,
            name: "asset.bin",
            label: None,
            content_type: "application/octet-stream",
            size: 12,
        },
    )
    .await
    .unwrap();
    applied(
        delete_organization_repository(f.pool(), owner, org, repo.id)
            .await
            .unwrap(),
    );
    let check = "SELECT (SELECT COUNT(*) FROM files WHERE id=$1), (SELECT COUNT(*) FROM pending_object_deletions WHERE object_key=$2)";
    let counts: (i64, i64) = match f.pool() {
        db::DbPool::Postgres(raw) => sqlx::query_as(check)
            .bind(id)
            .bind(&key)
            .fetch_one(raw)
            .await
            .unwrap(),
        db::DbPool::Sqlite(raw) => sqlx::query_as(check)
            .bind(id)
            .bind(&key)
            .fetch_one(raw)
            .await
            .unwrap(),
    };
    assert_eq!(counts, (0, 1));
    f.finish().await;
}
#[tokio::test]
async fn sqlite_organization_deletion_queues_release_upload_cleanup() {
    release_upload_cleanup(false).await;
}
#[tokio::test]
async fn postgres_organization_deletion_queues_release_upload_cleanup() {
    release_upload_cleanup(true).await;
}

#[tokio::test]
async fn postgres_accept_and_decline_have_one_winner() {
    let mut f = Database::new(true).await;
    let owner = f.actor("invite_owner").await;
    let member = f.actor("invite_member").await;
    let organization = org(&f, owner, "invite_org").await;
    applied(
        invite_organization_member(f.pool(), owner, organization, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    let db::DbPool::Postgres(raw) = f.pool() else {
        unreachable!()
    };
    let mut gate = raw.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(member)
        .execute(&mut *gate)
        .await
        .unwrap();
    let pool = f.pool().clone();
    let acceptance = tokio::spawn(async move {
        accept_organization_invitation(&pool, member, organization)
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(5),async {
        loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid)))").bind(blocker).fetch_one(raw).await.unwrap();
            if waiting {break;}
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let pool = f.pool().clone();
    let decline = tokio::spawn(async move {
        decline_organization_invitation(&pool, member, organization)
            .await
            .unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!decline.is_finished());
    gate.commit().await.unwrap();
    applied(acceptance.await.unwrap());
    assert_eq!(decline.await.unwrap(), OrganizationOutcome::Missing);
    f.finish().await;
}
