use codeyard_repos_core::ci::runs::CiRuns;
use codeyard_repos_core::repositories::pull_requests::database_error;
use codeyard_repos_core::repositories::pull_requests::workflows::queue_pull_request_ci;
use codeyard_repos_core::repositories::pull_requests::OperationError;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_notifications::create::create_and_broadcast_notification;
use codeyard_notifications::create::NotificationRequest;
use codeyard_notifications::mentions::notify_mentions;
use codeyard_notifications::mentions::MentionSource;
use codeyard_notifications::watchers::notify_watchers;
use codeyard_notifications::watchers::WatchEvent;
use codeyard_notifications::watchers::WatchedItem;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

#[derive(Deserialize)]
#[serde(try_from = "PullRequestSourceInput")]
pub enum PullRequestSource {
    New { title: String, body: Option<String> },
    Issue { issue: i32 },
}

#[derive(Deserialize)]
struct PullRequestSourceInput {
    title: Option<String>,
    body: Option<String>,
    issue: Option<i32>,
}

impl TryFrom<PullRequestSourceInput> for PullRequestSource {
    type Error = &'static str;
    fn try_from(input: PullRequestSourceInput) -> Result<Self, Self::Error> {
        match input.issue {
            Some(issue) if issue > 0 => Ok(Self::Issue { issue }),
            Some(_) => Err("issue must be a positive discussion number"),
            None => Ok(Self::New {
                title: input.title.ok_or("title or issue is required")?,
                body: input.body,
            }),
        }
    }
}

#[derive(Deserialize)]
pub struct CreatePullRequestRequest {
    /// Only server-owned proposals may suppress automatic execution; clients cannot set this.
    #[serde(skip)]
    pub suppress_ci: bool,
    pub head_repo_id: Option<i64>,
    #[serde(flatten)]
    pub source: PullRequestSource,
    pub head_branch: String,
    pub base_branch: String,
    #[serde(default)]
    pub is_draft: bool,
    pub maintainer_can_modify: Option<bool>,
}

#[derive(Clone, Deserialize)]
pub struct UpdatePullRequestRequest {
    pub expected_state: String,
    pub expected_updated: String,
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<String>,
    /// A new base branch for an open pull request.
    pub base: Option<String>,
    pub maintainer_can_modify: Option<bool>,
}

fn validate_maintainer_edit(
    actor: i64,
    base: i64,
    head: &codeyard_db_core::models::Repository,
    enabled: bool,
) -> Result<(), OperationError> {
    if enabled
        && !codeyard_db_repos::pull_requests::records::can_grant_maintainer_edit(actor, base, head)
    {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Only the personal fork owner can allow maintainer edits".into(),
        });
    }
    Ok(())
}

/// Branch names are stored in VARCHAR(255) columns, which count characters.
fn check_branch_name(branch: &str) -> Result<(), OperationError> {
    if codeyard_git::branch_ref(branch).is_none() || branch.chars().count() > 255 {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Branch names must be valid Git branch names of at most 255 characters."
                .to_owned(),
        });
    }
    Ok(())
}

/// The row already committed, even when publication is definitively refused.
fn head_publish_error(number: i32, error: RepoError) -> OperationError {
    tracing::warn!(number, %error, "pull request head was not published");
    OperationError::Opened { number }
}

/// Repair a committed creation without borrowing its historical author's credentials.
pub(crate) async fn repair_open_pull(
    pool: &codeyard_db_core::DbPool,
    ci: &CiRuns,
    repository: &codeyard_db_core::models::Repository,
    head_repository: &codeyard_db_core::models::Repository,
    credential: codeyard_repos_core::store_types::PullHeadActor,
    existing: &codeyard_db_core::models::PullRequest,
    suppress_ci: bool,
) -> Result<(), OperationError> {
    let actor_id = credential.user;
    let number = existing.number;
    // Exact CAS repairs an absent initial ref, never a later publication.
    repo_store(repository, ci.cluster.as_ref())
        .fetch_pull_head(
            pool,
            repository.id,
            head_repository.id,
            credential,
            &repo_store(head_repository, ci.cluster.as_ref()),
            number,
            &existing.head_sha,
            None,
        )
        .await
        .map_err(|error| head_publish_error(number, error))?;
    codeyard_repos_core::repositories::pull_requests::branches::refresh_open_pull_request(
        pool,
        repository,
        ci,
        actor_id,
        existing.id,
        (!suppress_ci).then_some(codeyard_db_core::models::AccessLevel::Owner),
    )
    .await
    .map_err(|_| OperationError::Opened { number })?;
    let current =
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, existing.id)
            .await
            .map_err(|_| OperationError::Opened { number })?
            .ok_or(OperationError::Opened { number })?;
    if !suppress_ci {
        let _ = queue_pull_request_ci(
            pool,
            ci,
            repository,
            &current,
            actor_id,
            false,
            codeyard_db_core::models::AccessLevel::Owner,
        )
        .await;
    }
    codeyard_repos_core::repositories::pull_requests::code_owners::request_code_owner_reviews(
        pool, ci, repository, &current, actor_id,
    )
    .await;
    Ok(())
}

