use super::*;

#[tokio::test]
async fn release_pagination_rejects_overflow_on_both_engines() {
    for pg in [false, true] {
        let mut world = setup(pg).await;
        for path in [
            "/api/repos/alice/proj/releases?page=9223372036854775807",
            "/alice/proj/releases?page=9223372036854775807",
        ] {
            let response = world
                .alice
                .exchange("GET", path, "text/html", Vec::new())
                .await;
            assert_eq!(
                response.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{}",
                response.text()
            );
        }
        world.database.finish().await;
    }
}

#[tokio::test]
async fn concurrent_release_updates_report_one_publication_on_both_engines() {
    use db::releases::{ReleaseChanges, ReleaseFields};
    for pg in [false, true] {
        let mut world = setup(pg).await;
        let pool = world.database.pool();
        let repo = db::repositories::records::get_repository(pool, "alice", "proj")
            .await
            .unwrap()
            .unwrap();
        let actor = repo.owner_user_id.unwrap();
        let draft = db::releases::create_release(
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
            db::releases::update_release(pool, actor, repo.id, draft.id, &publish),
            db::releases::update_release(pool, actor, repo.id, draft.id, &notes),
        );
        assert!(published.unwrap().1);
        assert!(!edited.unwrap().1);
        let saved = db::releases::get_release(pool, repo.id, draft.id)
            .await
            .unwrap()
            .unwrap()
            .release;
        assert!(!saved.is_draft);
        assert_eq!(saved.body.as_deref(), Some("Edited notes"));
        assert!(
            !db::releases::update_release(pool, actor, repo.id, draft.id, &publish)
                .await
                .unwrap()
                .1
        );
        world.database.finish().await;
    }
}

#[tokio::test]
async fn release_asset_file_lookup_has_an_index_on_both_engines() {
    for pg in [false, true] {
        let mut database = Database::new(pg).await;
        match database.pool() {
            db::DbPool::Sqlite(pool) => {
                let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(
                    "EXPLAIN QUERY PLAN SELECT 1 FROM release_assets WHERE file_id=?1",
                )
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
            db::DbPool::Postgres(pool) => {
                let definition: String = sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE schemaname=current_schema() AND indexname='idx_release_assets_file_id'")
                    .fetch_one(pool).await.unwrap();
                assert!(definition.contains("(file_id)"), "{definition}");
            }
        }
        database.finish().await;
    }
}
