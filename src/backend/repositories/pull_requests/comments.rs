use super::{database_error, OperationError};
use crate::backend::cluster::NodeContext;
use crate::backend::db;
use crate::backend::issues::comments::CommentTarget;
use crate::backend::issues::{discussion_link, discussion_page, COMMENTS_PER_PAGE};
use crate::backend::notifications::create::create_and_broadcast_notification;
use crate::backend::notifications::create::NotificationRequest;
use crate::backend::notifications::mentions::{mentions_anyone, notify_mentions, MentionSource};
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::repositories::store::repo_store;
use crate::backend::webhooks::{self, events, payloads, payloads::Scope};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
#[derive(Deserialize)]
pub struct CreatePullRequestCommentRequest {
    pub body: String,
    /// Immutable identity carried by native forms; JSON clients may also pin it.
    pub repository_id: Option<i64>,
    pub path: Option<String>,
    pub line: Option<i32>,
    pub side: Option<String>,
    pub start_line: Option<i32>,
    pub start_side: Option<String>,
    pub commit_sha: Option<String>,
    pub base_sha: Option<String>,
    pub in_reply_to_comment_id: Option<i64>,
    /// A new thread on a file goes into the author's pending review instead of being published.
    #[serde(default)]
    pub pending: bool,
    pub review_id: Option<i64>,
}

pub(super) type DiffCache =
    std::collections::HashMap<(String, String, String), std::sync::Arc<codeyard_git::DiffInfo>>;

pub(super) struct PreparedComment {
    pub request: CreatePullRequestCommentRequest,
    reply_parent: Option<(i64, i64)>,
}
impl PreparedComment {
    pub(super) fn input<'a>(
        &'a self,
        pull: &'a db::models::PullRequest,
        author: i64,
    ) -> db::pull_requests::reviews::NewPullRequestComment<'a> {
        let req = &self.request;
        db::pull_requests::reviews::NewPullRequestComment {
            pr_id: pull.id,
            author_id: author,
            body: &req.body,
            path: req.path.as_deref(),
            line: req.line,
            side: req.side.as_deref(),
            start_line: req.start_line,
            start_side: req.start_side.as_deref(),
            commit_sha: req.commit_sha.as_deref(),
            in_reply_to_comment_id: req.in_reply_to_comment_id,
            expected_revisions: (req.path.is_some() && req.in_reply_to_comment_id.is_none())
                .then_some((pull.head_sha.as_str(), pull.base_sha.as_str())),
        }
    }
}

#[cfg(test)]
mod tests;

/// The root of the thread that `parent` belongs to. A reply may name another reply as its
/// parent, so the chain is followed up to its root. A chain that loops back on itself has no
/// root and takes no reply.
async fn thread_root(
    pool: &db::DbPool,
    parent: &db::models::PullRequestComment,
) -> Result<i64, OperationError> {
    let mut visited = HashSet::from([parent.id]);
    let mut root_id = parent.id;
    let mut next = parent.in_reply_to_comment_id;
    while let Some(id) = next {
        if !visited.insert(id) {
            return Err(OperationError::Invalid {
                code: "validation_error",
                message: "That comment thread is damaged and cannot be replied to.".to_owned(),
            });
        }
        let comment = db::pull_requests::reviews::get_pull_request_comment_by_id(pool, id)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Parent comment not found".to_owned(),
            })?;
        root_id = comment.id;
        next = comment.in_reply_to_comment_id;
    }
    Ok(root_id)
}

/// The stored link to a pull request comment. The pull request page finds the comment's current
/// page when the link is followed, so deleting earlier comments does not break it.
pub(crate) fn pull_request_comment_link(
    owner: &str,
    repo: &str,
    pull_request: &db::models::PullRequest,
    comment: &db::models::PullRequestComment,
) -> String {
    format!(
        "/{owner}/{repo}/pulls/{}?comment={id}#comment-{id}",
        pull_request.number,
        id = comment.id
    )
}

