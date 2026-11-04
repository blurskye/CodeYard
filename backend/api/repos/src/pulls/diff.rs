use crate::access::readable_repository;
use crate::discussion::item;
use crate::discussion::repository;
use crate::discussion::unavailable;
use crate::discussion::RepositoryData;
use crate::discussion::ResultPage;
use crate::refs::references::resolve_references_or_none;
use crate::responses::storage_unavailable;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::shared::repos::discussion::page;
use codeyard_api_http::shared::repos::forms::with_status;
use codeyard_app::app::AppState;
use codeyard_ci_engine::code_owners::load_rules;
use codeyard_git::DiffFileInfo;
use codeyard_git::DiffInfo;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_webui::pages::repos::issues as view;
use codeyard_webui::pages::repos::issues::Kind;
use codeyard_webui::pages::repos::pulls;
use codeyard_webui::pages::repos::pulls::LineComment;
use codeyard_webui::pages::repos::pulls::ReviewDraft;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

/// Whether the viewer chose the split diff view.
fn split_view(ctx: &PageContext) -> bool {
    ctx.query.get("view").is_some_and(|view| view == "split")
}

/// The diff page's address, keeping the view the viewer chose.
pub(crate) fn diff_url(ctx: &PageContext, base: &str) -> String {
    let view = if split_view(ctx) { "?view=split" } else { "" };
    format!("{base}/diff{view}")
}

pub async fn get_pull_request_diff(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
) -> Result<Json<DiffInfo>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;

    let pull_request = codeyard_db_repos::pull_requests::records::get_pull_request(
        &state.pool,
        repository.id,
        number,
    )
    .await
    .map_err(|_| storage_unavailable())?
    .ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Pull request not found")),
        )
    })?;

    let diff = repo_store(&repository, state.cluster.as_ref())
        .diff(pull_request.diff_base(), &pull_request.head_sha)
        .await
        .map_err(|error| {
            crate::responses::repo_store_error(error, StatusCode::INTERNAL_SERVER_ERROR)
        })?;

    Ok(Json(diff))
}

pub(crate) async fn native_diff(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    let composer = composer_from_query(&query);
    render_diff(
        &ctx,
        &repo,
        &value,
        composer.as_ref(),
        None,
        None,
        StatusCode::OK,
    )
    .await
}

