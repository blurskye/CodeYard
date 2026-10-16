use crate::backend::api::context::PageContext;
use crate::backend::api::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{
    commit, default_ref, ensure_ref, json_page, load_refs_from_repo, number, page_window,
    repository_default_branch, store_error, Loaded, DEFAULT_PER_PAGE, MAX_PAGE,
};
use crate::backend::api::repos::responses::{AuthorInfo, CommitResponse};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::repo_store;
use crate::webui::pages::repositories as view;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct CommitsQuery {
    pub page: Option<u32>,
    pub per_page: Option<u32>,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
}

pub async fn list_commits(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<CommitsQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<CommitResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Repository operation is unavailable",
                )),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_read_access(&state.pool, &repository, &headers).await?;

    let (offset, limit) = json_page(query.page, query.per_page)?;
    let refs = load_refs_from_repo(&state, &repository, &format!("{}/{}", owner, repo)).await?;

    if refs.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let git_ref = if let Some(ref git_ref) = query.git_ref {
        git_ref.clone()
    } else {
        crate::backend::repositories::reads::default_branch(
            &refs,
            &repository_default_branch(&state, &repository).await?,
            &repository.default_branch,
        )
    };

    let commits = repo_store(&repository, state.cluster.as_ref())
        .commits(&git_ref, offset, limit)
        .await
        .map_err(|e| {
            crate::backend::api::repos::responses::repo_store_error(
                e,
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;

    let emails: Vec<String> = commits
        .iter()
        .map(|commit| commit.author_email.clone())
        .collect();
    let users = db::users::accounts::get_users_by_emails(&state.pool, &emails)
        .await
        .unwrap_or_default();
    let user_map: HashMap<String, &db::models::User> = users
        .iter()
        .filter_map(|user| user.email.clone().map(|email| (email, user)))
        .collect();

    let responses = commits
        .into_iter()
        .map(|commit| {
            let author = user_map.get(&commit.author_email).map(|user| AuthorInfo {
                id: Some(user.id),
                username: Some(user.username.clone()),
                avatar_url: user
                    .avatar_file_id
                    .map(|id| format!("/api/files/{}/download", id)),
            });
            CommitResponse::from_commit(commit, author)
        })
        .collect();

    Ok(Json(responses))
}

async fn commits_page(ctx: &PageContext, loaded: &Loaded) -> Result<Response, Response> {
    let reference = ctx
        .query
        .get("ref")
        .cloned()
        .unwrap_or_else(|| default_ref(loaded));
    let page = number(ctx, "page", 1, 1000)?;
    if page == 0 {
        return Err(ctx.error(StatusCode::BAD_REQUEST, "Page numbers start at 1."));
    }
    let mut rows = Vec::new();
    let mut more = false;
    if !loaded.refs.is_empty() {
        ensure_ref(ctx, loaded, &reference)?;
        let (offset, _) = page_window(page, DEFAULT_PER_PAGE)
            .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid commit page."))?;
        let commits = repo_store(&loaded.repository, ctx.state.cluster.as_ref())
            .commits(&reference, offset, DEFAULT_PER_PAGE + 1)
            .await
            .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
        // The last permitted page has no successor: the handler would refuse it.
        more = commits.len() > DEFAULT_PER_PAGE && page < MAX_PAGE;
        rows = commits
            .into_iter()
            .take(DEFAULT_PER_PAGE)
            .map(commit)
            .collect();
    }
    Ok(ctx.page_model(
        "Commits",
        crate::webui::app::Page::Repositories(view::Page::Commits {
            repo: loaded.chrome.clone(),
            rows,
            reference,
            page,
            has_more: more,
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
    let ctx = match crate::backend::api::context::PageContext::load(state, headers, &uri).await {
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
    match commits_page(&ctx, &loaded).await {
        Ok(response) | Err(response) => response,
    }
}

/// GitHub addresses a branch's history as `/commits/<branch>`. The list takes the branch
/// as `?ref=`.
pub(super) async fn ref_redirect(
    Path((owner, repo, git_ref)): Path<(String, String, String)>,
) -> Response {
    let mut url = url::Url::parse("http://localhost/").expect("static base address");
    url.path_segments_mut()
        .expect("http base address has path segments")
        .extend([owner.as_str(), repo.as_str(), "commits"]);
    url.query_pairs_mut().append_pair("ref", &git_ref);
    let target = format!("{}?{}", url.path(), url.query().unwrap_or_default());
    crate::backend::api::render::redirect_response(&target)
}
