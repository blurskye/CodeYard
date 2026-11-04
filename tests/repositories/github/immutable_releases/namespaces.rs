use super::*;
use db::organizations::{membership, profiles, OrganizationOutcome};

async fn organization(f: &Database, actor: i64, name: &str) -> i64 {
    let input = profiles::NewOrganization::new(
        name,
        profiles::OrganizationProfile::new(None, None).unwrap(),
    )
    .unwrap();
    let OrganizationOutcome::Applied(org) = profiles::create_organization(f.pool(), actor, &input)
        .await
        .unwrap()
    else {
        panic!("organization")
    };
    org.id
}
async fn refused(f: &Database, sql: &str, id: i64) -> String {
    match f {
        Database::Postgres(f) => sqlx::query(sql)
            .bind(id)
            .execute(&f.raw)
            .await
            .unwrap_err()
            .to_string(),
        Database::Sqlite(f) => sqlx::query(sql)
            .bind(id)
            .execute(&f.raw)
            .await
            .unwrap_err()
            .to_string(),
    }
}
async fn history(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let other = credential(&router, &f, bob, "write").await;
    let (_, original) = checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"history","auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    let old = original["id"].as_i64().unwrap();
    checked(
        &router,
        "repos/enable-immutable-releases",
        "PUT",
        "/api/v3/repos/alice/history/immutable-releases",
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &router,
        "repos/create-release",
        "POST",
        "/api/v3/repos/alice/history/releases",
        Some(&writer),
        json!({"tag_name":"retired","target_commitish":"main"}),
        StatusCode::CREATED,
    )
    .await;
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/alice/history",
        Some(&writer),
        json!({"name":"renamed"}),
        StatusCode::OK,
    )
    .await;
    f.execute(
        "UPDATE users SET username='alice_saved' WHERE id=$1",
        &[alice],
    )
    .await;
    let stale = db::repositories::records::patch_repository_and_default(
        f.pool(),
        alice,
        old,
        &db::repositories::records::RepositoryChanges {
            name: Some("raced"),
            ..Default::default()
        },
        "main",
        Some("alice"),
    )
    .await
    .expect_err("owner changed after the Git namespace check");
    assert!(matches!(
        stale.downcast_ref::<db::repositories::records::RepositorySettingsRefusal>(),
        Some(db::repositories::records::RepositorySettingsRefusal::NameConflict)
    ));
    let org = organization(&f, alice, "release_org").await;
    checked(
        &router,
        "repos/transfer",
        "POST",
        "/api/v3/repos/alice_saved/renamed/transfer",
        Some(&writer),
        json!({"new_owner":"release_org"}),
        StatusCode::ACCEPTED,
    )
    .await;
    checked(
        &router,
        "repos/delete",
        "DELETE",
        "/api/v3/repos/release_org/renamed",
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(f.scalar("SELECT COUNT(*) FROM immutable_release_publications WHERE repo_id=$1 AND release_id IS NULL",&[old]).await,1);
    // Deletion, repository rename, owner rename and transfer all retain reservations.
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"history","auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    checked(
        &router,
        "repos/create-release",
        "POST",
        "/api/v3/repos/alice_saved/history/releases",
        Some(&writer),
        json!({"tag_name":"retired","target_commitish":"main"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &router,
        "repos/create-release",
        "POST",
        "/api/v3/repos/alice_saved/history/releases",
        Some(&writer),
        json!({"tag_name":"fresh","target_commitish":"main"}),
        StatusCode::CREATED,
    )
    .await;
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&other),
        json!({"name":"renamed","auto_init":true}),
        StatusCode::CREATED,
    )
    .await;
    checked(
        &router,
        "repos/create-release",
        "POST",
        "/api/v3/repos/bob/renamed/releases",
        Some(&other),
        json!({"tag_name":"retired","target_commitish":"main"}),
        StatusCode::CREATED,
    )
    .await;
    assert!(matches!(
        membership::add_organization_member(
            f.pool(),
            alice,
            org,
            bob,
            codeyard_db_core::models::OrgRole::Admin
        )
        .await
        .unwrap(),
        OrganizationOutcome::Applied(_)
    ));
    checked(
        &router,
        "repos/transfer",
        "POST",
        "/api/v3/repos/bob/renamed/transfer",
        Some(&other),
        json!({"new_owner":"release_org"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert!(
        refused(&f, "UPDATE users SET username='alice' WHERE id=$1", bob)
            .await
            .contains("reserved immutable")
    );
    // Moving the conflicting repository out of the way permits the owner rename.
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/bob/renamed",
        Some(&other),
        json!({"name":"unrelated"}),
        StatusCode::OK,
    )
    .await;
    // Owner renames must check every historical repository alias.
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/bob/unrelated",
        Some(&other),
        json!({"name":"history"}),
        StatusCode::OK,
    )
    .await;
    assert!(
        refused(&f, "UPDATE users SET username='alice' WHERE id=$1", bob)
            .await
            .contains("reserved immutable")
    );
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/bob/history",
        Some(&other),
        json!({"name":"unrelated"}),
        StatusCode::OK,
    )
    .await;
    f.execute("UPDATE users SET username='alice' WHERE id=$1", &[bob])
        .await;
    // A fresh repository may exist at a historical path, but cannot import reserved tags.
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/alice/unrelated",
        Some(&other),
        json!({"name":"history"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let bob_repo = f
        .scalar(
            "SELECT id FROM repositories WHERE owner_user_id=$1 AND name='unrelated'",
            &[bob],
        )
        .await;
    let legacy = db::repositories::records::patch_repository(
        f.pool(),
        bob,
        bob_repo,
        &db::repositories::records::RepositoryChanges {
            name: Some("history"),
            ..Default::default()
        },
    )
    .await
    .expect_err("metadata-only callers cannot bypass reserved namespaces");
    assert!(matches!(
        legacy.downcast_ref::<db::repositories::records::RepositorySettingsRefusal>(),
        Some(db::repositories::records::RepositorySettingsRefusal::NameConflict)
    ));
    let recipient = organization(&f, bob, "recipient_org").await;
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/alice/unrelated",
        Some(&other),
        json!({"name":"renamed"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    // Use a distinct current owner for the organization-rename backstop.
    checked(
        &router,
        "repos/transfer",
        "POST",
        "/api/v3/repos/alice/unrelated/transfer",
        Some(&other),
        json!({"new_owner":"recipient_org"}),
        StatusCode::ACCEPTED,
    )
    .await;
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/recipient_org/unrelated",
        Some(&other),
        json!({"name":"renamed"}),
        StatusCode::OK,
    )
    .await;
    f.execute(
        "UPDATE organizations SET name='release_org_saved' WHERE id=$1",
        &[org],
    )
    .await;
    assert!(refused(
        &f,
        "UPDATE organizations SET name='release_org' WHERE id=$1",
        recipient
    )
    .await
    .contains("reserved immutable"));
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/recipient_org/renamed",
        Some(&other),
        json!({"name":"unrelated"}),
        StatusCode::OK,
    )
    .await;
    f.execute(
        "UPDATE organizations SET name='release_org' WHERE id=$1",
        &[recipient],
    )
    .await;
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/release_org/unrelated",
        Some(&other),
        json!({"name":"renamed"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    // Forks cannot smuggle a reserved tag into a recreated historical namespace.
    checked(
        &router,
        "repos/delete",
        "DELETE",
        "/api/v3/repos/alice_saved/history",
        Some(&writer),
        Value::Null,
        StatusCode::NO_CONTENT,
    )
    .await;
    checked(
        &router,
        "repos/create-fork",
        "POST",
        "/api/v3/repos/release_org/unrelated/forks",
        Some(&writer),
        json!({"name":"history"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    checked(
        &router,
        "repos/get",
        "GET",
        "/api/v3/repos/alice_saved/history",
        Some(&writer),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    // Independent template roots omit source tags, so they remain usable there.
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/release_org/unrelated",
        Some(&other),
        json!({"is_template":true}),
        StatusCode::OK,
    )
    .await;
    checked(
        &router,
        "repos/create-using-template",
        "POST",
        "/api/v3/repos/release_org/unrelated/generate",
        Some(&writer),
        json!({"owner":"alice_saved","name":"history"}),
        StatusCode::CREATED,
    )
    .await;
    checked(
        &router,
        "repos/create-release",
        "POST",
        "/api/v3/repos/alice_saved/history/releases",
        Some(&writer),
        json!({"tag_name":"retired","target_commitish":"main"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    drop(router);
    let mut f = f;
    f.finish().await;
}
#[tokio::test]
async fn namespaces_survive_deletion_and_follow_renames_and_transfers() {
    history(false).await;
    history(true).await;
}
