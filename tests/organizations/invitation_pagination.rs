use crate::support::{database::Database, fake_webui_bundle, pull_requests::Client};
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    db::{self, organizations::*},
    storage::ObjectStore,
};
use codeyard::webui::app::Page;
use std::sync::Arc;

fn applied<T: std::fmt::Debug>(outcome: OrganizationOutcome<T>) -> T {
    match outcome {
        OrganizationOutcome::Applied(value) => value,
        other => panic!("{other:?}"),
    }
}
async fn organization(f: &Database, owner: i64, name: &str) -> i64 {
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

async fn pages(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("invite_owner").await;
    let recipient = f.actor("invite_recipient").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[owner])
        .await;
    let mut organizations = Vec::new();
    for index in 0..101 {
        let org = organization(&f, owner, &format!("invite_org_{index:03}")).await;
        applied(
            invite_organization_member(
                f.pool(),
                owner,
                org,
                recipient,
                db::models::OrgRole::Member,
            )
            .await
            .unwrap(),
        );
        organizations.push(org);
    }
    let first_org = organizations[0];
    for index in 0..100 {
        let user = f.actor(&format!("invited_{index:03}")).await;
        applied(
            invite_organization_member(
                f.pool(),
                owner,
                first_org,
                user,
                db::models::OrgRole::Member,
            )
            .await
            .unwrap(),
        );
    }
    let first = list_received_organization_invitations(
        f.pool(),
        recipient,
        OrganizationPage::new(0, 100).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(first.len(), 100);
    let second = list_received_organization_invitations(
        f.pool(),
        recipient,
        OrganizationPage::new(first.last().unwrap().id, 100).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].org_id, organizations[100]);
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        Default::default(),
        None,
    ));
    let owner_client = Client::login(&router, f.pool(), owner).await;
    let recipient_client = Client::login(&router, f.pool(), recipient).await;
    let (status, page) = recipient_client.model("/settings/organizations").await;
    assert_eq!(status, StatusCode::OK);
    let Page::Organizations(page) = page else {
        panic!("organizations")
    };
    assert_eq!(page.invitations.len(), 100);
    let next = page.invitations_next.unwrap();
    let (_, page) = recipient_client.model(&next).await;
    let Page::Organizations(page) = page else {
        panic!("organizations next")
    };
    assert_eq!(page.invitations.len(), 1);
    assert!(page.invitations_next.is_none());
    let (_, page) = owner_client
        .model(&format!("/settings/organizations/{first_org}?tab=members"))
        .await;
    let Page::Organizations(page) = page else {
        panic!("members")
    };
    assert_eq!(page.pending_invitations.len(), 100);
    assert!(page.pending_invitations[0].id > 0);
    let next = page.pending_invitations_next.unwrap();
    let (_, page) = owner_client.model(&next).await;
    let Page::Organizations(page) = page else {
        panic!("members next")
    };
    assert_eq!(page.pending_invitations.len(), 1);
    assert!(page.pending_invitations_next.is_none());
    let reply = owner_client
        .form_reply(
            &format!("/settings/organizations/{first_org}/webhooks/webhook-save?tab=webhooks"),
            &[
                ("payload_url", "https://example.test/my-hook"),
                ("content_type", "application/json"),
                ("active", "yes"),
                ("ssl_verify", "yes"),
                ("secret", "do-not-echo-this"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.text().contains("https://example.test/my-hook"));
    assert!(reply.text().contains("name=\"payload_url\""));
    assert!(!reply.text().contains("do-not-echo-this"));
    for org in &organizations {
        f.execute("DELETE FROM org_memberships WHERE org_id=$1", &[*org])
            .await;
    }
    let first = applied(
        list_ownerless_organizations(f.pool(), owner, OrganizationPage::new(0, 100).unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(first.len(), 100);
    assert!(first
        .iter()
        .all(|org| org.description.is_none() && org.display_name.is_none()));
    let next = applied(
        list_ownerless_organizations(
            f.pool(),
            owner,
            OrganizationPage::new(first.last().unwrap().id, 100).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(next.len(), 1);
    let (_, page) = owner_client.model("/settings/organizations").await;
    let Page::Organizations(page) = page else {
        panic!("recovery")
    };
    assert_eq!(page.ownerless_organizations.len(), 100);
    let (_, page) = owner_client
        .model(page.ownerless_next.as_deref().unwrap())
        .await;
    let Page::Organizations(page) = page else {
        panic!("recovery next")
    };
    assert_eq!(page.ownerless_organizations.len(), 1);
    f.finish().await;
}

async fn deletion_removes_release_uploads(pg: bool) {
    let mut f = Database::new(pg).await;
    let owner = f.actor("release_org_owner").await;
    let org = organization(&f, owner, "release_org").await;
    let repository = db::repositories::creation::insert_repository(
        f.pool(),
        db::repositories::creation::NewRepository {
            owner: db::repositories::creation::RepositoryCreationOwner {
                actor_id: owner,
                target: db::repositories::creation::RepositoryTarget::Organization(org),
            },
            name: "assets",
            description: None,
            is_private: true,
            disk_path: "/unused/assets.git",
            storage_node_id: None,
            source_id: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let release = db::releases::create_release(
        f.pool(),
        owner,
        repository.id,
        "v1",
        "main",
        &Default::default(),
    )
    .await
    .unwrap();
    let file = db::files::reserve_upload(
        f.pool(),
        db::files::ReserveUpload {
            user_id: owner,
            filename: "tool.bin",
            content_type: "application/octet-stream",
            file_size: 1,
            reason: db::models::FileReason::ReleaseAsset,
            visibility: db::models::FileVisibility::Owner,
            sha256_hex: &"a".repeat(64),
        },
    )
    .await
    .unwrap();
    db::files::verify_pending_upload(f.pool(), file.id, owner)
        .await
        .unwrap();
    db::releases::add_release_asset(
        f.pool(),
        owner,
        repository.id,
        release.id,
        &db::releases::NewReleaseAsset {
            file_id: file.id,
            name: "tool.bin",
            label: None,
            content_type: "application/octet-stream",
            size: 1,
        },
    )
    .await
    .unwrap();
    assert!(
        db::files::get_file_by_id(f.pool(), file.id)
            .await
            .unwrap()
            .unwrap()
            .is_active
    );
    if let db::DbPool::Postgres(raw) = f.pool() {
        let mut reader = raw.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *reader)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtext(current_schema()),37002)")
            .execute(&mut *reader)
            .await
            .unwrap();
        let pool = f.pool().clone();
        let id = repository.id;
        let deleting =
            tokio::spawn(
                async move { delete_organization_repository(&pool, owner, org, id).await },
            );
        tokio::time::timeout(std::time::Duration::from_secs(5),async {
            loop {
                let waiting:bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))").bind(holder).fetch_one(raw).await.unwrap();
                if waiting { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("deletion excludes notification readers before taking organization rows");
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE NOWAIT")
            .bind(org)
            .fetch_one(&mut *reader)
            .await
            .unwrap();
        reader.rollback().await.unwrap();
        applied(deleting.await.unwrap().unwrap());
    } else {
        applied(
            delete_organization_repository(f.pool(), owner, org, repository.id)
                .await
                .unwrap(),
        );
    }
    assert!(db::files::get_file_by_id(f.pool(), file.id)
        .await
        .unwrap()
        .is_none());
    f.execute(
        "UPDATE pending_object_deletions SET delete_after='2000-01-01T00:00:00Z'",
        &[],
    )
    .await;
    assert!(db::files::due_object_deletions(f.pool(), 100)
        .await
        .unwrap()
        .contains(&file.object_key));
    f.finish().await;
}

#[tokio::test]
async fn sqlite_invitation_and_recovery_pages_and_webhook_draft() {
    pages(false).await;
}
#[tokio::test]
async fn postgres_invitation_and_recovery_pages_and_webhook_draft() {
    pages(true).await;
}
#[tokio::test]
async fn sqlite_organization_deletion_removes_release_uploads() {
    deletion_removes_release_uploads(false).await;
}
#[tokio::test]
async fn postgres_organization_deletion_removes_release_uploads() {
    deletion_removes_release_uploads(true).await;
}