pub async fn create_pull_request(
    pool: &codeyard_db_core::DbPool,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    actor_id: impl Into<codeyard_repos_core::store_types::PullHeadActor>,
    owner: String,
    repo: String,
    req: CreatePullRequestRequest,
) -> Result<codeyard_db_core::models::PullRequest, OperationError> {
    let credential = actor_id.into();
    let actor_id = credential.user;
    let user = codeyard_repos_core::repositories::pull_requests::actor(pool, actor_id).await?;
    let repository = codeyard_db_repos::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;

    codeyard_repos_core::repositories::pull_requests::require_read(pool, &user, &repository).await?;
    let mut converted_retry = None;
    let (source_issue, title, body) = match req.source {
        PullRequestSource::Issue { issue } => {
            if !codeyard_identity::repository_access::can_write(pool, &user, &repository)
                .await
                .map_err(database_error)?
            {
                return Err(OperationError::Denied {
                    code: "write_denied",
                    message: "Converting an issue requires repository write access".into(),
                });
            }
            if let Some(source) = codeyard_db_repos::issues::get_issue(pool, repository.id, issue)
                .await
                .map_err(database_error)?
            {
                (Some(issue), source.title, source.body)
            } else {
                let converted = codeyard_db_repos::pull_requests::records::get_converted_pull_request(
                    pool,
                    repository.id,
                    issue,
                )
                .await
                .map_err(database_error)?
                .ok_or_else(|| {
                    codeyard_repos_core::repositories::pull_requests::write_error(
                        codeyard_db_repos::pull_requests::records::IssueConversionRefusal::Missing.into(),
                    )
                })?;
                let content = (Some(issue), converted.title.clone(), converted.body.clone());
                converted_retry = Some(converted);
                content
            }
        }
        PullRequestSource::New { title, body } => (None, title, body.unwrap_or_default()),
    };
    let checked = codeyard_model::issue_input::validate_title(&title)
        .and_then(|_| codeyard_model::issue_input::validate_body(&body, false));
    if let Err(message) = checked {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: message.to_owned(),
        });
    }
    for branch in [&req.head_branch, &req.base_branch] {
        check_branch_name(branch)?;
    }
    let head_repository = codeyard_repos_core::repositories::pull_requests::branches::head_repository(
        pool,
        &repository,
        req.head_repo_id.unwrap_or(repository.id),
    )
    .await
    .map_err(|error| match error {
        codeyard_repos_core::repositories::pull_requests::branches::HeadError::Unavailable => {
            OperationError::Unavailable {
                code: "database_error",
                message: "Head repository lookup is unavailable".into(),
            }
        }
        codeyard_repos_core::repositories::pull_requests::branches::HeadError::Missing
        | codeyard_repos_core::repositories::pull_requests::branches::HeadError::OutsideFork => {
            OperationError::Invalid {
                code: "invalid_head",
                message: "Head must be this repository or one of its forks".into(),
            }
        }
    })?;
    codeyard_repos_core::repositories::pull_requests::require_read(pool, &user, &head_repository).await?;
    validate_maintainer_edit(
        user.id,
        repository.id,
        &head_repository,
        req.maintainer_can_modify.unwrap_or(false),
    )?;
    if head_repository.id == repository.id && req.head_branch == req.base_branch {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Head and base branch cannot be the same".to_owned(),
        });
    }

    let refs: HashMap<String, String> = repo_store(&repository, ci.cluster.as_ref())
        .refs_from_primary()
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "git_unavailable",
            message: "Repository storage is unavailable".into(),
        })?;
    let head_ref = format!("refs/heads/{}", req.head_branch);
    let head_refs = if head_repository.id == repository.id {
        refs.clone()
    } else {
        repo_store(&head_repository, ci.cluster.as_ref())
            .refs_from_primary()
            .await
            .map_err(|_| OperationError::Unavailable {
                code: "git_unavailable",
                message: "Head repository storage is unavailable".into(),
            })?
    };
    let head_sha = head_refs
        .get(&head_ref)
        .cloned()
        .ok_or_else(|| OperationError::Invalid {
            code: "git_error",
            message: format!("Head branch not found: {}", req.head_branch),
        })?;
    let base_ref = format!("refs/heads/{}", req.base_branch);
    let base_sha = refs
        .get(&base_ref)
        .cloned()
        .ok_or_else(|| OperationError::Invalid {
            code: "git_error",
            message: format!("Base branch not found: {}", req.base_branch),
        })?;

    if let Some(existing) = converted_retry {
        if existing.state != "open"
            || existing.head_repo_id != Some(head_repository.id)
            || existing.head_branch != req.head_branch
            || existing.base_branch != req.base_branch
        {
            return Err(codeyard_repos_core::repositories::pull_requests::write_error(
                codeyard_db_repos::pull_requests::records::IssueConversionRefusal::Missing.into(),
            ));
        }
        repair_open_pull(
            pool,
            ci,
            &repository,
            &head_repository,
            credential,
            &existing,
            req.suppress_ci,
        )
        .await?;
        return Err(OperationError::Conflict {
            code: "pull_request_exists",
            message: format!(
                "This issue is already converted to pull request #{}",
                existing.number
            ),
        });
    }

    // A head with nothing outside the base, or with no history in common with it, cannot carry
    // a pull request. Checked before the insert, so a converted issue stays open. The head is
    // only counted, without listing its commits.
    let base_store = repo_store(&repository, ci.cluster.as_ref());
    let head_store = if head_repository.id == repository.id {
        base_store.clone()
    } else {
        repo_store(&head_repository, ci.cluster.as_ref())
    };
    let merged = base_store
        .comparison_from(&head_store, &base_sha, &head_sha, 0, 0)
        .await
        .map(|comparison| comparison.ahead_by == 0)
        .map_err(|error| {
            if error.kind == ErrorKind::NotFound {
                OperationError::Invalid {
                    code: "validation_error",
                    message: format!(
                        "No common ancestor between {} and {}",
                        req.base_branch, req.head_branch
                    ),
                }
            } else {
                OperationError::Unavailable {
                    code: "git_unavailable",
                    message: "Repository storage is unavailable".into(),
                }
            }
        })?;
    if merged {
        return Err(OperationError::Invalid {
            code: "no_commits",
            message: format!(
                "No commits between {} and {}",
                req.base_branch, req.head_branch
            ),
        });
    }
    let created = codeyard_db_repos::pull_requests::records::create_pull_request(
        pool,
        codeyard_db_repos::pull_requests::records::NewPullRequest {
            repo_id: repository.id,
            head_repo_id: Some(head_repository.id),
            author_id: user.id,
            issue: source_issue,
            title: &title,
            body: &body,
            head_branch: &req.head_branch,
            head_sha: &head_sha,
            base_branch: &req.base_branch,
            base_sha: &base_sha,
            is_draft: req.is_draft,
            maintainer_can_modify: req.maintainer_can_modify.unwrap_or(false),
        },
    )
    .await;
    let pull_request = match created {
        Ok(pull_request) => pull_request,
        Err(error) => {
            if source_issue.is_some()
                && error.is::<codeyard_db_repos::pull_requests::records::DuplicateOpenPullRequest>()
            {
                return Err(OperationError::Conflict {
                    code: "conflict",
                    message: "A pull request is already open for these branches".into(),
                });
            }
            let number = match error
                .downcast::<codeyard_db_repos::pull_requests::records::DuplicateOpenPullRequest>(
            ) {
                Ok(duplicate) => duplicate.number,
                Err(error) => return Err(codeyard_repos_core::repositories::pull_requests::write_error(error)),
            };
            let existing = codeyard_db_repos::pull_requests::records::get_pull_request(
                pool,
                repository.id,
                number,
            )
            .await
            .map_err(database_error)?;
            let Some(existing) = existing.filter(|pull| pull.state == "open") else {
                return Err(OperationError::Conflict {
                    code: "revision_conflict",
                    message: "Pull request state changed; retry the request".to_owned(),
                });
            };
            repair_open_pull(
                pool,
                ci,
                &repository,
                &head_repository,
                credential,
                &existing,
                req.suppress_ci,
            )
            .await?;
            return Err(OperationError::Conflict {
                code: "pull_request_exists",
                message: format!(
                    "An open pull request already exists for these branches as #{number}"
                ),
            });
        }
    };

    {
        repo_store(&repository, ci.cluster.as_ref())
            .fetch_pull_head(
                pool,
                repository.id,
                head_repository.id,
                credential,
                &repo_store(&head_repository, ci.cluster.as_ref()),
                pull_request.number,
                &pull_request.head_sha,
                None,
            )
            .await
            .map_err(|error| head_publish_error(pull_request.number, error))?;
    }

    codeyard_repos_core::repositories::pull_requests::branches::refresh_open_pull_request(
        pool,
        &repository,
        ci,
        user.id,
        pull_request.id,
        (!req.suppress_ci).then_some(codeyard_db_core::models::AccessLevel::Owner),
    )
    .await
    .map_err(|_| OperationError::Opened {
        number: pull_request.number,
    })?;
    let pull_request =
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, pull_request.id)
            .await
            .map_err(|_| OperationError::Opened {
                number: pull_request.number,
            })?
            .ok_or(OperationError::Opened {
                number: pull_request.number,
            })?;

    let mut direct_recipients: Vec<_> = repository.personal_owner_id().into_iter().collect();
    if !direct_recipients.contains(&pull_request.author_id) {
        direct_recipients.push(pull_request.author_id);
    }
    for recipient in direct_recipients
        .iter()
        .copied()
        .filter(|id| *id != user.id)
    {
        let title = format!(
            "{} opened pull request #{}",
            user.username, pull_request.number
        );
        let link = format!("/{}/{}/pulls/{}", owner, repo, pull_request.number);
        let _ = create_and_broadcast_notification(
            pool,
            broadcaster,
            NotificationRequest {
                user_id: recipient,
                actor_id: Some(user.id),
                repo_id: Some(repository.id),
                kind: "pull_request_opened",
                title: &title,
                body: Some(&pull_request.title),
                link: Some(&link),
                metadata: json!({
                    "repo_owner": owner,
                    "repo_name": repo,
                    "pull_number": pull_request.number,
                    "subject": pull_request.title,
                    "head_branch": pull_request.head_branch.as_str(),
                    "base_branch": pull_request.base_branch.as_str(),
                }),
            },
        )
        .await;
    }

    let link = format!("/{}/{}/pulls/{}", owner, repo, pull_request.number);
    let mut notified = notify_watchers(
        pool,
        broadcaster,
        WatchedItem {
            event: WatchEvent::Pull,
            actor: &user,
            repository: &repository,
            owner: &owner,
            repo: &repo,
            number: Some(pull_request.number),
            title: &pull_request.title,
            link: &link,
            notified: &direct_recipients,
        },
    )
    .await;
    notified.extend(&direct_recipients);
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
            comment_id: None,
            old_text: None,
            text: &pull_request.body,
            notified: &notified,
        },
    )
    .await;

    codeyard_repos_core::repositories::pull_requests::code_owners::request_code_owner_reviews(
        pool,
        ci,
        &repository,
        &pull_request,
        user.id,
    )
    .await;

    let payload = codeyard_repos_core::repositories::pull_requests::emit_pull_request_event(
        pool,
        ci.cluster.as_ref(),
        &repository,
        &pull_request,
        "opened",
        user.id,
        serde_json::Map::new(),
    )
    .await;
    if !req.suppress_ci {
        codeyard_repos_core::repositories::pull_requests::workflows::queue_pull_request_event_ci(
            pool,
            ci,
            &repository,
            &pull_request,
            user.id,
            codeyard_db_core::models::AccessLevel::Owner,
            &payload,
        )
        .await;
    }

    Ok(pull_request)
}

