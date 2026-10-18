use super::OperationError;
use crate::backend::cluster::NodeContext;
use crate::backend::db;
use crate::backend::issues::comment_link;
use crate::backend::notifications::create::create_and_broadcast_notification;
use crate::backend::notifications::create::NotificationRequest;
use crate::backend::notifications::mentions::{mentions_anyone, notify_mentions, MentionSource};
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::repositories::store::repo_store;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
#[derive(Deserialize)]
pub struct CreatePullRequestCommentRequest {
    pub body: String,
    pub path: Option<String>,
    pub line: Option<i32>,
    pub side: Option<String>,
    pub commit_sha: Option<String>,
    pub in_reply_to_comment_id: Option<i64>,
    /// A new thread on a file goes into the author's pending review instead of being published.
    #[serde(default)]
    pub pending: bool,
}

fn is_valid_sha1_hex(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

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
            .map_err(|_| OperationError::Unavailable {
                code: "database_error",
                message: "Collaboration storage is temporarily unavailable".to_owned(),
            })?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Parent comment not found".to_owned(),
            })?;
        root_id = comment.id;
        next = comment.in_reply_to_comment_id;
    }
    Ok(root_id)
}

/// The discussion page and anchor of a saved pull request comment. The position is read only when
/// a notification names this link.
async fn pull_request_comment_link(
    pool: &db::DbPool,
    owner: &str,
    repo: &str,
    pull_request: &db::models::PullRequest,
    comment: &db::models::PullRequestComment,
) -> String {
    let base = format!("/{}/{}/pulls/{}", owner, repo, pull_request.number);
    let position = db::pull_requests::reviews::count_discussion_items_through(
        pool,
        pull_request.id,
        db::pull_requests::reviews::DiscussionRef::of_comment(comment),
    )
    .await;
    comment_link(&base, comment.id, position)
}

/// Whether a comment is filed in a review that is still pending. Nothing is sent for it until the
/// review is submitted, and a discard removes it without a trace.
async fn in_pending_review(
    pool: &db::DbPool,
    comment: &db::models::PullRequestComment,
) -> Result<bool, OperationError> {
    let Some(review_id) = comment.review_id else {
        return Ok(false);
    };
    let review = db::pull_requests::reviews::get_pull_request_review_by_id(pool, review_id)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?;
    Ok(review.is_some_and(|review| review.state == "pending"))
}

