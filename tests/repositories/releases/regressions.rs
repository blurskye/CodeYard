use super::*;

async fn release_pagination_rejects_overflow(pg: bool) {
    let mut world = setup(pg).await;
    // The JSON list pages like every other JSON list: an out-of-range page is a bad request.
    for (path, status) in [
        (
            "/api/repos/alice/proj/releases?page=9223372036854775807",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/alice/proj/releases?page=9223372036854775807",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let response = world
            .alice
            .exchange("GET", path, "text/html", Vec::new())
            .await;
        assert_eq!(response.status, status, "{}", response.text());
    }
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_release_pagination_rejects_overflow() {
    release_pagination_rejects_overflow(false).await;
}

#[tokio::test]
async fn postgres_release_pagination_rejects_overflow() {
    release_pagination_rejects_overflow(true).await;
}

async fn concurrent_release_updates_report_one_publication(pg: bool) {
    use codeyard_db_repos::releases::ReleaseChanges;
    use codeyard_db_repos::releases::ReleaseFields;
    let mut world = setup(pg).await;
    let pool = world.database.pool();
    let repo = codeyard_db_repos::repositories::records::get_repository(pool, "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let actor = repo.owner_user_id.unwrap();
    let draft = codeyard_db_repos::releases::create_release(
        pool,
        actor,
        repo.id,
        "draft",
        &world.main,
        &ReleaseFields {
            is_draft: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let publish = ReleaseChanges {
        is_draft: Some(false),
        ..Default::default()
    };
    let notes = ReleaseChanges {
        body: Some(Some("Edited notes".into())),
        ..Default::default()
    };
    let (published, edited) = tokio::join!(
        codeyard_db_repos::releases::update_release(pool, actor, repo.id, draft.id, &publish),
        codeyard_db_repos::releases::update_release(pool, actor, repo.id, draft.id, &notes),
    );
    assert!(published.unwrap().1);
    assert!(!edited.unwrap().1);
    let saved = codeyard_db_repos::releases::get_release(pool, repo.id, draft.id)
        .await
        .unwrap()
        .unwrap()
        .release;
    assert!(!saved.is_draft);
    assert_eq!(saved.body.as_deref(), Some("Edited notes"));
    assert!(
        !codeyard_db_repos::releases::update_release(pool, actor, repo.id, draft.id, &publish)
            .await
            .unwrap()
            .1
    );
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_concurrent_release_updates_report_one_publication() {
    concurrent_release_updates_report_one_publication(false).await;
}

#[tokio::test]
async fn postgres_concurrent_release_updates_report_one_publication() {
    concurrent_release_updates_report_one_publication(true).await;
}

/// An edit prepared for a published release skips the tag checks that publishing runs, so it
/// must not publish a release that became a draft before the edit was written.
async fn an_edit_prepared_for_a_published_release_does_not_publish_a_draft(pg: bool) {
    use codeyard_db_repos::releases::ReleaseChanges;
    use codeyard_db_repos::releases::ReleaseFields;
    use codeyard_db_repos::releases::ReleaseRefusal;
    let mut world = setup(pg).await;
    let pool = world.database.pool();
    let repo = codeyard_db_repos::repositories::records::get_repository(pool, "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let actor = repo.owner_user_id.unwrap();
    let release = codeyard_db_repos::releases::create_release(
        pool,
        actor,
        repo.id,
        "published",
        &world.main,
        &ReleaseFields::default(),
    )
    .await
    .unwrap();
    let unpublish = ReleaseChanges {
        is_draft: Some(true),
        expected_draft: Some(false),
        ..Default::default()
    };
    let stale = ReleaseChanges {
        body: Some(Some("Edited while published".into())),
        is_draft: Some(false),
        expected_draft: Some(false),
        ..Default::default()
    };
    let (unpublished, edited) = tokio::join!(
        codeyard_db_repos::releases::update_release(pool, actor, repo.id, release.id, &unpublish),
        codeyard_db_repos::releases::update_release(pool, actor, repo.id, release.id, &stale),
    );
    assert!(!unpublished.unwrap().1);
    if let Ok((_, published)) = edited {
        assert!(!published);
    }
    async fn draft(pool: &codeyard_db_core::DbPool, repo: i64, release: i64) -> bool {
        codeyard_db_repos::releases::get_release(pool, repo, release)
            .await
            .unwrap()
            .unwrap()
            .release
            .is_draft
    }
    assert!(draft(pool, repo.id, release.id).await);
    let refused =
        codeyard_db_repos::releases::update_release(pool, actor, repo.id, release.id, &stale)
            .await
            .unwrap_err();
    assert!(matches!(
        refused.downcast_ref::<ReleaseRefusal>(),
        Some(ReleaseRefusal::Conflict)
    ));
    assert!(draft(pool, repo.id, release.id).await);
    let publish = ReleaseChanges {
        is_draft: Some(false),
        expected_draft: Some(true),
        ..Default::default()
    };
    assert!(
        codeyard_db_repos::releases::update_release(pool, actor, repo.id, release.id, &publish)
            .await
            .unwrap()
            .1
    );
    world.database.finish().await;
}

#[tokio::test]
async fn sqlite_an_edit_prepared_for_a_published_release_does_not_publish_a_draft() {
    an_edit_prepared_for_a_published_release_does_not_publish_a_draft(false).await;
}

#[tokio::test]
async fn postgres_an_edit_prepared_for_a_published_release_does_not_publish_a_draft() {
    an_edit_prepared_for_a_published_release_does_not_publish_a_draft(true).await;
}

async fn release_asset_file_lookup_has_an_index(pg: bool) {
    let mut database = Database::new(pg).await;
    match database.pool() {
        codeyard_db_core::DbPool::Sqlite(pool) => {
            let plan: Vec<(i64, i64, i64, String)> =
                sqlx::query_as("EXPLAIN QUERY PLAN SELECT 1 FROM release_assets WHERE file_id=?1")
                    .bind(uuid::Uuid::nil())
                    .fetch_all(pool)
                    .await
                    .unwrap();
            assert!(
                plan.iter()
                    .any(|row| row.3.contains("idx_release_assets_file_id")),
                "{plan:?}"
            );
        }
        codeyard_db_core::DbPool::Postgres(pool) => {
            let definition: String = sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE schemaname=current_schema() AND indexname='idx_release_assets_file_id'")
                    .fetch_one(pool).await.unwrap();
            assert!(definition.contains("(file_id)"), "{definition}");
        }
    }
    database.finish().await;
}

#[tokio::test]
async fn sqlite_release_asset_file_lookup_has_an_index() {
    release_asset_file_lookup_has_an_index(false).await;
}

#[tokio::test]
async fn postgres_release_asset_file_lookup_has_an_index() {
    release_asset_file_lookup_has_an_index(true).await;
}