pub async fn update_pull_request(
    pool: &codeyard_db_core::DbPool,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    (repository_id, number): (i64, i32),
    req: UpdatePullRequestRequest,
) -> Result<codeyard_db_core::models::PullRequest, OperationError> {
    let (repository, owner) =
        codeyard_repos_core::repositories::pull_requests::repository(pool, repository_id).await?;
    let repo = repository.name.clone();

    let user = codeyard_repos_core::repositories::pull_requests::actor(pool, actor_id).await?;
    codeyard_repos_core::repositories::pull_requests::require_read(pool, &user, &repository).await?;
    let existing =
        codeyard_db_repos::pull_requests::records::get_pull_request(pool, repository.id, number)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Pull request not found".to_owned(),
            })?;

    let writer = codeyard_identity::repository_access::can_write(pool, &user, &repository)
        .await
        .map_err(database_error)?;
    let head = if req.maintainer_can_modify.is_some() {
        match existing.head_repo_id {
            Some(id) => codeyard_db_repos::repositories::records::get_repository_by_id(pool, id)
                .await
                .map_err(database_error)?,
            None => None,
        }
    } else {
        None
    };
    let head_owner = head
        .as_ref()
        .is_some_and(|head| head.is_personal_owner(user.id));
    let ordinary = req.title.is_some() || req.body.is_some() || req.state.is_some();
    // Without write access the author keeps ordinary edits, close and reopen, and the
    // maintainer policy; the personal fork owner keeps the policy; only writers move the base.
    // The database repeats this check when it saves.
    let contributor_edit = req.base.is_none()
        && (existing.author_id == user.id
            || (head_owner && !ordinary && req.maintainer_can_modify.is_some()));
    if repository.is_archived || !(writer || contributor_edit) {
        return Err(OperationError::Denied {
            code: "write_denied",
            message: "Pull request edit denied".into(),
        });
    }
    if req.maintainer_can_modify == Some(true) {
        let head = head.as_ref().ok_or_else(|| OperationError::Invalid {
            code: "invalid_head",
            message: "Head repository is unavailable".into(),
        })?;
        validate_maintainer_edit(user.id, repository.id, head, true)?;
    }
    let requested_state = match req.state.as_deref() {
        Some("open") | Some("closed") => req.state.as_deref(),
        Some("merged") => {
            return Err(OperationError::Invalid {
                code: "validation_error",
                message: "Use merge endpoint to merge pull requests".to_owned(),
            })
        }
        Some(_) => {
            return Err(OperationError::Invalid {
                code: "validation_error",
                message: "state must be one of: open, closed".to_owned(),
            })
        }
        None => None,
    };

    if existing.state == "merged" && requested_state.is_some() {
        return Err(OperationError::Invalid {
            code: "invalid_state",
            message: "Merged pull requests cannot be reopened or closed".to_owned(),
        });
    }

    let expected_updated = req
        .expected_updated
        .parse::<chrono::DateTime<chrono::Utc>>()
        .map_err(|_| OperationError::Invalid {
            code: "validation_error",
            message: "expected_updated must be the timestamp returned when reading this entry"
                .to_owned(),
        })?;
    if req
        .title
        .as_ref()
        .is_some_and(|title| codeyard_model::issue_input::validate_title(title).is_err())
        || req
            .body
            .as_ref()
            .is_some_and(|body| codeyard_model::issue_input::validate_body(body, false).is_err())
        || !matches!(req.expected_state.as_str(), "open" | "closed" | "merged")
    {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Invalid title, body, or expected state".to_owned(),
        });
    }
    if req.base.is_some() && existing.stack_id.is_some() {
        return Err(OperationError::Conflict {
            code: "conflict",
            message: "Unstack the pull request before changing its base branch.".to_owned(),
        });
    }
    let reopening = requested_state == Some("open") && existing.state == "closed";
    let reopened_base = if reopening {
        if let Some(stack) = existing.stack_id {
            let members = codeyard_db_repos::pull_requests::records::list_stack_members(
                pool,
                repository.id,
                stack,
            )
            .await
            .map_err(codeyard_repos_core::repositories::pull_requests::write_error)?;
            codeyard_db_repos::pull_requests::records::reopened_stack_base(&existing, &members)
        } else {
            None
        }
    } else {
        None
    };
    let base_commit = match req.base.as_deref().or(reopened_base.as_deref()) {
        Some(branch) => Some((branch.to_owned(), {
            // The base is checked as the reopened pull request will have it.
            let mut target = existing.clone();
            if reopening {
                target.state = "open".into();
            }
            base_branch_commit(ci, &repository, &target, branch).await?
        })),
        None => None,
    };
    let base_changed = base_commit.is_some();
    let new_base = base_commit.as_ref().map(|(branch, sha)| {
        codeyard_db_repos::pull_requests::records::NewBase {
            branch: branch.as_str(),
            sha: sha.as_str(),
            automatic_reopen: req.base.is_none() && reopened_base.is_some(),
        }
    });
    let pull_request = codeyard_db_repos::pull_requests::records::update_pull_request(
        pool,
        existing.id,
        codeyard_db_repos::discussions::DiscussionEdit {
            actor_id: user.id,
            expected_state: &req.expected_state,
            expected_updated,
            title: req.title.as_deref(),
            body: req.body.as_deref(),
            state: requested_state,
        },
        codeyard_db_repos::pull_requests::records::PullRequestUpdate {
            base: new_base,
            maintainer_can_modify: req.maintainer_can_modify,
        },
    ).await.map_err(|error| match error.downcast::<codeyard_db_repos::pull_requests::records::DuplicateOpenPullRequest>() {
        Ok(duplicate) => OperationError::Conflict {
            code: "conflict",
            message: format!("Pull request #{} is already open for these branches", duplicate.number),
        },
        Err(error) => {
            if error.is::<codeyard_db_repos::pull_caps::PullCapReached>() {return codeyard_repos_core::repositories::pull_requests::write_error(error);}
            tracing::error!(pull_request_id = existing.id, "conditional pull request update failed");
            OperationError::Unavailable { code: "database_error", message: "Unable to save this entry; your submission was not confirmed".to_owned() }
        }
    })?.ok_or_else(|| OperationError::Conflict { code: "conflict", message: "This entry, merge operation, or your permission changed; reload and review your preserved submission".to_owned() })?;

    if let Some(body) = req.body.as_deref() {
        let link = format!("/{owner}/{repo}/pulls/{number}");
        notify_mentions(
            pool,
            broadcaster,
            MentionSource {
                actor: &user,
                repository: &repository,
                owner: &owner,
                repo: &repo,
                number,
                subject: &pull_request.title,
                link: &link,
                comment_id: None,
                old_text: Some(&existing.body),
                text: body,
                notified: &[],
            },
        )
        .await;
    }

    // Branches may have moved while the pull request was closed. Catch up now that
    // it is open again, and answer with what it is after that.
    let pull_request = if requested_state == Some("open") || base_changed {
        codeyard_repos_core::repositories::pull_requests::branches::refresh_open_pull_request(
            pool,
            &repository,
            ci,
            user.id,
            pull_request.id,
            Some(codeyard_db_core::models::AccessLevel::Owner),
        )
        .await
        .map_err(|_| OperationError::Updated { number })?;
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, pull_request.id)
            .await
            .map_err(|_| OperationError::Updated { number })?
            .ok_or(OperationError::Updated { number })?
    } else {
        pull_request
    };
    if base_changed {
        codeyard_repos_core::repositories::pull_requests::code_owners::request_code_owner_reviews(
            pool,
            ci,
            &repository,
            &pull_request,
            user.id,
        )
        .await;
        // A new base can make workflows eligible that the old one did not, so queue them for it.
        // The `edited` event below does not start default pull request workflows.
        codeyard_repos_core::repositories::pull_requests::workflows::queue_moved_pull_requests_ci(
            pool,
            ci,
            &repository,
            std::slice::from_ref(&pull_request),
            user.id,
        )
        .await;
    }

    let mut extra = serde_json::Map::new();
    let action = if pull_request.state == existing.state {
        extra.insert(
            "changes".into(),
            serde_json::Value::Object(edited_changes(&existing, &pull_request)),
        );
        "edited"
    } else if pull_request.state == "closed" {
        "closed"
    } else {
        "reopened"
    };
    let payload = codeyard_repos_core::repositories::pull_requests::emit_pull_request_event(
        pool,
        ci.cluster.as_ref(),
        &repository,
        &pull_request,
        action,
        user.id,
        extra,
    )
    .await;
    codeyard_repos_core::repositories::pull_requests::workflows::queue_pull_request_event_ci(
        pool,
        ci,
        &repository,
        &pull_request,
        user.id,
        codeyard_db_core::models::AccessLevel::Owner,
        &payload,
    )
    .await;

    Ok(pull_request)
}

