//! Native participation in the same commit discussions exposed by REST.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::refs::commits::commit::commit_page;

use crate::backend::api::repos::chrome::Loaded;
use crate::backend::api::repos::forms::authorize_post;
use crate::backend::api::repos::forms::field;
use crate::backend::api::repos::forms::with_status;

use crate::backend::api::repos::refs::references::resolve_references_or_none;
use crate::backend::app::AppState;

use crate::backend::repositories::commit_comments as comments;
use crate::backend::repositories::commit_comments::CommentError;
use axum::{
    extract::{MatchedPath, OriginalUri, Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Form,
};
use codeyard_webui::pages::repos::commits::comments::{Comment, Discussion, Draft};
use std::{collections::HashMap, sync::Arc};
const PER_PAGE: usize = 20;

fn failure(error: &CommentError) -> StatusCode {
    match error {
        CommentError::Missing => StatusCode::NOT_FOUND,
        CommentError::Denied => StatusCode::FORBIDDEN,
        CommentError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        CommentError::Conflict => StatusCode::CONFLICT,
        CommentError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    }
}

pub(super) async fn discussion(
    ctx: &PageContext,
    loaded: &Loaded,
    sha: &str,
    draft: Option<Draft>,
    error: Option<String>,
) -> Result<Discussion, Response> {
    let pool = &ctx.state.pool;
    let unavailable = || {
        ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Commit discussions are temporarily unavailable.",
        )
    };
    let target = match draft.as_ref().and_then(|draft| draft.id) {
        Some(id) => Some(id),
        None => ctx
            .query
            .get("comment")
            .map(|value| value.parse::<i64>())
            .transpose()
            .map_err(|_| ctx.error(StatusCode::BAD_REQUEST, "Invalid comment ID."))?,
    };
    let mut page = crate::backend::api::repos::paging::number(ctx, "comments_page", 1, 1..=50_001)?;
    if let Some(id) = target {
        match codeyard_db_repos::repositories::commit_comments::get_comment(
            pool,
            loaded.repository.id,
            id,
        )
        .await
        .map_err(|_| unavailable())?
        {
            Some(comment) if comment.sha == sha => {
                let through = codeyard_db_repos::repositories::commit_comments::count_comments(
                    pool,
                    loaded.repository.id,
                    sha,
                    Some(id),
                )
                .await
                .map_err(|_| unavailable())?;
                page = usize::try_from((through.max(1) - 1) / PER_PAGE as i64 + 1)
                    .map_err(|_| unavailable())?;
            }
            _ if draft.is_some() => (),
            _ => return Err(ctx.error(StatusCode::NOT_FOUND, "Commit comment not found.")),
        }
    }
    let offset = page
        .saturating_sub(1)
        .checked_mul(PER_PAGE)
        .filter(|offset| *offset <= 1_000_000)
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Comment page exceeds its limit."))?;
    let mut rows = codeyard_db_repos::repositories::commit_comments::list_comments(
        pool,
        loaded.repository.id,
        Some(sha),
        offset as i64,
        (PER_PAGE + 1) as i64,
    )
    .await
    .map_err(|_| unavailable())?;
    let has_more = rows.len() > PER_PAGE;
    rows.truncate(PER_PAGE);
    let total = codeyard_db_repos::repositories::commit_comments::count_comments(
        pool,
        loaded.repository.id,
        sha,
        None,
    )
    .await
    .map_err(|_| unavailable())?;
    let can_comment = !loaded.repository.is_archived
        && ctx
            .user
            .as_ref()
            .is_some_and(|user| !user.is_banned && !user.must_change_password);
    let maintain = match ctx.user.as_ref() {
        Some(user) => {
            crate::backend::repositories::access::can_maintain(pool, user, &loaded.repository)
                .await
                .map_err(|_| unavailable())?
        }
        None => false,
    };
    let entries =
        comments::comment_details(pool, ctx.state.cluster.as_ref(), &loaded.repository, rows)
            .await
            .map_err(|_| unavailable())?;
    let ids = entries
        .iter()
        .map(|entry| entry.comment.id)
        .collect::<Vec<_>>();
    let counts = codeyard_db_repos::reactions::reaction_counts(
        pool,
        "commit_comment",
        &ids,
        ctx.user.as_ref().map_or(0, |user| user.id),
    )
    .await
    .map_err(|_| unavailable())?;
    let rows: Vec<_> = entries
        .into_iter()
        .map(|entry| {
            let comment = entry.comment;
            let own = comment.author_id == ctx.user.as_ref().map(|user| user.id)
                && comment.author_id.is_some();
            Comment {
                id: comment.id,
                author: entry.author.map(|person| person.login),
                body: comment.body.clone(),
                created: comment.created_at.to_rfc3339(),
                edited: (comment.created_at != comment.updated_at)
                    .then(|| comment.updated_at.to_rfc3339()),
                location: comment.path.as_ref().map(|path| {
                    comment
                        .line
                        .map_or_else(|| path.clone(), |line| format!("{path}:{line}"))
                }),
                association: entry.association,
                revision: codeyard_db_repos::discussions::comment_revision(&comment.body),
                can_edit: can_comment && own,
                can_delete: can_comment && (own || maintain),
                reactions: counts
                    .iter()
                    .filter(|count| count.subject_id == comment.id)
                    .map(|count| codeyard_webui::pages::repos::issues::Reaction {
                        content: count.content.clone(),
                        count: count.count,
                        mine: count.mine,
                    })
                    .collect(),
            }
        })
        .collect();
    let mut texts: Vec<&str> = rows.iter().map(|comment| comment.body.as_str()).collect();
    if let Some(draft) = &draft {
        texts.push(&draft.body);
    }
    let references = resolve_references_or_none(
        ctx,
        &loaded.repository,
        &loaded.chrome.owner,
        &loaded.chrome.name,
        &texts,
        false,
    )
    .await;
    Ok(Discussion {
        comments: rows,
        total,
        page,
        has_more,
        can_comment,
        draft,
        error,
        references,
    })
}