/// The page and anchor of a comment where it is now.
pub(crate) async fn saved_comment_link(
    pool: &db::DbPool,
    base: &str,
    comment: &db::models::PullRequestComment,
) -> String {
    use db::pull_requests::reviews::{self, DiscussionItem, DiscussionRef};
    let page = discussion_page(
        reviews::count_discussion_items_through(
            pool,
            comment.pull_request_id,
            DiscussionRef::of_comment(comment),
        )
        .await,
    );
    let anchor = format!("comment-{}", comment.id);
    let link = discussion_link(base, page, &anchor);
    if comment.review_id.is_none() {
        return link;
    }
    let offset = async {
        let items = reviews::list_discussion_items(
            pool,
            comment.pull_request_id,
            COMMENTS_PER_PAGE as i64,
            ((page - 1) * COMMENTS_PER_PAGE) as i64,
        )
        .await?;
        let ids: Vec<i64> = items
            .iter()
            .filter_map(|item| match item {
                DiscussionItem::Review(entry) => Some(entry.review.id),
                _ => None,
            })
            .collect();
        let (before, bytes) = reviews::review_comment_position(pool, &ids, comment.id).await?;
        Ok::<_, anyhow::Error>(
            if before >= reviews::MAX_REVIEW_COMMENTS_PAGE as i64
                || bytes > reviews::MAX_REVIEW_COMMENT_PAGE_BYTES as i64
            {
                before
            } else {
                0
            },
        )
    }
    .await;
    match offset {
        Ok(offset) if offset > 0 => format!("{base}?page={page}&review_comments={offset}#{anchor}"),
        Ok(_) => link,
        Err(error) => {
            tracing::warn!(comment_id = comment.id, %error, "saved review comment position unavailable");
            link
        }
    }
}

/// A review comment is publishable only while its submitted review still exists.
/// A concurrent discard deletes the review and must never publish the private text.
async fn in_pending_review(
    pool: &db::DbPool,
    comment: &db::models::PullRequestComment,
) -> Result<bool, OperationError> {
    let Some(review_id) = comment.review_id else {
        return Ok(false);
    };
    let review = db::pull_requests::reviews::get_pull_request_review_by_id(pool, review_id)
        .await
        .map_err(database_error)?;
    Ok(review.is_none_or(|review| review.state == "pending"))
}

/// Whether `commit` is one of this pull request's own commits: reachable from its head and not
/// from the base its diff starts at.
async fn is_pull_request_commit(
    cluster: Option<&NodeContext>,
    repository: &db::models::Repository,
    pull_request: &db::models::PullRequest,
    commit: &str,
) -> Result<bool, OperationError> {
    let store = repo_store(repository, cluster);
    let unavailable = |error: codeyard_git::error::RepoError| {
        if error.kind == codeyard_git::error::ErrorKind::Unavailable {
            Err(OperationError::Unavailable {
                code: "git_unavailable",
                message: "The pull request commits are temporarily unavailable".to_owned(),
            })
        } else {
            // A commit this repository does not have is not one of the pull request's.
            Ok(false)
        }
    };
    match store.is_ancestor(commit, &pull_request.head_sha).await {
        Ok(true) => {}
        Ok(false) => return Ok(false),
        Err(error) => return unavailable(error),
    }
    match store.is_ancestor(commit, pull_request.diff_base()).await {
        Ok(in_base) => Ok(!in_base),
        Err(error) => unavailable(error),
    }
}