/// The `changes` document of an `edited` event: the earlier value of each field that changed.
fn edited_changes(
    before: &codeyard_db_core::models::PullRequest,
    after: &codeyard_db_core::models::PullRequest,
) -> serde_json::Map<String, serde_json::Value> {
    let mut changes = serde_json::Map::new();
    if before.title != after.title {
        changes.insert("title".into(), json!({"from": before.title}));
    }
    if before.body != after.body {
        changes.insert("body".into(), json!({"from": before.body}));
    }
    if before.base_branch != after.base_branch {
        changes.insert(
            "base".into(),
            json!({"ref": {"from": before.base_branch}, "sha": {"from": before.base_sha}}),
        );
    }
    changes
}

/// Saves an edit from the GitHub API, which carries no form revision: the caller passes the
/// state and time it just read. A concurrent change, such as gh adding labels while it saves the
/// title, moves that time, so read the pull request again and retry instead of refusing.
pub async fn update_pull_request_latest(
    pool: &codeyard_db_core::DbPool,
    ci: &CiRuns,
    broadcaster: &NotificationBroadcaster,
    actor_id: i64,
    target: (i64, i32),
    mut req: UpdatePullRequestRequest,
) -> Result<codeyard_db_core::models::PullRequest, OperationError> {
    for _ in 0..3 {
        let refused =
            match update_pull_request(pool, ci, broadcaster, actor_id, target, req.clone()).await {
                Err(error @ OperationError::Conflict { .. }) => error,
                result => return result,
            };
        let latest =
            codeyard_db_repos::pull_requests::records::get_pull_request(pool, target.0, target.1)
                .await
                .map_err(database_error)?;
        match latest {
            Some(latest)
                if latest.state != req.expected_state
                    || latest.updated_at.to_rfc3339() != req.expected_updated =>
            {
                req.expected_state = latest.state;
                req.expected_updated = latest.updated_at.to_rfc3339();
            }
            _ => return Err(refused),
        }
    }
    update_pull_request(pool, ci, broadcaster, actor_id, target, req).await
}