/// The file must be one this pull request changes, and the line must be a line of its diff on the
/// side named, or on either side when none is. The diff is the one the diff page shows.
async fn check_diff_anchor(
    cluster: Option<&NodeContext>,
    repository: &db::models::Repository,
    pull_request: &db::models::PullRequest,
    path: &str,
    line: Option<i32>,
    side: Option<&str>,
) -> Result<(), OperationError> {
    let refusal = |message: &str| OperationError::Invalid {
        code: "validation_error",
        message: message.to_owned(),
    };
    // A path with a NUL byte names no file in any diff, and git cannot take it as an argument.
    if path.contains('\0') {
        return Err(refusal("path must be a file this pull request changes"));
    }
    // Only the selected file's patch is read, so another file's size cannot block this comment.
    let diff = repo_store(repository, cluster)
        .diff_file(&pull_request.base_sha, &pull_request.head_sha, path)
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
    let file = diff
        .files
        .iter()
        .find(|file| file.filename == path)
        .ok_or_else(|| refusal("path must be a file this pull request changes"))?;
    let Some(line) = line else {
        return Ok(());
    };
    let number =
        u32::try_from(line).map_err(|_| refusal("line must be a line of the pull request diff"))?;
    let found = crate::webui::components::patch_rows(&file.patch)
        .iter()
        .any(|row| match side {
            Some("LEFT") => row.old == Some(number),
            Some(_) => row.new == Some(number),
            None => row.old == Some(number) || row.new == Some(number),
        });
    if found {
        Ok(())
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
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;

    super::require_read(pool, &super::actor(pool, actor_id).await?, &repository).await?;

    let pull_request = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "database_error",
            message: "Collaboration storage is temporarily unavailable".to_owned(),
        })?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;

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
    let mut commit_sha = req
        .commit_sha
        .as_deref()
        .map(str::trim)
        .filter(|sha| !sha.is_empty())
        .map(str::to_string)
        .or_else(|| req.path.as_ref().map(|_| pull_request.head_sha.clone()));

    let mut reply_root = None;
    // The comment replied to, as its id and author.
    let mut reply_parent = None;
    if let Some(parent_id) = req.in_reply_to_comment_id {
        let parent_comment =
            db::pull_requests::reviews::get_pull_request_comment_by_id(pool, parent_id)
                .await
                .map_err(|_| OperationError::Unavailable {
                    code: "database_error",
                    message: "Collaboration storage is temporarily unavailable".to_owned(),
                })?
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
        if path.is_none() {
            path = parent_comment.path.clone();
        }
        if line.is_none() {
            line = parent_comment.line;
        }
        if side.is_none() {
            side = parent_comment
                .side
                .as_deref()
                .map(|value| value.trim().to_uppercase());
        }
        if commit_sha.is_none() {
            commit_sha = parent_comment.commit_sha;
        }
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

    if let Some(ref sha) = commit_sha {
        if !is_valid_sha1_hex(sha) {
            return Err(OperationError::Invalid {
                code: "validation_error",
                message: "commit_sha must be a 40-character hex SHA-1".to_owned(),
            });
        }
    }

    // A new conversation must sit on the diff. A reply takes its thread root's place, so it is
    // not checked again. The lines are checked against the head this pull request was loaded
    // with, so a commit the caller names must be that head: otherwise the stored commit and the
    // checked lines would describe different revisions.
    if req.in_reply_to_comment_id.is_none() {
        if let Some(path) = path.as_deref() {
            if let Some(submitted) = req.commit_sha.as_deref() {
                if !submitted.eq_ignore_ascii_case(&pull_request.head_sha) {
                    return Err(OperationError::Conflict {
                        code: "revision_conflict",
                        message: "The pull request head changed after this comment was started. Reload the diff before commenting.".to_owned(),
                    });
                }
            }
            check_diff_anchor(
                cluster,
                &repository,
                &pull_request,
                path,
                line,
                side.as_deref(),
            )
            .await?;
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
    let input = db::pull_requests::reviews::NewPullRequestComment {
        pr_id: pull_request.id,
        author_id: user.id,
        body: &req.body,
        path: path.as_deref(),
        line,
        side: side.as_deref(),
        commit_sha: commit_sha.as_deref(),
        in_reply_to_comment_id: reply_root,
    };
    let stored = if pending {
        db::pull_requests::reviews::add_pending_pull_request_comment(pool, input).await
    } else {
        db::pull_requests::reviews::add_pull_request_comment(pool, input).await
    };
    let comment = stored.map_err(super::write_error)?;
    // A new thread on a file, and a reply that joins one, wait in the pending review.
    if in_pending_review(pool, &comment).await? {
        return Ok(comment);
    }

    // The users this comment already notifies. A mention of one of them adds nothing.
    let mut notified = vec![pull_request.author_id];
    notified.extend(reply_parent.map(|(_, author)| author));
    let notifies_author = pull_request.author_id != user.id;
    // The author of the comment that was replied to is notified, even when the reply is
    // stored under the thread root.
    let reply_to =
        reply_parent.filter(|(_, author)| *author != user.id && *author != pull_request.author_id);
    if notifies_author || reply_to.is_some() || mentions_anyone(&req.body) {
        // One link serves every notification of this comment.
        let link = pull_request_comment_link(pool, &owner, &repo, &pull_request, &comment).await;
        if notifies_author {
            let title = format!(
                "{} commented on pull request #{}",
                user.username, pull_request.number
            );
            let _ = create_and_broadcast_notification(
                pool,
                broadcaster,
                NotificationRequest {
                    user_id: pull_request.author_id,
                    actor_id: Some(user.id),
                    repo_id: Some(repository.id),
                    kind: "pull_request_comment",
                    title: &title,
                    body: Some(&req.body),
                    link: Some(&link),
                    metadata: json!({
                        "repo_owner": owner,
                        "repo_name": repo,
                        "pull_number": pull_request.number,
                        "subject": pull_request.title,
                        "comment_id": comment.id,
                        "in_reply_to_comment_id": comment.in_reply_to_comment_id,
                    }),
                },
            )
            .await;
        }
        if let Some((parent_id, parent_author)) = reply_to {
            let title = format!("{} replied to your pull request comment", user.username);
            let _ = create_and_broadcast_notification(
                pool,
                broadcaster,
                NotificationRequest {
                    user_id: parent_author,
                    actor_id: Some(user.id),
                    repo_id: Some(repository.id),
                    kind: "pull_request_comment_reply",
                    title: &title,
                    body: Some(&req.body),
                    link: Some(&link),
                    metadata: json!({
                        "repo_owner": owner,
                        "repo_name": repo,
                        "pull_number": pull_request.number,
                        "subject": pull_request.title,
                        "comment_id": comment.id,
                        "parent_comment_id": parent_id,
                    }),
                },
            )
            .await;
        }
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
                old_text: None,
                text: &req.body,
                notified: &notified,
            },
        )
        .await;
    }

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
    let unavailable = |_| OperationError::Unavailable {
        code: "database_error",
        message: "Collaboration storage is temporarily unavailable".to_owned(),
    };
    let repository = db::repositories::records::get_repository(pool, owner, repo)
        .await
        .map_err(unavailable)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;
    super::require_read(pool, &user, &repository).await?;
    let pull_request = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(unavailable)?
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
            .map_err(unavailable)?
    {
        return Err(OperationError::Denied {
            code: "forbidden",
            message: "Only the pull request author or a collaborator with write access can resolve conversations".to_owned(),
        });
    }
    let comment = db::pull_requests::reviews::get_pull_request_comment_by_id(pool, comment_id)
        .await
        .map_err(unavailable)?
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