pub(super) async fn react(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<HashMap<String, String>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &form) {
        return response;
    }
    let loaded =
        match crate::backend::api::repos::chrome::load(&ctx, &params["owner"], &params["repo"])
            .await
        {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    let actor = match ctx.require_user() {
        Ok(actor) => actor.id,
        Err(response) => return response,
    };
    let id = match params["id"].parse::<i64>() {
        Ok(id) => id,
        Err(_) => return ctx.error(StatusCode::NOT_FOUND, "Commit comment not found."),
    };
    let row = match codeyard_db_repos::repositories::commit_comments::get_comment(
        &ctx.state.pool,
        loaded.repository.id,
        id,
    )
    .await
    {
        Ok(Some(row)) if row.sha == params["sha"] => row,
        Ok(_) => return ctx.error(StatusCode::NOT_FOUND, "Commit comment not found."),
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Commit discussion unavailable.",
            )
        }
    };
    let content = field(&form, "content");
    if !codeyard_db_repos::reactions::valid_content(
        codeyard_db_repos::reactions::Subject::CommitComment,
        &content,
    ) {
        return ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Unknown reaction.");
    }
    if let Err(error) = codeyard_db_repos::reactions::toggle_reaction(
        &ctx.state.pool,
        actor,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &crate::backend::api::http::session::extract_session_id(&ctx.headers)
                .unwrap_or_default(),
        ),
        loaded.repository.id,
        row.id,
        codeyard_db_repos::reactions::Subject::CommitComment,
        row.id,
        &content,
    )
    .await
    {
        return if error
            .downcast_ref::<codeyard_db_repos::reactions::ReactionRefusal>()
            .is_some()
        {
            ctx.error(StatusCode::NOT_FOUND, "Commit comment not found.")
        } else if error
            .downcast_ref::<codeyard_db_core::queries::discussions::DiscussionWriteRefusal>()
            .is_some()
        {
            ctx.error(StatusCode::FORBIDDEN, "Reaction mutation denied.")
        } else {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Reaction service unavailable.",
            )
        };
    }
    let view = if ctx.query.get("view").is_some_and(|s| s == "split") {
        "&view=split"
    } else {
        ""
    };
    ctx.redirect(&format!(
        "{}/commit/{}?comment={id}{view}#commitcomment-{id}",
        loaded.chrome.base(),
        row.sha
    ))
}