/// Sends the webhook for a saved comment. A comment on a file is a review comment. A conversation
/// comment is sent as an issue comment, which is how GitHub sends those on pull requests.
pub(super) async fn emit_comment_webhook(
    pool: &db::DbPool,
    repository: &db::models::Repository,
    pull_request: &db::models::PullRequest,
    comment: &db::models::PullRequestComment,
    action: &'static str,
    sender_id: i64,
) {
    let on_file = comment.path.is_some();
    // A pending review hides its comments, with or without a path, until it is submitted.
    if in_pending_review(pool, comment).await.unwrap_or(true) {
        return;
    }
    let event = if on_file {
        events::PULL_REQUEST_REVIEW_COMMENT
    } else {
        events::ISSUE_COMMENT
    };
    // The conversation link names the comment; the pull request page finds its page.
    let link = if on_file {
        String::new()
    } else if action == "deleted" {
        format!("#comment-{}", comment.id)
    } else {
        format!("?comment={0}#comment-{0}", comment.id)
    };
    let (comment_for_hook, pull_for_hook, repository_for_hook) =
        (comment.clone(), pull_request.clone(), repository.clone());
    let (pull_author_id, comment_author_id) = (pull_request.author_id, comment.author_id);
    webhooks::emit(
        pool,
        repository,
        event,
        vec![pull_author_id, comment_author_id, sender_id],
        move |owner, people| {
            let (pull_author, comment_author, sender) = (
                people.get(pull_author_id),
                people.get(comment_author_id),
                people.get(sender_id),
            );
            let scope = Scope {
                repo: &repository_for_hook,
                owner,
                sender: &sender,
            };
            if on_file {
                payloads::pull_request_review_comment(
                    action,
                    &comment_for_hook,
                    &pull_for_hook,
                    &pull_author,
                    &comment_author,
                    scope,
                )
            } else {
                payloads::pull_request_conversation_comment(
                    action,
                    &comment_for_hook,
                    &pull_for_hook,
                    &pull_author,
                    &comment_author,
                    &link,
                    scope,
                )
            }
        },
    )
    .await;
}