/// The files a signed-in reader marked as viewed. A mark counts only while the file's blob id is
/// the one it was marked at, so a push that changes the file shows it unviewed again.
async fn viewed_marks(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull_id: i64,
    pull: &codeyard_db_core::models::PullRequest,
    user_id: i64,
    files: &[DiffFileInfo],
) -> ResultPage<pulls::Viewed> {
    let displayed: Vec<String> = files.iter().map(|file| file.filename.clone()).collect();
    let marks: HashMap<String, String> =
        codeyard_db_repos::pull_requests::viewed::list_pull_request_viewed_files(
            &ctx.state.pool,
            pull_id,
            user_id,
            &displayed,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        .into_iter()
        .collect();
    let paths: BTreeSet<String> = files
        .iter()
        .filter(|file| marks.get(&file.filename) == Some(&file.blob_sha))
        .map(|file| file.filename.clone())
        .collect();
    Ok(pulls::Viewed {
        csrf: repo.view.csrf.clone(),
        action: format!(
            "{}/{}/viewed",
            repo.view.family_url(Kind::Pull),
            pull.number
        ),
        head: pull.head_sha.clone(),
        base: pull.base_sha.clone(),
        paths,
    })
}

/// The composer a gutter link opens: `side` and `line` name the line, `path` the file.
fn composer_from_query(query: &HashMap<String, String>) -> Option<LineComment> {
    let side = query.get("side")?;
    let line = query.get("line")?;
    if !matches!(side.as_str(), "LEFT" | "RIGHT") || !line.parse::<i32>().is_ok_and(|n| n >= 1) {
        return None;
    }
    Some(LineComment {
        repository_id: None,
        path: query.get("path")?.clone(),
        side: side.clone(),
        line: line.clone(),
        start_line: query.get("start_line").cloned().unwrap_or_default(),
        start_side: query.get("start_side").cloned().unwrap_or_default(),
        in_reply_to: None,
        body: String::new(),
        preview: false,
        commit: String::new(),
        base: String::new(),
    })
}

/// The diff as the Files changed page, a comparison and a commit page show it. `viewed` holds the
/// reader's Viewed marks where a pull request has them.
pub(crate) fn diff_model(diff: DiffInfo, viewed: Option<pulls::Viewed>) -> pulls::Diff {
    pulls::Diff {
        additions: diff.additions,
        deletions: diff.deletions,
        files: diff
            .files
            .into_iter()
            .map(|file| pulls::DiffFile {
                filename: file.filename,
                status: file.status,
                additions: file.additions,
                deletions: file.deletions,
                patch: file.patch,
                owners: Vec::new(),
            })
            .collect(),
        viewed,
    }
}

/// The Files changed page: the diff with its inline conversations, and a composer open under
/// `composer`'s line. A refused comment or resolve comes back here with its error, and a refused
/// review comes back with its text in the review form.
pub(crate) async fn render_diff(
    ctx: &PageContext,
    repo: &RepositoryData,
    value: &view::Item,
    composer: Option<&LineComment>,
    review: Option<ReviewDraft>,
    error: Option<&str>,
    status: StatusCode,
) -> ResultPage<Response> {
    let mut pull = codeyard_db_repos::pull_requests::records::get_pull_request(
        &ctx.state.pool,
        repo.record.id,
        value.number,
    )
    .await
    .map_err(|_| unavailable(ctx))?
    .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found"))?;
    // A retargeted stack layer compares against its saved fork point, so the merged lower
    // layer is not shown again. The base SHA stays for the revision checks.
    let fork = (pull.diff_base() != pull.base_sha).then(|| pull.diff_base().to_owned());
    // Keep the patch, thread state and form fields on the item's revision snapshot.
    if let Some(compare) = &value.comparison {
        pull.head_sha.clone_from(&compare.head_sha);
        pull.base_sha.clone_from(&compare.base_sha);
    }
    let diff = repo_store(&repo.record, ctx.state.cluster.as_ref())
        .diff(fork.as_deref().unwrap_or(&pull.base_sha), &pull.head_sha)
        .await
        .map_err(|error| {
            if error.is_response_too_large() {
                ctx.error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Repository response exceeds its limit.",
                )
            } else {
                unavailable(ctx)
            }
        })?;
    let code_scanning =
        crate::pulls::security::checks(ctx, repo, &pull, &pull.head_sha, &pull.base_sha).await?;
    let inline = codeyard_db_repos::pull_requests::reviews::list_pull_request_inline_comments(
        &ctx.state.pool,
        pull.id,
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    let truncated = inline.truncated;
    // The viewer's own pending comments show here, and nobody else's do.
    let pending = match ctx.user.as_ref() {
        Some(user) => codeyard_db_repos::pull_requests::reviews::list_pending_review_comments(
            &ctx.state.pool,
            pull.id,
            user.id,
        )
        .await
        .map_err(|_| unavailable(ctx))?,
        None => Default::default(),
    };
    let truncated = truncated || pending.truncated;
    // The badge counts every pending comment, though the byte budget may show fewer.
    let pending_comments = pending.count;
    let pending = pending.comments;
    let pending_ids: HashSet<i64> = pending.iter().map(|entry| entry.comment.id).collect();
    let stored = match (&review, ctx.user.as_ref()) {
        (None, Some(user)) => codeyard_db_repos::pull_requests::reviews::get_pending_review(
            &ctx.state.pool,
            pull.id,
            user.id,
        )
        .await
        .map_err(|_| unavailable(ctx))?,
        _ => None,
    };
    // A re-rendered form keeps the pending review it was submitted with. A fresh form names the
    // stored one, which may hold only a summary.
    let pending_review = match &review {
        Some(draft) => draft.pending_review,
        None => stored.as_ref().map(|pending| pending.id),
    };
    // A pending review started through the API can carry a summary. The form starts from it,
    // so submitting the review keeps it.
    let review = review.or_else(|| {
        stored
            .and_then(|pending| pending.body)
            .filter(|body| !body.is_empty())
            .map(|body| ReviewDraft {
                state: "commented".to_owned(),
                body,
                preview: false,
                commit: String::new(),
                repository_id: None,
                pending_review,
            })
    });
    let mut entries = inline.comments;
    entries.extend(pending);
    let mut threads = crate::pulls::review_forms::threads(ctx, repo, &pull, entries, &pending_ids);
    crate::issues::reactions::attach_threads(ctx, &mut threads).await?;
    let mut texts: Vec<&str> = threads
        .iter()
        .flat_map(|thread| {
            std::iter::once(thread.root.body.as_str())
                .chain(thread.replies.iter().map(|reply| reply.body.as_str()))
        })
        .collect();
    texts.extend(composer.map(|draft| draft.body.as_str()));
    texts.extend(review.as_ref().map(|draft| draft.body.as_str()));
    let references = resolve_references_or_none(
        ctx,
        &repo.record,
        &repo.view.owner,
        &repo.view.name,
        &texts,
        false,
    )
    .await;
    let viewed = match ctx.user.as_ref() {
        Some(user) => Some(viewed_marks(ctx, repo, pull.id, &pull, user.id, &diff.files).await?),
        None => None,
    };
    let mut diff = diff_model(diff, viewed);
    // A CODEOWNERS file that cannot be read leaves the owner chips out; the page still shows.
    let rules = load_rules(&repo.record, ctx.state.cluster.as_ref(), &pull.base_sha)
        .await
        .unwrap_or_default();
    let paths: Vec<String> = diff
        .files
        .iter()
        .map(|file| file.filename.clone())
        .collect();
    let owned: HashMap<String, &[String]> = rules
        .owned_paths(&paths)
        .unwrap_or_default()
        .into_iter()
        .collect();
    for file in &mut diff.files {
        file.owners = owned
            .get(&file.filename)
            .copied()
            .unwrap_or_default()
            .to_vec();
    }
    let mut ctx = ctx.clone();
    let split = split_view(&ctx);
    ctx.return_to = Some(diff_url(
        &ctx,
        &format!("{}/{}", repo.view.family_url(Kind::Pull), value.number),
    ));
    Ok(with_status(
        ctx.page_model(
            "Pull request diff",
            page(view::Page::Diff {
                repository: repo.view.clone(),
                item: value.clone(),
                diff,
                code_scanning,
                threads,
                truncated,
                composer: composer.cloned(),
                review,
                pending_comments,
                pending_review,
                error: error.map(str::to_owned),
                references: Box::new(references),
                split,
            }),
        ),
        status,
    ))
}