/// The commit that a new base branch of an open pull request points at. Only an open pull request
/// can change its base, and only to a branch of this repository other than its own head branch.
async fn base_branch_commit(
    ci: &CiRuns,
    repository: &codeyard_db_core::models::Repository,
    pull_request: &codeyard_db_core::models::PullRequest,
    branch: &str,
) -> Result<String, OperationError> {
    if pull_request.state != "open" {
        return Err(OperationError::Invalid {
            code: "invalid_state",
            message: "Merged and closed pull requests cannot change their base branch".to_owned(),
        });
    }
    check_branch_name(branch)?;
    if pull_request.head_repo_id == Some(repository.id) && pull_request.head_branch == branch {
        return Err(OperationError::Invalid {
            code: "validation_error",
            message: "Head and base branch cannot be the same".to_owned(),
        });
    }
    let refs = repo_store(repository, ci.cluster.as_ref())
        .refs_from_primary()
        .await
        .map_err(|_| OperationError::Unavailable {
            code: "git_unavailable",
            message: "Repository storage is unavailable".into(),
        })?;
    refs.get(&format!("refs/heads/{branch}"))
        .cloned()
        .ok_or_else(|| OperationError::Invalid {
            code: "validation_error",
            message: format!("Base branch `{branch}` does not exist."),
        })
}