/// The file must be one this pull request changes, and the line must be a line of its diff on the
/// side named, or on either side when none is. The diff is the one the diff page shows, up to
/// `head`: the pull request head, or the earlier commit of it that the comment names.
#[expect(
    clippy::too_many_arguments,
    reason = "An anchor is a revision, a file, a line, an optional range start and a patch cache."
)]
async fn check_diff_anchor(
    cluster: Option<&NodeContext>,
    repository: &db::models::Repository,
    pull_request: &db::models::PullRequest,
    head: &str,
    path: &str,
    line: Option<i32>,
    side: Option<&str>,
    start: Option<(i32, &str)>,
    cache: &mut DiffCache,
) -> Result<Option<&'static str>, OperationError> {
    let refusal = |message: &str| OperationError::Invalid {
        code: "validation_error",
        message: message.to_owned(),
    };
    // A path with a NUL byte names no file in any diff, and git cannot take it as an argument.
    if path.contains('\0') {
        return Err(refusal("path must be a file this pull request changes"));
    }
    // Only the selected file's patch is read, so another file's size cannot block this comment.
    // It starts where the diff page starts, at a retargeted stack layer's fork point.
    let key = (
        pull_request.diff_base().to_owned(),
        head.to_owned(),
        path.to_owned(),
    );
    if !cache.contains_key(&key) {
        let diff = repo_store(repository, cluster)
            .diff_file(&key.0, &key.1, path)
            .await
            .map_err(|error| {
                if error.is_response_too_large() {
                    OperationError::TooLarge {
                        code: "response_too_large",
                        message: "The diff of this file is too large to comment on.".to_owned(),
                    }
                } else {
                    OperationError::Unavailable {
                        code: "git_unavailable",
                        message: "The pull request diff is temporarily unavailable".to_owned(),
                    }
                }
            })?;
        let bytes = |diff: &codeyard_git::DiffInfo| {
            diff.files
                .iter()
                .map(|file| {
                    file.patch.len()
                        + file.filename.len()
                        + file.previous_filename.as_ref().map_or(0, String::len)
                        + 512
                })
                .sum::<usize>()
        };
        let held = cache.values().map(|diff| bytes(diff)).sum::<usize>();
        if held.saturating_add(bytes(&diff)) > 16 * 1024 * 1024 {
            return Err(OperationError::TooLarge {
                code: "response_too_large",
                message: "The review's selected patches exceed the validation budget.".into(),
            });
        }
        cache.insert(key.clone(), std::sync::Arc::new(diff));
    }
    let diff = &cache[&key];
    let file = diff
        .files
        .iter()
        .find(|file| file.filename == path)
        .ok_or_else(|| refusal("path must be a file this pull request changes"))?;
    let Some(line) = line else {
        return Ok(None);
    };
    let number =
        u32::try_from(line).map_err(|_| refusal("line must be a line of the pull request diff"))?;
    let rows = codeyard_webui::components::patch_rows(&file.patch);
    let on_left = rows.iter().any(|row| row.old == Some(number));
    let on_right = rows.iter().any(|row| row.new == Some(number));
    let found = match side {
        Some("LEFT") => on_left,
        Some(_) => on_right,
        None => on_left || on_right,
    };
    if found {
        if let Some((start_line, start_side)) = start {
            let start_number =
                u32::try_from(start_line).map_err(|_| refusal("start_line must be positive"))?;
            let coordinate = |row: &codeyard_webui::components::PatchRow<'_>, side: &str| {
                if side == "LEFT" {
                    row.old
                } else {
                    row.new
                }
            };
            let first = rows
                .iter()
                .position(|row| coordinate(row, start_side) == Some(start_number))
                .ok_or_else(|| refusal("start_line is not in the diff on start_side"))?;
            let end_side = side.unwrap_or(if on_right { "RIGHT" } else { "LEFT" });
            let last = rows
                .iter()
                .position(|row| coordinate(row, end_side) == Some(number))
                .ok_or_else(|| refusal("line is not in the diff"))?;
            if first >= last
                || rows[first..=last]
                    .iter()
                    .any(|row| row.header || row.class.contains("patch-hunk"))
            {
                return Err(refusal(
                    "The comment range must run forward within one diff hunk",
                ));
            }
        }
        // Without a side the comment is stored on the side that has the line, so it is shown
        // under that line.
        Ok(match side {
            Some(_) => None,
            None => Some(if on_right { "RIGHT" } else { "LEFT" }),
        })
    } else {
        Err(refusal(
            "line must be a line of the pull request diff on that side",
        ))
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "The database, git, broadcast and actor handles plus the target and request are each needed."
)]
pub async fn create_pull_request_comment(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
    req: CreatePullRequestCommentRequest,
) -> Result<db::models::PullRequestComment, OperationError> {
    let user = super::actor(pool, actor_id).await?;

    let repository = db::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;

    if req.repository_id.is_some_and(|id| id != repository.id) {
        return Err(OperationError::Conflict {
            code: "revision_conflict",
            message:
                "The repository changed after this comment was started. Reload before commenting."
                    .into(),
        });
    }
    super::require_read(pool, &user, &repository).await?;

    let pull_request = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;

    let prepared = prepare_comment(
        pool,
        cluster,
        &repository,
        &pull_request,
        req,
        &mut DiffCache::new(),
    )
    .await?;
    save_comment(
        pool,
        broadcaster,
        &user,
        &repository,
        &pull_request,
        (owner, repo),
        prepared,
    )
    .await
}

