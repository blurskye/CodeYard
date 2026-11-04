//! The Checks tab of a pull request: the workflow runs on its head commit.
use crate::discussion::item;
use crate::discussion::repository;
use crate::discussion::unavailable;
use crate::discussion::RepositoryData;
use crate::discussion::ResultPage;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::ci::responses::map_run;
use codeyard_api_http::shared::repos::discussion::page;
use codeyard_app::app::AppState;
use codeyard_webui::pages::repos::issues as view;
use codeyard_webui::pages::repos::issues::Kind;
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
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request(
        &ctx.state.pool,
        repo.record.id,
        value.number,
    )
    .await
    .map_err(|_| unavailable(ctx))?
    .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))?;
    let head = value
        .comparison
        .as_ref()
        .map_or(pull.head_sha.as_str(), |compare| compare.head_sha.as_str());
    let base = value
        .comparison
        .as_ref()
        .map_or(pull.base_sha.as_str(), |comparison| {
            comparison.base_sha.as_str()
        });
    let code_scanning = crate::pulls::security::checks(ctx, repo, &pull, head, base).await?;
    let (target, _, _) = codeyard_repos::stacks::merge_target(&ctx.state.pool, &pull)
        .await
        .map_err(|_| unavailable(ctx))?;
    let runs = codeyard_db_actions::ci::latest_ci_runs_for_commit(
        &ctx.state.pool,
        repo.record.id,
        head,
        pull.id,
        &target,
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    // Check runs an integration published through the API, latest per name, link to their pages.
    // They come a page at a time and without their summaries and text, which this tab never shows.
    const CHECKS_PER_PAGE: usize = 100;
    let max_page = crate::paging::MAX_PAGE;
    let check_page = crate::paging::number(ctx, "checks_page", 1, 1..=max_page)?;
    let offset = (check_page - 1) * CHECKS_PER_PAGE;
    let check_total = codeyard_db_actions::checks::summary(&ctx.state.pool, repo.record.id, head)
        .await
        .map_err(|_| unavailable(ctx))?
        .total;
    let more_checks = check_page < max_page
        && i64::try_from(offset + CHECKS_PER_PAGE).is_ok_and(|end| end < check_total);
    let check_runs = codeyard_db_actions::checks::rollup_runs(
        &ctx.state.pool,
        repo.record.id,
        head,
        offset as i64,
        CHECKS_PER_PAGE as i64,
    )
    .await
    .map_err(|_| unavailable(ctx))?
    .into_iter()
    .map(|run| codeyard_webui::pages::repos::pulls::CheckStatus {
        state: match (run.status.as_str(), run.conclusion.as_deref()) {
            ("completed", Some("success" | "neutral" | "skipped")) => "success",
            ("completed", _) => "failure",
            _ => "pending",
        }
        .to_owned(),
        description: run.data.title.clone().or(run.conclusion),
        // The view links only absolute HTTP(S) addresses.
        target_url: Some(format!(
            "{}/{}/{}/runs/{}",
            codeyard_model::http_config::link_origin(),
            repo.view.owner,
            repo.record.name,
            run.id
        )),
        context: run.name,
    });
    let statuses = codeyard_db_repos::repositories::protection::list_commit_statuses(
        &ctx.state.pool,
        repo.record.id,
        head,
    )
    .await
    .map_err(|_| unavailable(ctx))?
    .into_iter()
    .filter(|status| {
        !codeyard_db_repos::repositories::protection::server_owned_status_context(&status.context)
            && !runs
                .iter()
                .any(|run| run.commit_status_context() == status.context)
    })
    .map(|status| codeyard_webui::pages::repos::pulls::CheckStatus {
        context: status.context,
        state: status.state,
        description: status.description,
        target_url: status.target_url,
    })
    .chain(check_runs)
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
            code_scanning,
            check_page: u32::try_from(check_page).unwrap_or(1),
            more_checks,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discussion::pull_item;
    use crate::discussion::HeadRepository;

    #[tokio::test]
    async fn checks_keep_the_item_snapshot_after_a_push() {
        let pool = codeyard_db_core::init_db("sqlite::memory:").await.unwrap();
        codeyard_db_core::run_migrations(&pool).await.unwrap();
        let codeyard_db_core::DbPool::Sqlite(sqlite) = &pool else {
            unreachable!()
        };
        sqlx::query("INSERT INTO node_configuration(id, setup_completed) VALUES(1, TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE").execute(sqlite).await.unwrap();
        let owner = codeyard_db_identity::users::accounts::create_user(&pool, "alice", None)
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let record = codeyard_repos_core::repositories::lifecycle::provision(
            (&pool, None),
            root.path(),
            codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner.id),
            "proj",
            None,
            false,
            None,
        )
        .await
        .unwrap();
        let first = "1".repeat(40);
        let second = "2".repeat(40);
        let pull = codeyard_db_repos::pull_requests::records::create_pull_request(
            &pool,
            codeyard_db_repos::pull_requests::records::NewPullRequest {
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
                maintainer_can_modify: false,
                issue: None,
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
                codeyard_storage::ObjectStore::open(root.path(), None)
                    .await
                    .unwrap(),
            ),
            None,
            codeyard_ci::config::CiConfig::default(),
            None,
        ));
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ACCEPT,
            codeyard_webui::bootstrap::MODEL_CONTENT_TYPE
                .parse()
                .unwrap(),
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
        let envelope: codeyard_webui::app::PageEnvelope = serde_json::from_slice(&body).unwrap();
        let codeyard_webui::app::Page::Collaboration(model) = envelope.page else {
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