/// The repository, the actor and the pull request a stack change works on. Only people who can
/// write may change stacks, and a missing pull request is refused before anything changes.
async fn stack_change_target(
    pool: &codeyard_db_core::DbPool,
    actor_id: i64,
    owner: &str,
    repo: &str,
    number: i32,
) -> Result<
    (
        codeyard_db_core::models::Repository,
        codeyard_db_core::models::User,
        codeyard_db_core::models::PullRequest,
    ),
    OperationError,
> {
    let repository = codeyard_db_repos::repositories::records::get_repository(pool, owner, repo)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;
    let user = codeyard_repos_core::repositories::pull_requests::actor(pool, actor_id).await?;
    codeyard_repos_core::repositories::pull_requests::require_write(pool, &user, &repository).await?;
    let existing =
        codeyard_db_repos::pull_requests::records::get_pull_request(pool, repository.id, number)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Pull request not found".to_owned(),
            })?;
    Ok((repository, user, existing))
}

/// Takes the unmerged pull requests of the stack this pull request is in out of it. Each keeps its
/// base branch, and merged pull requests keep the stack. A pull request in no stack is returned
/// unchanged. The database reads the stack when it writes, not from an earlier read.
pub async fn unstack_pull_request(
    pool: &codeyard_db_core::DbPool,
    ci: &CiRuns,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
) -> Result<codeyard_db_core::models::PullRequest, OperationError> {
    let (repository, user, existing) =
        stack_change_target(pool, actor_id, &owner, &repo, number).await?;
    let unstacked = codeyard_db_repos::pull_requests::records::unstack_pull_requests(
        pool,
        user.id,
        existing.id,
    )
    .await
    .map_err(|_| OperationError::Unavailable {
        code: "database_error",
        message: "Unable to save this change; it was not confirmed".to_owned(),
    })?;
    let Some(members) = unstacked else {
        return Err(OperationError::Denied {
            code: "forbidden",
            message: "Only people with write access can unstack pull requests".to_owned(),
        });
    };
    // Each open layer goes back to its own base, so workflows for that base may now apply. They are
    // read again, since a join or a reopen may have changed them since the stack was read.
    let mut open = Vec::new();
    for member in members.iter().filter(|pull| pull.state == "open") {
        if let Ok(Some(pull)) =
            codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, member.id).await
        {
            open.push(pull);
        }
    }
    codeyard_repos_core::repositories::pull_requests::workflows::queue_moved_pull_requests_ci(
        pool,
        ci,
        &repository,
        &open,
        user.id,
    )
    .await;
    for pull in &open {
        codeyard_repos_core::repositories::pull_requests::code_owners::request_code_owner_reviews(
            pool,
            ci,
            &repository,
            pull,
            user.id,
        )
        .await;
    }
    codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, existing.id)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })
}

/// Adds a pull request to the stack of the open pull request its base branch names, or starts a
/// stack with that pull request. Nothing else adds a pull request to a stack.
pub async fn unstack_named_stack(
    pool: &codeyard_db_core::DbPool,
    ci: &CiRuns,
    actor_id: i64,
    namespace: (String, String),
    stack_id: i64,
) -> Result<usize, OperationError> {
    let user = codeyard_repos_core::repositories::pull_requests::actor(pool, actor_id).await?;
    let repository =
        codeyard_db_repos::repositories::records::get_repository(pool, &namespace.0, &namespace.1)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Repository not found".into(),
            })?;
    codeyard_repos_core::repositories::pull_requests::require_write(pool, &user, &repository).await?;
    let rows = codeyard_db_repos::pull_requests::records::unstack_named_stack(
        pool,
        user.id,
        repository.id,
        stack_id,
    )
    .await
    .map_err(database_error)?
    .ok_or_else(|| OperationError::Denied {
        code: "forbidden",
        message: "Stack write denied".into(),
    })?;
    let count = rows.len();
    let mut open = Vec::new();
    for member in rows.iter().filter(|p| p.state == "open") {
        if let Ok(Some(pull)) =
            codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, member.id).await
        {
            open.push(pull);
        }
    }
    codeyard_repos_core::repositories::pull_requests::workflows::queue_moved_pull_requests_ci(
        pool,
        ci,
        &repository,
        &open,
        user.id,
    )
    .await;
    for pull in &open {
        codeyard_repos_core::repositories::pull_requests::code_owners::request_code_owner_reviews(
            pool,
            ci,
            &repository,
            pull,
            user.id,
        )
        .await;
    }
    Ok(count)
}