pub(super) async fn prepare_comment(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    repository: &db::models::Repository,
    pull_request: &db::models::PullRequest,
    mut req: CreatePullRequestCommentRequest,
    cache: &mut DiffCache,
) -> Result<PreparedComment, OperationError> {
    if req.repository_id.is_some_and(|id| id != repository.id) {
        return Err(OperationError::Conflict {
            code: "revision_conflict",
            message:
                "The repository changed after this comment was started. Reload before commenting."
                    .into(),
        });
    }
    if let Err(message) = crate::backend::issues::validate_body(&req.body, true) {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: message.to_owned(),
        });
    }

    if req
        .path
        .as_deref()
        .is_some_and(|path| path.trim().is_empty())
    {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "path cannot be empty".to_owned(),
        });
    }

    if let Some(line) = req.line {
        if line <= 0 {
            return Err(OperationError::Invalid {
                code: "validation_error",
                message: "line must be a positive integer".to_owned(),
            });
        }
    }

    let mut path = req.path.clone();
    let mut line = req.line;
    let mut side = req.side.as_deref().map(|value| value.trim().to_uppercase());
    let mut start_line = req.start_line;
    let mut start_side = req
        .start_side
        .as_deref()
        .map(|value| value.trim().to_uppercase());
    let mut commit_sha = req
        .commit_sha
        .as_deref()
        .map(str::trim)
        .filter(|sha| !sha.is_empty())
        .map(str::to_ascii_lowercase)
        .or_else(|| req.path.as_ref().map(|_| pull_request.head_sha.clone()));

    let mut reply_root = None;
    // The comment replied to, as its id and author.
    let mut reply_parent = None;
    if let Some(parent_id) = req.in_reply_to_comment_id {
        let parent_comment =
            db::pull_requests::reviews::get_pull_request_comment_by_id(pool, parent_id)
                .await
                .map_err(database_error)?
                .ok_or_else(|| OperationError::Missing {
                    code: "not_found",
                    message: "Parent comment not found".to_owned(),
                })?;

        if parent_comment.pull_request_id != pull_request.id {
            return Err(OperationError::Invalid {
                code: "validation_error",
                message: "Parent comment belongs to a different pull request".to_owned(),
            });
        }

        // A reply to a reply joins the thread of its root.
        reply_root = Some(thread_root(pool, &parent_comment).await?);
        reply_parent = Some((parent_comment.id, parent_comment.author_id));
        // A reply sits where its thread sits, whatever position the caller sent.
        path = parent_comment.path.clone();
        line = parent_comment.line;
        side = parent_comment
            .side
            .as_deref()
            .map(|value| value.trim().to_uppercase());
        start_line = parent_comment.start_line;
        start_side = parent_comment.start_side;
        commit_sha = parent_comment.commit_sha;
    }

    if path
        .as_ref()
        .is_some_and(|path| path.len() > db::pull_requests::reviews::MAX_COMMENT_PATH_BYTES)
    {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "path must be at most 4096 bytes".to_owned(),
        });
    }

    if line.is_some() && path.is_none() {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "path is required when line is provided".to_owned(),
        });
    }

    if let Some(side_value) = side.as_deref() {
        if side_value != "LEFT" && side_value != "RIGHT" {
            return Err(OperationError::Invalid {
                code: "validation_error",
                message: "side must be LEFT or RIGHT".to_owned(),
            });
        }

        if path.is_none() || line.is_none() {
            return Err(OperationError::Invalid {
                code: "validation_error",
                message: "path and line are required when side is provided".to_owned(),
            });
        }
    }

    if start_line.is_some() != start_side.is_some()
        || start_line.is_some_and(|line| line <= 0)
        || start_side
            .as_deref()
            .is_some_and(|side| !matches!(side, "LEFT" | "RIGHT"))
        || (start_line.is_some() && (path.is_none() || line.is_none()))
    {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message:
                "A range requires positive start_line and line, path, and LEFT or RIGHT start_side"
                    .into(),
        });
    }

    if let Some(ref sha) = commit_sha {
        if !codeyard_git::is_full_commit_id(sha) {
            return Err(OperationError::Invalid {
                code: "validation_error",
                message: "commit_sha must be a 40-character hex SHA-1".to_owned(),
            });
        }
    }

    // A new conversation must sit on the diff. A reply takes its thread root's place, so it is
    // not checked again. The lines are checked against the commit the comment is stored with:
    // the head this pull request was loaded with, or an earlier commit of the pull request that
    // the caller names, as GitHub allows. Any other commit is refused.
    if req.in_reply_to_comment_id.is_none() {
        if let Some(path) = path.as_deref() {
            if req
                .base_sha
                .as_deref()
                .is_some_and(|base| !base.eq_ignore_ascii_case(&pull_request.base_sha))
            {
                return Err(OperationError::Conflict {
                    code: "revision_conflict",
                    message: "The pull request base changed after this comment was started. Reload the diff before commenting.".to_owned(),
                });
            }
            let anchor = commit_sha.as_deref().unwrap_or(&pull_request.head_sha);
            if !anchor.eq_ignore_ascii_case(&pull_request.head_sha)
                && !is_pull_request_commit(cluster, repository, pull_request, anchor).await?
            {
                return Err(OperationError::Conflict {
                    code: "revision_conflict",
                    message: "The pull request head changed after this comment was started. Reload the diff before commenting.".to_owned(),
                });
            }
            let inferred = check_diff_anchor(
                cluster,
                repository,
                pull_request,
                anchor,
                path,
                line,
                side.as_deref(),
                start_line.zip(start_side.as_deref()),
                cache,
            )
            .await?;
            if let Some(inferred) = inferred {
                side = Some(inferred.to_owned());
            }
        }
    }

    // A new thread on a file can wait in the author's pending review. Its notices and mentions
    // go out when the review is submitted, so nothing is sent for it here.
    let pending = req.pending && path.is_some() && req.in_reply_to_comment_id.is_none();
    if pending && pull_request.state != "open" {
        return Err(OperationError::Invalid {
            code: "invalid_state",
            message: "Pull request is not open".to_owned(),
        });
    }
    req.path = path;
    req.line = line;
    req.side = side;
    req.start_line = start_line;
    req.start_side = start_side;
    req.commit_sha = commit_sha;
    req.in_reply_to_comment_id = reply_root;
    req.pending = pending;
    Ok(PreparedComment {
        request: req,
        reply_parent,
    })
}

