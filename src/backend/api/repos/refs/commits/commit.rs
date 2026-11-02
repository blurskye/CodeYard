use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::{
    access_error, check_repo_read_access, readable_repository,
};
use crate::backend::api::repos::chrome::{commit, hex_revision, store_error, Loaded};
use crate::backend::api::repos::pulls::diff::diff_model;
use crate::backend::api::repos::responses::{AuthorInfo, CommitResponse};
use crate::backend::app::AppState;
use crate::backend::repositories::access::AccessError;
use crate::backend::repositories::store::{repo_store, DiffInfo};
use crate::webui::pages::repos as view;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
#[derive(Serialize)]
pub struct CommitDetailResponse {
    pub commit: CommitResponse,
    pub diff: DiffInfo,
}

pub async fn get_commit(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<CommitDetailResponse>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;

    let store = repo_store(&repository, state.cluster.as_ref());
    let commit_info = store.commit(&sha).await.map_err(|e| {
        crate::backend::api::repos::responses::repo_store_error(
            e,
            StatusCode::INTERNAL_SERVER_ERROR,
        )
    })?;
    let diff = store.commit_diff(&commit_info.sha).await.map_err(|e| {
        crate::backend::api::repos::responses::repo_store_error(
            e,
            StatusCode::INTERNAL_SERVER_ERROR,
        )
    })?;

    let author = crate::backend::repositories::reads::commit_authors(
        &state.pool,
        std::slice::from_ref(&commit_info.author_email),
    )
    .await
    .map_err(|_| access_error(AccessError::Unavailable))?
    .remove(&commit_info.author_email)
    .map(|user| AuthorInfo {
        id: Some(user.id),
        username: Some(user.username),
        avatar_url: user
            .avatar_file_id
            .map(|id| format!("/api/files/{}/download", id)),
    });

    check_repo_read_access(&state.pool, &repository, &headers).await?;
    Ok(Json(CommitDetailResponse {
        commit: CommitResponse::from_commit(commit_info, author),
        diff,
    }))
}

/// The branch and tag names, sorted, whose tip is `sha`. Finding every ref that contains a commit
/// would walk the history of each one, so only the refs that point at the commit are listed.
fn refs_at(loaded: &Loaded, sha: &str) -> (Vec<String>, Vec<String>) {
    let names = |prefix: &str| -> Vec<String> {
        let mut names: Vec<String> = loaded
            .refs
            .iter()
            .filter(|(_, tip)| tip.as_str() == sha)
            .filter_map(|(name, _)| name.strip_prefix(prefix))
            .map(str::to_owned)
            .collect();
        names.sort();
        names
    };
    (names("refs/heads/"), names("refs/tags/"))
}

async fn related_pulls(
    ctx: &PageContext,
    loaded: &Loaded,
    sha: &str,
) -> Result<view::commits::pulls::RelatedPulls, Response> {
    use crate::backend::repositories::commit_pulls::{associated_pulls, CommitPullQuery};
    use view::commits::pulls::{RelatedPull, RelatedPulls};
    let page = ctx
        .query
        .get("pulls_page")
        .map_or(Some(1), |value| value.parse::<usize>().ok())
        .filter(|page| *page > 0)
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid pull request page."))?;
    let offset = page
        .checked_sub(1)
        .and_then(|page| page.checked_mul(10))
        .filter(|offset| *offset <= 100_000)
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid pull request page."))?;
    let link = |page: usize| {
        let mut query = ctx
            .query
            .iter()
            .filter(|(key, _)| key.as_str() != "pulls_page")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        query.sort();
        query.push(("pulls_page".into(), page.to_string()));
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(query)
            .finish();
        format!(
            "{}/commit/{sha}?{query}#related-pull-requests",
            loaded.chrome.base()
        )
    };
    let previous = (page > 1).then(|| link(page - 1));
    match associated_pulls(
        &ctx.state.pool,
        ctx.state.cluster.as_ref(),
        CommitPullQuery {
            repository_id: loaded.repository.id,
            actor: ctx.user.as_ref().map(|user| user.id),
            reference: sha,
            limit: 10,
            offset,
        },
    )
    .await
    {
        Ok(result) => Ok(RelatedPulls {
            rows: result
                .pulls
                .into_iter()
                .map(|pull| RelatedPull {
                    number: pull.number,
                    title: pull.title,
                    state: if pull.merged {
                        "Merged"
                    } else if pull.is_draft {
                        "Draft"
                    } else {
                        "Open"
                    }
                    .into(),
                })
                .collect(),
            previous,
            next: result.has_more.then(|| link(page + 1)),
            error: None,
        }),
        Err(_) => Ok(RelatedPulls {
            previous,
            error: Some("Associated pull requests could not be loaded.".into()),
            ..Default::default()
        }),
    }
}

pub(super) async fn commit_page(
    ctx: &PageContext,
    loaded: &Loaded,
    sha: &str,
    draft: Option<view::commits::comments::Draft>,
    error: Option<String>,
) -> Result<Response, Response> {
    if !hex_revision(sha) {
        return Err(ctx.error(StatusCode::BAD_REQUEST, "Invalid commit SHA."));
    }
    let store = repo_store(&loaded.repository, ctx.state.cluster.as_ref());
    let info = store
        .commit(sha)
        .await
        .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
    let diff = store
        .commit_diff(&info.sha)
        .await
        .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
    let (branches, mut tags) = refs_at(loaded, &info.sha);
    // An annotated tag's ref names the tag object, so it is matched by the commit it points at.
    if let Ok(all) = store.tags().await {
        tags.extend(
            all.into_iter()
                .filter(|tag| tag.commit == info.sha)
                .map(|tag| tag.name),
        );
        tags.sort();
        tags.dedup();
    }
    let discussion = super::comments::discussion(ctx, loaded, &info.sha, draft, error).await?;
    let related_pulls = related_pulls(ctx, loaded, &info.sha).await?;
    check_repo_read_access(&ctx.state.pool, &loaded.repository, &ctx.headers)
        .await
        .map_err(|(status, _)| ctx.error(status, "Repository not found."))?;
    let info = commit(info);
    Ok(ctx.page_model(
        "Commit",
        crate::webui::app::Page::Repositories(view::page::Page::CommitDetail {
            repo: loaded.chrome.clone(),
            commit: info,
            diff: diff_model(diff, None),
            branches,
            tags,
            discussion: Box::new(discussion),
            related_pulls: Box::new(related_pulls),
            split: ctx.query.get("view").is_some_and(|view| view == "split"),
        }),
    ))
}

pub(super) async fn page(
    axum::extract::State(state): axum::extract::State<
        std::sync::Arc<crate::backend::app::AppState>,
    >,
    headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    axum::extract::Path(params): axum::extract::Path<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let ctx =
        match crate::backend::api::http::context::PageContext::load(state, headers, &uri).await {
            Ok(ctx) => ctx,
            Err(response) => return response,
        };
    let loaded =
        match crate::backend::api::repos::chrome::load(&ctx, &params["owner"], &params["repo"])
            .await
        {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    match commit_page(&ctx, &loaded, &params["sha"], None, None).await {
        Ok(response) | Err(response) => response,
    }
}