pub async fn join_stack_batch(
    pool: &codeyard_db_core::DbPool,
    ci: &CiRuns,
    actor_id: i64,
    namespace: (String, String),
    root: i64,
    ids: &[i64],
    creating: bool,
) -> Result<(), OperationError> {
    let user = codeyard_repos_core::repositories::pull_requests::actor(pool, actor_id).await?;
    let repository =
        codeyard_db_repos::repositories::records::get_repository(pool, &namespace.0, &namespace.1)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Repository not found".into(),
            })?;
    codeyard_repos_core::repositories::pull_requests::require_write(pool, &user, &repository).await?;
    let result = codeyard_db_repos::pull_requests::records::join_pull_request_stack_batch(
        pool,
        user.id,
        repository.id,
        root,
        ids,
        creating,
    )
    .await
    .map_err(database_error)?;
    match result {
        codeyard_db_repos::pull_requests::records::StackJoin::Joined(id) if id == root => {}
        codeyard_db_repos::pull_requests::records::StackJoin::Refused(reason) => {
            return Err(join_refusal(reason))
        }
        _ => {
            return Err(OperationError::Conflict {
                code: "conflict",
                message: "Stack changed while it was being saved".into(),
            })
        }
    }
    for id in ids.iter().skip(if creating { 1 } else { 0 }) {
        let pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, *id)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Unavailable {
                code: "database_error",
                message: "Saved stack member could not be loaded".into(),
            })?;
        codeyard_repos_core::repositories::pull_requests::code_owners::request_code_owner_reviews(
            pool,
            ci,
            &repository,
            &pull,
            user.id,
        )
        .await;
        codeyard_repos_core::repositories::pull_requests::workflows::queue_moved_pull_requests_ci(
            pool,
            ci,
            &repository,
            std::slice::from_ref(&pull),
            user.id,
        )
        .await;
    }
    Ok(())
}

pub async fn join_stack(
    pool: &codeyard_db_core::DbPool,
    ci: &CiRuns,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
) -> Result<codeyard_db_core::models::PullRequest, OperationError> {
    let (repository, user, existing) =
        stack_change_target(pool, actor_id, &owner, &repo, number).await?;
    let joined = codeyard_db_repos::pull_requests::records::join_pull_request_stack(
        pool,
        user.id,
        existing.id,
    )
    .await
    .map_err(|_| OperationError::Unavailable {
        code: "database_error",
        message: "Unable to save this change; it was not confirmed".to_owned(),
    })?;
    if let codeyard_db_repos::pull_requests::records::StackJoin::Refused(reason) = joined {
        return Err(join_refusal(reason));
    }
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, existing.id)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        })?;
    codeyard_repos_core::repositories::pull_requests::code_owners::request_code_owner_reviews(
        pool,
        ci,
        &repository,
        &pull,
        user.id,
    )
    .await;
    // The layer now merges into its stack's trunk, so workflows for that trunk may now apply.
    codeyard_repos_core::repositories::pull_requests::workflows::queue_moved_pull_requests_ci(
        pool,
        ci,
        &repository,
        std::slice::from_ref(&pull),
        user.id,
    )
    .await;
    Ok(pull)
}

/// The answer for a pull request that cannot join a stack.
fn join_refusal(reason: codeyard_db_repos::pull_requests::records::StackRefusal) -> OperationError {
    use codeyard_db_repos::pull_requests::records::StackRefusal;
    use codeyard_db_repos::pull_requests::records::MAX_STACK_LAYERS;
    match reason {
        StackRefusal::Missing => OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".to_owned(),
        },
        StackRefusal::Denied => OperationError::Denied {
            code: "forbidden",
            message: "Only people with write access can change stacks".to_owned(),
        },
        StackRefusal::NotOpen => OperationError::Conflict {
            code: "invalid_state",
            message: "Only open pull requests can join a stack.".to_owned(),
        },
        StackRefusal::ForkHead => OperationError::Invalid {
            code: "validation_error",
            message: "Only a pull request from this repository's own branch can join a stack."
                .to_owned(),
        },
        StackRefusal::NoParent => OperationError::Conflict {
            code: "stack_join_refused",
            message: "No open pull request in this repository has this base branch as its head."
                .to_owned(),
        },
        StackRefusal::ParentNotTop => OperationError::Conflict {
            code: "stack_join_refused",
            message: "Something is already stacked on that pull request.".to_owned(),
        },
        StackRefusal::AlreadyStacked => OperationError::Conflict {
            code: "stack_join_refused",
            message: "Unstack this pull request before adding it to another stack.".to_owned(),
        },
        StackRefusal::Cycle => OperationError::Conflict {
            code: "stack_join_refused",
            message: "These pull requests would depend on each other, so they cannot be stacked."
                .to_owned(),
        },
        StackRefusal::TooLarge => OperationError::Conflict {
            code: "stack_join_refused",
            message: format!("A stack can have at most {MAX_STACK_LAYERS} pull requests."),
        },
        StackRefusal::Changed => OperationError::Conflict {
            code: "stack_join_refused",
            message: "The stack changed while this pull request was joining. Reload and try again."
                .to_owned(),
        },
    }
}

pub async fn mark_pull_request_ready(
    pool: &codeyard_db_core::DbPool,
    ci: &CiRuns,
    actor_id: i64,
    owner: String,
    repo: String,
    number: i32,
    expected: Option<&str>,
) -> Result<codeyard_db_core::models::PullRequest, OperationError> {
    set_pull_request_draft(pool, ci, actor_id, (owner, repo, number), expected, false).await
}