async fn save_comment(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    user: &db::models::User,
    repository: &db::models::Repository,
    pull_request: &db::models::PullRequest,
    location: (String, String),
    prepared: PreparedComment,
) -> Result<db::models::PullRequestComment, OperationError> {
    let (owner, repo) = location;
    let req = &prepared.request;
    let pending = req.pending;
    let reply_parent = prepared.reply_parent;
    let input = prepared.input(pull_request, user.id);
    let (comment, deferred) = db::pull_requests::reviews::insert_comment(
        pool,
        input,
        pending || req.review_id.is_some(),
        req.review_id,
    )
    .await
    .map_err(super::write_error)?;
    // Submission owns publication for every comment that was pending when inserted.
    if deferred {
        return Ok(comment);
    }

    // Public comments notify each real recipient once; pending reviews returned above.
    let link = pull_request_comment_link(&owner, &repo, pull_request, &comment);
    let title = format!(
        "{} commented on pull request #{}",
        user.username, pull_request.number
    );
    let notice = NotificationRequest {
        user_id: pull_request.author_id,
        actor_id: Some(user.id),
        repo_id: Some(repository.id),
        kind: "pull_request_comment",
        title: &title,
        body: Some(&req.body),
        link: Some(&link),
        metadata: json!({"repo_owner":owner,"repo_name":repo,"pull_number":pull_request.number,"subject":pull_request.title,"comment_id":comment.id,"in_reply_to_comment_id":comment.in_reply_to_comment_id}),
    };
    let mut notified = Vec::new();
    if pull_request.author_id != user.id
        && create_and_broadcast_notification(pool, broadcaster, notice.clone())
            .await
            .is_ok()
    {
        notified.push(pull_request.author_id);
    }
    // A direct reply can notify an unsubscribed parent author, but never an ignoring one.
    if let Some((parent_id, parent_author)) =
        reply_parent.filter(|(_, author)| *author != user.id && !notified.contains(author))
    {
        let title = format!("{} replied to your pull request comment", user.username);
        if create_and_broadcast_notification(pool,broadcaster,NotificationRequest{
            user_id:parent_author,actor_id:Some(user.id),repo_id:Some(repository.id),kind:"pull_request_comment_reply",title:&title,body:Some(&req.body),link:Some(&link),
            metadata:json!({"repo_owner":owner,"repo_name":repo,"pull_number":pull_request.number,"subject":pull_request.title,"comment_id":comment.id,"parent_comment_id":parent_id}),
        }).await.is_ok() {notified.push(parent_author);}
    }
    // A mention comes before the subscriber notice, so a mentioned subscriber hears it as one.
    let mentioned = notify_mentions(
        pool,
        broadcaster,
        MentionSource {
            actor: user,
            repository,
            owner: &owner,
            repo: &repo,
            number: pull_request.number,
            subject: &pull_request.title,
            link: &link,
            comment_id: Some(
                crate::backend::notifications::mentions::MentionComment::PullRequest(comment.id),
            ),
            old_text: None,
            text: &req.body,
            notified: &notified,
        },
    )
    .await;
    notified.extend(mentioned);
    crate::backend::notifications::create::notify_subscribers(pool, broadcaster, notice, &notified)
        .await;

    emit_comment_webhook(pool, repository, pull_request, &comment, "created", user.id).await;
    Ok(comment)
}

