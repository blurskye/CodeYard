//! The Checks tab of a pull request: the workflow runs on its head commit.
use crate::backend::api::ci::responses::map_run;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::discussion::*;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::webui::pages::repos::issues::{self as view, Kind};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::sync::Arc;

pub(crate) async fn native_checks(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    render_checks(&ctx, &repo, value).await
}

async fn render_checks(
    ctx: &PageContext,
    repo: &RepositoryData,
    value: view::Item,
) -> ResultPage<Response> {
    let pull =
        db::pull_requests::records::get_pull_request(&ctx.state.pool, repo.record.id, value.number)
            .await
            .map_err(|_| unavailable(ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))?;
    let head = value
        .comparison
        .as_ref()
        .map_or(pull.head_sha.as_str(), |compare| compare.head_sha.as_str());
    let (target, _, _) =
        crate::backend::repositories::pull_requests::stacks::merge_target(&ctx.state.pool, &pull)
            .await
            .map_err(|_| unavailable(ctx))?;
    let runs =
        db::ci::latest_ci_runs_for_commit(&ctx.state.pool, repo.record.id, head, pull.id, &target)
            .await
            .map_err(|_| unavailable(ctx))?;
    let statuses =
        db::repositories::protection::list_commit_statuses(&ctx.state.pool, repo.record.id, head)
            .await
            .map_err(|_| unavailable(ctx))?
            .into_iter()
            .filter(|status| {
                !db::repositories::protection::server_owned_status_context(&status.context)
                    && !runs
                        .iter()
                        .any(|run| run.commit_status_context() == status.context)
            })
            .map(|status| crate::webui::pages::repos::pulls::CheckStatus {
                context: status.context,
                state: status.state,
                description: status.description,
                target_url: status.target_url,
            })
            .collect();
    let runs = runs
        .into_iter()
        .map(|run| map_run(run, &repo.record))
        .collect();
    Ok(ctx.page_model(
        &format!("{} #{}", value.title, value.number),
        page(view::Page::PullChecks {
            repository: repo.view.clone(),
            item: value,
            runs,
            statuses,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn checks_keep_the_item_snapshot_after_a_push() {
        let pool = db::init_db("sqlite::memory:").await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let db::DbPool::Sqlite(sqlite) = &pool else {
            unreachable!()
        };
        sqlx::query("INSERT INTO node_configuration(id, setup_completed) VALUES(1, TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE").execute(sqlite).await.unwrap();
        let owner = db::users::accounts::create_user(&pool, "alice", None)
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let record = crate::backend::repositories::lifecycle::provision(
            (&pool, None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(owner.id),
            "proj",
            None,
            false,
            None,
        )
        .await
        .unwrap();
        let first = "1".repeat(40);
        let second = "2".repeat(40);
        let pull = db::pull_requests::records::create_pull_request(
            &pool,
            db::pull_requests::records::NewPullRequest {
                repo_id: record.id,
                head_repo_id: Some(record.id),
                author_id: owner.id,
                title: "Snapshot",
                body: "",
                head_branch: "topic",
                head_sha: &first,
                base_branch: "main",
                base_sha: &first,
                is_draft: false,
            },
        )
        .await
        .unwrap();
        for (head, context) in [(&first, "old-check"), (&second, "new-check")] {
            sqlx::query("INSERT INTO commit_statuses(repo_id, sha, context, state, creator_id) VALUES(?1, ?2, ?3, 'success', ?4)")
                .bind(record.id).bind(head).bind(context).bind(owner.id).execute(sqlite).await.unwrap();
        }
        let state = Arc::new(AppState::new(
            pool.clone(),
            root.path().into(),
            Arc::new(
                crate::backend::storage::ObjectStore::open(root.path(), None)
                    .await
                    .unwrap(),
            ),
            None,
            crate::backend::ci::CiConfig::default(),
            None,
        ));
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ACCEPT,
            crate::webui::bootstrap::MODEL_CONTENT_TYPE.parse().unwrap(),
        );
        let ctx = PageContext::load(
            state,
            headers,
            &"/alice/proj/pulls/1/checks".parse().unwrap(),
        )
        .await
        .unwrap();
        let repo = repository(&ctx, "alice", "proj").await.unwrap();
        let value = pull_item(
            &ctx,
            &repo,
            pull.clone(),
            "alice".into(),
            HeadRepository::Deleted,
        )
        .unwrap();
        sqlx::query("UPDATE pull_requests SET head_sha=?1 WHERE id=?2")
            .bind(&second)
            .bind(pull.id)
            .execute(sqlite)
            .await
            .unwrap();
        let response = render_checks(&ctx, &repo, value).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let envelope: crate::webui::app::PageEnvelope = serde_json::from_slice(&body).unwrap();
        let crate::webui::app::Page::Collaboration(model) = envelope.page else {
            panic!("not collaboration")
        };
        let view::Page::PullChecks { item, statuses, .. } = *model else {
            panic!("not checks")
        };
        assert_eq!(item.comparison.unwrap().head_sha, first);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].context, "old-check");
        pool.close().await;
    }
}