pub async fn set_pull_request_draft(
    pool: &codeyard_db_core::DbPool,
    ci: &CiRuns,
    actor_id: i64,
    (owner, repo, number): (String, String, i32),
    expected: Option<&str>,
    is_draft: bool,
) -> Result<codeyard_db_core::models::PullRequest, OperationError> {
    let user = codeyard_repos_core::repositories::pull_requests::actor(pool, actor_id).await?;
    let repository = codeyard_db_repos::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".to_owned(),
        })?;
    codeyard_repos_core::repositories::pull_requests::require_read(pool, &user, &repository).await?;
    let existing =
        codeyard_db_repos::pull_requests::records::get_pull_request(pool, repository.id, number)
            .await
            .map_err(database_error)?
            .ok_or_else(|| OperationError::Missing {
                code: "not_found",
                message: "Pull request not found".to_owned(),
            })?;

    let outcome = codeyard_db_repos::pull_requests::records::set_pull_request_draft(
        pool,
        existing.id,
        user.id,
        expected,
        is_draft,
    )
    .await
    .map_err(|error| {
        if error.is::<codeyard_db_repos::pull_caps::PullCapReached>() {
            return codeyard_repos_core::repositories::pull_requests::write_error(error);
        }
        tracing::error!(
            pull_request_id = existing.id,
            "marking a pull request ready failed"
        );
        OperationError::Unavailable {
            code: "database_error",
            message: "Unable to save this change; it was not confirmed".to_owned(),
        }
    })?;
    let pull_request = match outcome {
        codeyard_db_repos::pull_requests::records::ReadyOutcome::Changed => {
            return Err(OperationError::Conflict { code: "stale_revision", message: "This pull request changed after the form was opened. Reload before trying again.".to_owned() })
        }
        codeyard_db_repos::pull_requests::records::ReadyOutcome::Ready(pull_request) => *pull_request,
        codeyard_db_repos::pull_requests::records::ReadyOutcome::Missing => {
            return Err(OperationError::Missing { code: "not_found", message: "Pull request not found".to_owned() })
        }
        codeyard_db_repos::pull_requests::records::ReadyOutcome::Denied => {
            return Err(OperationError::Denied { code: "forbidden", message: "Only the author and people with write access can change draft status".to_owned() })
        }
        codeyard_db_repos::pull_requests::records::ReadyOutcome::NotOpen => {
            return Err(OperationError::Invalid { code: "invalid_state", message: "Pull request is not open".to_owned() })
        }
        codeyard_db_repos::pull_requests::records::ReadyOutcome::NotDraft => {
            return Err(OperationError::Conflict { code: "invalid_state", message: "Pull request already has this draft status".to_owned() })
        }
    };

    if is_draft {
        let payload = codeyard_repos_core::repositories::pull_requests::emit_pull_request_event(
            pool,
            ci.cluster.as_ref(),
            &repository,
            &pull_request,
            "converted_to_draft",
            user.id,
            serde_json::Map::new(),
        )
        .await;
        codeyard_repos_core::repositories::pull_requests::workflows::queue_pull_request_event_ci(
            pool,
            ci,
            &repository,
            &pull_request,
            user.id,
            codeyard_db_core::models::AccessLevel::Owner,
            &payload,
        )
        .await;
        return Ok(pull_request);
    }
    // Branches may have moved while it was a draft. Catch up, then queue its workflows the way
    // a new pull request does. The pull request is ready already, so a retry would be refused:
    // the event and its workflows go out even when the catch-up fails, and the failure is
    // reported after them.
    let refreshed = codeyard_repos_core::repositories::pull_requests::branches::refresh_open_pull_request(
        pool,
        &repository,
        ci,
        user.id,
        pull_request.id,
        Some(codeyard_db_core::models::AccessLevel::Owner),
    )
    .await;
    if let Err(error) = &refreshed {
        tracing::warn!(pull_request_id = pull_request.id, %error, "ready pull request revisions were not refreshed");
    }
    let current =
        codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, pull_request.id)
            .await
            .ok()
            .flatten();
    let complete = refreshed.is_ok() && current.is_some();
    let pull_request = current.unwrap_or(pull_request);
    codeyard_repos_core::repositories::pull_requests::code_owners::request_code_owner_reviews(
        pool,
        ci,
        &repository,
        &pull_request,
        user.id,
    )
    .await;

    let payload = codeyard_repos_core::repositories::pull_requests::emit_pull_request_event(
        pool,
        ci.cluster.as_ref(),
        &repository,
        &pull_request,
        "ready_for_review",
        user.id,
        serde_json::Map::new(),
    )
    .await;
    codeyard_repos_core::repositories::pull_requests::workflows::queue_pull_request_event_ci(
        pool,
        ci,
        &repository,
        &pull_request,
        user.id,
        codeyard_db_core::models::AccessLevel::Owner,
        &payload,
    )
    .await;
    if !complete {
        return Err(OperationError::Updated { number });
    }
    Ok(pull_request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_head_publication_failures_report_the_already_opened_pull_request() {
        let refused = RepoError::new(
            ErrorKind::Invalid,
            "A repository holds at most 1 pull request heads",
        );
        assert!(matches!(
            head_publish_error(3, refused),
            OperationError::Opened { number: 3 }
        ));
        let failed = RepoError::new(ErrorKind::Unavailable, "Git did not finish");
        assert!(matches!(
            head_publish_error(3, failed),
            OperationError::Opened { number: 3 }
        ));
    }
}