/// Marks a review conversation resolved, or open again. Only a root comment on a file is a
/// conversation. The pull request's author and anyone with write access may do this.
pub async fn set_comment_resolved(
    pool: &db::DbPool,
    actor_id: i64,
    owner: &str,
    repo: &str,
    number: i32,
    comment_id: i64,
    resolved: bool,
) -> Result<db::models::PullRequestComment, OperationError> {
    let user = super::actor(pool, actor_id).await?;
    let repository = db::repositories::records::get_repository(pool, owner, repo)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;
    super::require_read(pool, &user, &repository).await?;
    let pull_request = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;
    if repository.is_archived || user.must_change_password {
        return Err(OperationError::Denied {
            code: "write_denied",
            message: "Archived repositories and password-change-required accounts cannot write"
                .to_owned(),
        });
    }
    if pull_request.author_id != user.id
        && !crate::backend::repositories::access::can_write(pool, &user, &repository)
            .await
            .map_err(database_error)?
    {
        return Err(OperationError::Denied {
            code: "comment_forbidden",
            message: "Only the pull request author or a collaborator with write access can resolve conversations".to_owned(),
        });
    }
    let comment = db::pull_requests::reviews::get_pull_request_comment_by_id(pool, comment_id)
        .await
        .map_err(database_error)?
        .filter(|comment| comment.pull_request_id == pull_request.id)
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Comment not found".to_owned(),
        })?;
    if comment.in_reply_to_comment_id.is_some() || comment.path.is_none() {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Only the first comment of a conversation on a file can be resolved"
                .to_owned(),
        });
    }
    // The database checks the resolver's authority in the same transaction as the update. The
    // checks above only give the specific early refusals.
    db::pull_requests::reviews::set_pull_request_comment_resolved(
        pool,
        pull_request.id,
        comment.id,
        resolved,
        user.id,
    )
    .await
    .map_err(super::write_error)?
    .ok_or_else(|| OperationError::Missing {
        code: "not_found",
        message: "Comment not found".to_owned(),
    })
}

/// The pull request and the conversation comment on it, when the user may read them. A comment
/// filed in a review is not a conversation comment and is not found here.
async fn conversation_comment_on_pull(
    pool: &db::DbPool,
    user: &db::models::User,
    owner: &str,
    repo: &str,
    number: i32,
    comment_id: i64,
) -> Result<
    (
        db::models::Repository,
        db::models::PullRequest,
        db::models::PullRequestComment,
    ),
    OperationError,
> {
    let repository = db::repositories::records::get_repository(pool, owner, repo)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;
    super::require_read(pool, user, &repository).await?;
    let pull_request = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;
    let comment = db::pull_requests::reviews::get_pull_request_comment_by_id(pool, comment_id)
        .await
        .map_err(database_error)?
        .filter(|comment| comment.pull_request_id == pull_request.id)
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Comment not found".to_owned(),
        })?;
    if in_pending_review(pool, &comment).await? && comment.author_id != user.id {
        return Err(OperationError::Missing {
            code: "not_found",
            message: "Comment not found".into(),
        });
    }
    Ok((repository, pull_request, comment))
}

