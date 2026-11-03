use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::{
    access_error, check_repo_read_access, readable_repository,
};
use crate::backend::api::repos::chrome::{
    commit, default_ref, ensure_ref, load_refs_from_repo, ref_names_under,
    repository_default_branch, store_error, Loaded,
};
use crate::backend::api::repos::paging::{
    json_page, number, page_window, DEFAULT_PER_PAGE, MAX_PAGE,
};
use crate::backend::api::repos::responses::{AuthorInfo, CommitResponse};
use crate::backend::app::AppState;
use crate::backend::repositories::access::AccessError;
use crate::backend::repositories::store::{repo_store, CommitFilter};
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use codeyard_webui::pages::repos as view;
use serde::Deserialize;
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
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;

    let (offset, limit) = json_page(query.page, query.per_page)?;
    let refs = load_refs_from_repo(&state, &repository).await?;

    if refs.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let git_ref = if let Some(ref git_ref) = query.git_ref {
        git_ref.clone()
    } else {
        // Qualified, so a branch named like `refs/topic` is not read as a full reference.
        format!(
            "refs/heads/{}",
            crate::backend::repositories::reads::default_branch(
                &refs,
                &repository_default_branch(&state, &repository).await?,
                &repository.default_branch,
            )
        )
    };

    let commits = repo_store(&repository, state.cluster.as_ref())
        .commits(&git_ref, offset, limit, &Default::default())
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
    // The same lookup as a single commit, so both name the account behind a noreply address.
    let user_map = crate::backend::repositories::reads::commit_authors(&state.pool, &emails)
        .await
        .map_err(|_| access_error(AccessError::Unavailable))?;

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

    check_repo_read_access(&state.pool, &repository, &headers).await?;
    Ok(Json(responses))
}

/// The `path` filter of the history page, normalised as the file routes read their paths: empty
/// parts are dropped. A `.` or `..` part names no file, so there is no such path (`None`).
fn history_path(ctx: &PageContext) -> Option<String> {
    let parts = view::ref_path::split_path(ctx.query.get("path").map_or("", String::as_str));
    if parts
        .iter()
        .any(|part| part == "." || part == ".." || part.contains('\0'))
    {
        return None;
    }
    Some(parts.join("/"))
}

/// The author and date filters of the history page, as the store applies them and as the form
/// shows them again.
#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
fn history_filter(
    ctx: &PageContext,
) -> Result<(CommitFilter, view::commits::HistoryFilter), Response> {
    let field = |name: &str| {
        ctx.query
            .get(name)
            .map_or("", String::as_str)
            .trim()
            .to_owned()
    };
    let (author, mut since, until) = (field("author"), field("since"), field("until"));
    // A range such as `week` stands for the day it begins, so the form shows the real date.
    if let Some(days) = [("day", 1), ("week", 7), ("month", 30), ("year", 365)]
        .into_iter()
        .find_map(|(name, days)| (name == since).then_some(days))
    {
        since = (chrono::Utc::now().date_naive() - chrono::Duration::days(days))
            .format("%Y-%m-%d")
            .to_string();
    }
    let filter = CommitFilter {
        author: (!author.is_empty()).then(|| author.clone()),
        since: day_bound(ctx, &since, false)?,
        until: day_bound(ctx, &until, true)?,
        ..CommitFilter::default()
    };
    Ok((
        filter,
        view::commits::HistoryFilter {
            author,
            since,
            until,
        },
    ))
}

/// The first second of the UTC day that `value` names, or its last second when `last_second`.
/// An empty value names no day.
#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
fn day_bound(ctx: &PageContext, value: &str, last_second: bool) -> Result<Option<i64>, Response> {
    if value.is_empty() {
        return Ok(None);
    }
    let day = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| ctx.error(StatusCode::BAD_REQUEST, "Dates must be YYYY-MM-DD."))?;
    let (hour, minute, second) = if last_second { (23, 59, 59) } else { (0, 0, 0) };
    Ok(day
        .and_hms_opt(hour, minute, second)
        .map(|time| time.and_utc().timestamp()))
}

async fn commits_page(ctx: &PageContext, loaded: &Loaded) -> Result<Response, Response> {
    let reference = ctx
        .query
        .get("ref")
        .cloned()
        .unwrap_or_else(|| default_ref(loaded));
    let page = number(ctx, "page", 1, 1..=MAX_PAGE)?;
    let Some(path) = history_path(ctx) else {
        return Err(ctx.repo_error(
            &loaded.chrome,
            StatusCode::NOT_FOUND,
            "That path is not in this repository.",
        ));
    };
    let (filter, shown_filter) = history_filter(ctx)?;
    let mut rows = Vec::new();
    let mut more = false;
    if !loaded.refs.is_empty() {
        ensure_ref(ctx, loaded, &reference)?;
        let (offset, _) = page_window(page, DEFAULT_PER_PAGE)
            .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid commit page."))?;
        let store = repo_store(&loaded.repository, ctx.state.cluster.as_ref());
        let fetched = DEFAULT_PER_PAGE + 1;
        let commits = if path.is_empty() {
            store.commits(&reference, offset, fetched, &filter).await
        } else {
            store
                .path_commits(&reference, &path, offset, fetched, &filter)
                .await
        };
        let commits = commits.map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
        // The last permitted page has no successor: the handler would refuse it.
        more = commits.len() > DEFAULT_PER_PAGE && page < MAX_PAGE;
        rows = commits
            .into_iter()
            .take(DEFAULT_PER_PAGE)
            .map(commit)
            .collect();
    }
    let title = if path.is_empty() {
        "Commits".to_owned()
    } else {
        format!("Commits to {path}")
    };
    check_repo_read_access(&ctx.state.pool, &loaded.repository, &ctx.headers)
        .await
        .map_err(|(status, _)| ctx.error(status, "Repository not found."))?;
    Ok(ctx.page_model(
        &title,
        codeyard_webui::app::Page::Repositories(view::page::Page::Commits {
            repo: loaded.chrome.clone(),
            rows,
            reference,
            path,
            page,
            has_more: more,
            branches: ref_names_under(loaded, "refs/heads/"),
            tags: ref_names_under(loaded, "refs/tags/"),
            filter: shown_filter,
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
    match commits_page(&ctx, &loaded).await {
        Ok(response) | Err(response) => response,
    }
}

/// GitHub addresses a branch's history as `/commits/<branch>`. The list takes the branch
/// as `?ref=` and keeps the other filters.
pub(super) async fn ref_redirect(
    Path((owner, repo, git_ref)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    let mut url = url::Url::parse("http://localhost/").expect("static base address");
    url.path_segments_mut()
        .expect("http base address has path segments")
        .extend([owner.as_str(), repo.as_str(), "commits"]);
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("ref", &git_ref);
        for (name, value) in
            url::form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes())
        {
            if name != "ref" {
                pairs.append_pair(&name, &value);
            }
        }
    }
    let target = format!("{}?{}", url.path(), url.query().unwrap_or_default());
    crate::backend::api::http::render::redirect_response(&target)
}