pub(super) async fn submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<HashMap<String, String>>,
    route: MatchedPath,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &form) {
        return response;
    }
    // A Git outage must not replace the submitted text: the commit page then fails and the
    // draft is shown by the fallback below.
    let loaded = match crate::backend::api::repos::chrome::load_metadata(
        &ctx,
        &params["owner"],
        &params["repo"],
    )
    .await
    {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    let id = match params
        .get("id")
        .map(|value| value.parse::<i64>())
        .transpose()
    {
        Ok(id) => id,
        Err(_) => return ctx.error(StatusCode::NOT_FOUND, "Commit comment not found."),
    };
    // Invalid authority fields stay invalid on a retry, without reflecting unbounded hidden text.
    let repository_id = form
        .get("repository_id")
        .and_then(|value| value.parse::<i64>().ok())
        .map(|id| id.to_string())
        .unwrap_or_default();
    let mut commit_sha = field(&form, "commit_sha");
    if !codeyard_git::is_full_commit_id(&commit_sha) {
        commit_sha.clear();
    }
    let mut revision = field(&form, "expected_revision");
    if revision.len() != 64 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        revision.clear();
    }
    let draft = Draft {
        id,
        body: field(&form, "body"),
        revision,
        preview: field(&form, "intent") == "preview",
        repository_id: Some(repository_id),
        commit_sha: Some(commit_sha),
    };
    let sha = &params["sha"];
    let operation = async {
        if !codeyard_git::is_full_commit_id(sha) {
            return Err(CommentError::Invalid("Use an immutable full commit SHA"));
        }
        if field(&form, "repository_id").parse::<i64>().ok() != Some(loaded.repository.id)
            || field(&form, "commit_sha") != *sha
        {
            return Err(CommentError::Conflict);
        }
        if loaded.repository.is_archived
            || ctx
                .user
                .as_ref()
                .is_none_or(|user| user.is_banned || user.must_change_password)
        {
            return Err(CommentError::Denied);
        }
        if let Some(id) = id {
            let comment = codeyard_db_repos::repositories::commit_comments::get_comment(
                &ctx.state.pool,
                loaded.repository.id,
                id,
            )
            .await
            .map_err(|_| CommentError::Unavailable)?
            .ok_or(CommentError::Missing)?;
            if comment.sha != *sha {
                return Err(CommentError::Missing);
            }
            let own = comment.author_id == Some(actor);
            if !own
                && (!route.as_str().ends_with("/delete")
                    || !crate::backend::repositories::access::can_maintain(
                        &ctx.state.pool,
                        ctx.user.as_ref().ok_or(CommentError::Denied)?,
                        &loaded.repository,
                    )
                    .await
                    .map_err(|_| CommentError::Unavailable)?)
            {
                return Err(CommentError::Denied);
            }
        }
        if draft.preview {
            return Ok(None);
        }
        if let Some(id) = id {
            if route.as_str().ends_with("/delete") {
                comments::delete(
                    &ctx.state.pool,
                    ctx.database_actor(actor),
                    loaded.repository.id,
                    id,
                    Some(&draft.revision),
                )
                .await
                .map(|_| None)
            } else {
                comments::edit(
                    &ctx.state.pool,
                    ctx.state.cluster.as_ref(),
                    &ctx.state.notification_broadcaster,
                    codeyard_db_repos::repositories::commit_comments::CommentEdit {
                        actor: ctx.database_actor(actor),
                        repo_id: loaded.repository.id,
                        id,
                        body: &draft.body,
                        expected_revision: Some(&draft.revision),
                    },
                )
                .await
                .map(Some)
            }
        } else {
            comments::create(
                &ctx.state.pool,
                ctx.state.cluster.as_ref(),
                &ctx.state.notification_broadcaster,
                ctx.database_actor(actor),
                loaded.repository.id,
                sha,
                comments::CreateComment {
                    body: draft.body.clone(),
                    path: None,
                    position: None,
                    line: None,
                    repository_id: Some(loaded.repository.id),
                },
            )
            .await
            .map(Some)
        }
    }
    .await;
    let (status, message) = match operation {
        Ok(Some(entry)) => {
            let mut target =
                comments::comment_link(&loaded.chrome.owner, &loaded.chrome.name, &entry.comment);
            if ctx.query.get("view").is_some_and(|view| view == "split") {
                if let Some(anchor) = target.find('#') {
                    target.insert_str(anchor, "&view=split");
                }
            }
            return ctx.redirect(&target);
        }
        Ok(None) if route.as_str().ends_with("/delete") && !draft.preview => {
            return ctx.redirect(&format!(
                "{}/commit/{sha}?comments_page={}{}#comments",
                loaded.chrome.base(),
                ctx.query
                    .get("comments_page")
                    .and_then(|page| page.parse::<usize>().ok())
                    .unwrap_or(1),
                if ctx.query.get("view").is_some_and(|view| view == "split") {
                    "&view=split"
                } else {
                    ""
                }
            ));
        }
        Ok(None) => (StatusCode::OK, None),
        Err(error) => (failure(&error), Some(error.to_string())),
    };
    // The form's own address takes only POST, so appearance changes return to the commit page.
    if codeyard_git::is_full_commit_id(sha) {
        ctx.return_to = Some(format!("{}/commit/{sha}", loaded.chrome.base()));
    }
    match commit_page(&ctx, &loaded, sha, Some(draft.clone()), message.clone()).await {
        Ok(response) => with_status(response, status),
        Err(response) => {
            let fallback = ctx.page_model(
                "Commit comment draft",
                codeyard_webui::app::Page::Repositories(
                    codeyard_webui::pages::repos::page::Page::CommitCommentDraft {
                        repo: loaded.chrome.clone(),
                        sha: sha.clone(),
                        draft: Box::new(draft),
                        message: message.unwrap_or_else(|| {
                            "The commit could not be loaded. Reload before retrying.".into()
                        }),
                    },
                ),
            );
            with_status(fallback, response.status())
        }
    }
}