/// The author or a repository writer may edit a visible comment in a live repository.
pub async fn edit_pull_request_comment(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    target: CommentTarget,
    body: String,
    expected_body: Option<&str>,
) -> Result<db::models::PullRequestComment, OperationError> {
    let CommentTarget {
        owner,
        repo,
        number,
        comment_id,
    } = target;
    let user = super::actor(pool, actor_id).await?;
    let (repository, pull_request, previous) =
        conversation_comment_on_pull(pool, &user, &owner, &repo, number, comment_id).await?;
    if repository.is_archived {
        return Err(archived_refusal());
    }
    if previous.author_id != user.id
        && !crate::backend::repositories::access::can_write(pool, &user, &repository)
            .await
            .map_err(database_error)?
    {
        return Err(OperationError::Denied {
            code: "comment_forbidden",
            message: "Only the author or a repository writer can edit this comment.".to_owned(),
        });
    }
    if expected_body.is_some_and(|body| body != previous.body) {
        return Err(OperationError::Conflict {
            code: "stale_comment",
            message: "The comment changed. Reload before saving.".into(),
        });
    }
    if let Err(message) = crate::backend::issues::validate_body(&body, true) {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: message.to_owned(),
        });
    }
    let comment = db::pull_requests::reviews::edit_pull_request_comment(
        pool,
        user.id,
        repository.id,
        pull_request.id,
        comment_id,
        &body,
        &previous.body,
    )
    .await
    .map_err(super::write_error)?
    .ok_or_else(|| match expected_body {
        Some(_) => OperationError::Conflict {
            code: "conflict",
            message: "This comment changed; reload before saving.".to_owned(),
        },
        None => OperationError::Denied {
            code: "comment_forbidden",
            message: "You cannot edit this comment.".to_owned(),
        },
    })?;

    // Only the names the edit adds notify; the others were notified when first written. The
    // author's notice for the comment is not a mention, so the author is not excluded here.
    // The edit is saved: a failed visibility read keeps the mentions private instead of
    // reporting the saved edit as failed.
    if !in_pending_review(pool, &comment).await.unwrap_or(true) && mentions_anyone(&body) {
        let link = pull_request_comment_link(&owner, &repo, &pull_request, &comment);
        notify_mentions(
            pool,
            broadcaster,
            MentionSource {
                actor: &user,
                repository: &repository,
                owner: &owner,
                repo: &repo,
                number: pull_request.number,
                subject: &pull_request.title,
                link: &link,
                comment_id: Some(
                    crate::backend::notifications::mentions::MentionComment::PullRequest(
                        comment.id,
                    ),
                ),
                old_text: Some(&previous.body),
                text: &body,
                notified: &[],
            },
        )
        .await;
    }
    emit_comment_webhook(
        pool,
        &repository,
        &pull_request,
        &comment,
        "edited",
        user.id,
    )
    .await;
    Ok(comment)
}

/// Deletes a conversation comment and the notifications about it. The author may delete their own
/// comment and a repository writer may delete anyone's visible comment.
pub async fn delete_pull_request_comment(
    pool: &db::DbPool,
    actor_id: i64,
    target: CommentTarget,
) -> Result<(), OperationError> {
    let CommentTarget {
        owner,
        repo,
        number,
        comment_id,
    } = target;
    let user = super::actor(pool, actor_id).await?;
    let (repository, pull_request, previous) =
        conversation_comment_on_pull(pool, &user, &owner, &repo, number, comment_id).await?;
    if repository.is_archived {
        return Err(archived_refusal());
    }
    let removed = db::pull_requests::reviews::delete_pull_request_comment(
        pool,
        user.id,
        repository.id,
        pull_request.id,
        comment_id,
    )
    .await
    .map_err(super::write_error)?;
    if removed {
        emit_comment_webhook(
            pool,
            &repository,
            &pull_request,
            &previous,
            "deleted",
            user.id,
        )
        .await;
        Ok(())
    } else {
        Err(OperationError::Denied {
            code: "comment_forbidden",
            message: "You cannot delete this comment.".to_owned(),
        })
    }
}

fn archived_refusal() -> OperationError {
    OperationError::Denied {
        code: "repository_archived",
        message: "This repository is archived and read-only.".to_owned(),
    }
}
