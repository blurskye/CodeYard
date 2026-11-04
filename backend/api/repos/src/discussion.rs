//! What issue and pull request pages share: loading the item and its repository, the detail
//! view with comments and reactions, and the text shown for failed operations.
use crate::milestones::milestone_choice_view;
use crate::milestones::milestone_view;
use crate::refs::references::resolve_references_or_none;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use chrono::DateTime;
use chrono::Utc;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::error::ApiFailure;
use codeyard_api_http::shared::repos::access::check_repo_read_access;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_api_http::shared::repos::forms::with_status;
use codeyard_repos_pulls::issues::COMMENTS_PER_PAGE;
use codeyard_repos_pulls::issues::MAX_COMMENT_PAGE;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_repos_core::store_types::allowed_merge_methods;
use codeyard_repos_core::store_types::MergeMethod;
use codeyard_db_repos::discussions::comment_revision;
use codeyard_db_repos::pull_requests::reviews::DiscussionItem;
use codeyard_webui::pages::repos::issues as view;
use codeyard_webui::pages::repos::issues::DetailDraft;
use codeyard_webui::pages::repos::issues::Kind;
use codeyard_webui::pages::repos::issues::NewDraft;
use codeyard_webui::pages::repos::pulls::MAX_LISTED_COMMITS;
use std::collections::HashMap;

pub(crate) type ResultPage<T> = Result<T, Response>;

// Handlers are registered only under `/:owner/:repo/issues` and `/:owner/:repo/pulls`,
// also behind the legacy prefix.
pub(crate) fn kind(uri: &Uri) -> Kind {
    let path = uri
        .path()
        .strip_prefix(codeyard_api_http::shared::repos::LEGACY_PREFIX)
        .unwrap_or(uri.path());
    if path.split('/').nth(3) == Some("pulls") {
        Kind::Pull
    } else {
        Kind::Issue
    }
}
pub(crate) fn unavailable(ctx: &PageContext) -> Response {
    ctx.error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Collaboration data is temporarily unavailable. Please try again.",
    )
}
pub(crate) fn safe_api_error(failure: &ApiFailure) -> (StatusCode, String) {
    let (status, Json(error)) = failure;
    let message = if status.is_server_error() {
        match error.message.as_str() {
            crate::pulls::BRANCH_MERGED_MESSAGE => "The branch was merged, but the pull request status could not be updated. Reload before retrying.",
            crate::pulls::OPENED_MESSAGE => "The pull request was opened, but its head could not be fetched. Reload before retrying.",
            crate::pulls::UPDATED_MESSAGE => crate::pulls::UPDATED_MESSAGE,
            _ => "The operation could not be completed. Its outcome may be uncertain; reload before retrying.",
        }
    } else {
        match error.error.as_str() {
            "validation_error"
            | "invalid_state"
            | "branch_protection"
            | "unresolved_conversations"
            | "ci_not_queued"
            | "stack_needs_merge_commit"
            | "stack_needs_restack"
            | "stack_broken"
            | "stack_layer_closed"
            | "stack_join_refused"
            | "stack_fork_head"
            | "not_stacked"
            | "rebase_conflict"
            | "merge_method_not_allowed"
            | "unsupported_merge"
            | "revision_conflict"
            | "response_too_large"
            | "milestone_title_taken"
            | "unassignable"
            | "too_many_assignees"
            | "repository_archived"
            | "conversation_locked"
            | "comment_forbidden" => &error.message,
            "merge_conflict" => "Git could not merge these branches. Resolve the conflict before retrying.",
            "not_found" => "The requested resource was not found or you do not have access.",
            "unauthorized" => "Please sign in again before continuing.",
            "forbidden" => "You do not have permission to perform this action.",
            "git_error" => "The selected Git branches are unavailable. Check the comparison and try again.",
            _ => "The operation was rejected. Reload the page and check your permissions and the current state.",
        }
    };
    (*status, message.to_owned())
}
pub(crate) fn api_error(ctx: &PageContext, failure: ApiFailure) -> Response {
    let (status, message) = safe_api_error(&failure);
    ctx.error(status, &message)
}
pub(crate) fn submitted_new(form: &HashMap<String, String>) -> NewDraft {
    NewDraft {
        repository_id: field(form, "repository_id"),
        template: field(form, "template"),
        template_revision: field(form, "template_revision"),
        head_repo: field(form, "head_repo"),
        title: field(form, "title"),
        body: field(form, "body"),
        head: field(form, "head"),
        base: field(form, "base"),
        draft: form.get("draft").is_some_and(|v| v == "on"),
        ..NewDraft::default()
    }
}
pub(crate) fn submitted_detail(form: &HashMap<String, String>, action: &str) -> DetailDraft {
    match action {
        "edit" => DetailDraft {
            title: Some(field(form, "title")),
            body: Some(field(form, "body")),
            preview: field(form, "intent") == "preview",
            ..Default::default()
        },
        "comment" => DetailDraft {
            comment: field(form, "body"),
            repository_id: form.get("repository_id").cloned(),
            issue_id: form.get("issue_id").cloned(),
            expected_body: field(form, "expected_body"),
            preview: field(form, "intent") == "preview",
            ..Default::default()
        },
        "label" => DetailDraft {
            label: field(form, "label"),
            ..Default::default()
        },
        _ => DetailDraft::default(),
    }
}
pub(crate) struct RepositoryData {
    pub(crate) record: codeyard_db_core::models::Repository,
    pub(crate) view: view::Repository,
}
pub(crate) async fn repository(
    ctx: &PageContext,
    owner: &str,
    name: &str,
) -> ResultPage<RepositoryData> {
    let loaded = codeyard_api_http::shared::repos::chrome::load_database(ctx, owner, name).await?;
    let record = loaded.repository;
    let access = match &ctx.user {
        Some(user) => {
            codeyard_identity::repository_access::user_access(&ctx.state.pool, user, &record)
                .await
                .map_err(|_| unavailable(ctx))?
        }
        None => codeyard_db_core::models::AccessLevel::None,
    };
    let mutable = ctx.user.is_some()
        && !record.is_archived
        && !codeyard_db_repos::repository_exports::locked(&ctx.state.pool, record.id)
            .await
            .map_err(|_| unavailable(ctx))?;
    let can_write = mutable && access.can_write();
    let can_triage = mutable && access.can_triage();
    let can_admin = access.can_admin();
    // Reuse the Code page's authorized projection, including visibility, real
    // engagement counts and actor-scoped actions. Never serialize the DB record.
    let chrome = loaded.chrome;
    Ok(RepositoryData {
        record,
        view: view::Repository {
            owner: owner.to_owned(),
            name: name.to_owned(),
            signed_in: ctx.user.is_some(),
            can_write,
            can_triage,
            can_admin,
            csrf: ctx.csrf_token.clone(),
            viewer: ctx
                .user
                .as_ref()
                .map(|user| user.username.clone())
                .unwrap_or_default(),
            chrome,
        },
    })
}

pub(crate) async fn author(ctx: &PageContext, id: i64) -> ResultPage<String> {
    Ok(
        codeyard_db_identity::users::accounts::get_user_by_id(&ctx.state.pool, id)
            .await
            .map_err(|_| unavailable(ctx))?
            .map(|user| user.username)
            .unwrap_or_else(|| "Deleted user".into()),
    )
}
pub(crate) fn label(value: codeyard_db_core::models::Label) -> view::Label {
    view::Label {
        id: value.id,
        name: value.name,
        color: value.color,
        description: value.description.unwrap_or_default(),
    }
}
pub(crate) async fn issue_reference(
    ctx: &PageContext,
    scope: (&str, &str),
    reference: &str,
) -> ResultPage<i64> {
    codeyard_repos_pulls::issues::references::resolve(
        &ctx.state.pool,
        ctx.require_user()?.id,
        scope,
        reference,
    )
    .await
    .map(|issue| issue.id)
    .map_err(|error| {
        let (status, axum::Json(reason)) = crate::issues::operation_error(error);
        ctx.error(
            if status == StatusCode::BAD_REQUEST {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                status
            },
            &reason.message,
        )
    })
}

fn can_edit_entry(ctx: &PageContext, repo: &RepositoryData, author_id: i64) -> bool {
    !repo.record.is_archived
        && ctx.user.as_ref().is_some_and(|user| {
            !user.is_banned
                && !user.must_change_password
                && (repo.view.can_write || user.id == author_id)
        })
}

pub(crate) fn issue_item(
    issue: codeyard_db_core::models::Issue,
    author: String,
    labels: Vec<codeyard_db_core::models::Label>,
) -> view::Item {
    view::Item {
        id: issue.id,
        number: issue.number,
        title: issue.title,
        body: issue.body,
        state: issue.state,
        author,
        created: issue.created_at.to_rfc3339(),
        updated: issue.updated_at.to_rfc3339(),
        labels: labels.into_iter().map(label).collect(),
        stacked: false,
        stack: None,
        stack_candidate: None,
        assignees: Vec::new(),
        milestone: None,
        comparison: None,
        comment_count: 0,
        can_edit: false,
        can_tick: false,
        tasks: Default::default(),
        commit_count: None,
        file_count: None,
        check_count: 0,
        review_state: None,
        check_state: None,
        reactions: Vec::new(),
        projects: Vec::new(),
    }
}
/// A pull request's head repository as this viewer may see it. The current name and
/// link are shown only to a viewer who can read that repository.
#[derive(Clone)]
pub(crate) enum HeadRepository {
    Readable(codeyard_db_repos::pull_requests::records::PullRequestHeadDisplay),
    /// It exists, but this viewer cannot read it.
    Unreadable,
    Deleted,
}

async fn can_view_head(
    ctx: &PageContext,
    head: &codeyard_db_core::models::Repository,
) -> ResultPage<bool> {
    codeyard_identity::repository_access::can_read(&ctx.state.pool, ctx.user.as_ref(), head)
        .await
        .map_err(|_| unavailable(ctx))
}

/// The head repository of a pull request opened from a fork, for the detail page.
async fn head_for_viewer(
    ctx: &PageContext,
    head: Option<codeyard_db_core::models::Repository>,
) -> ResultPage<HeadRepository> {
    let Some(head) = head else {
        return Ok(HeadRepository::Deleted);
    };
    if !can_view_head(ctx, &head).await? {
        return Ok(HeadRepository::Unreadable);
    }
    let owner_name =
        codeyard_db_repos::repositories::records::repository_owner_name(&ctx.state.pool, head.id)
            .await
            .map_err(|_| unavailable(ctx))?;
    Ok(HeadRepository::Readable(
        codeyard_db_repos::pull_requests::records::PullRequestHeadDisplay {
            id: head.id,
            name: head.name,
            owner_name,
        },
    ))
}

/// The head repository of a pull request in a list. The listing supplies the current
/// name; the viewer's read access decides whether it is shown.
pub(crate) async fn listed_head(
    ctx: &PageContext,
    base_id: i64,
    cache: &mut HashMap<i64, HeadRepository>,
    head: Option<codeyard_db_repos::pull_requests::records::PullRequestHeadDisplay>,
) -> ResultPage<HeadRepository> {
    let Some(head) = head else {
        return Ok(HeadRepository::Deleted);
    };
    if head.id == base_id {
        return Ok(HeadRepository::Deleted); // Same-repository heads use the base's display data.
    }
    if let Some(shown) = cache.get(&head.id) {
        return Ok(shown.clone());
    }
    let id = head.id;
    let Some(record) =
        codeyard_db_repos::repositories::records::get_repository_by_id(&ctx.state.pool, head.id)
            .await
            .map_err(|_| unavailable(ctx))?
    else {
        cache.insert(id, HeadRepository::Deleted);
        return Ok(HeadRepository::Deleted);
    };
    let shown = if can_view_head(ctx, &record).await? {
        HeadRepository::Readable(head)
    } else {
        HeadRepository::Unreadable
    };
    cache.insert(id, shown.clone());
    Ok(shown)
}

/// The assignees of each listed issue or pull request, by item id, in name order.
pub(crate) async fn listed_assignees(
    ctx: &PageContext,
    item_ids: &[i64],
    pull_requests: bool,
) -> ResultPage<HashMap<i64, Vec<String>>> {
    let mut assignees: HashMap<i64, Vec<String>> = HashMap::new();
    for (id, username) in
        codeyard_db_repos::assignees::list_item_assignees(&ctx.state.pool, pull_requests, item_ids)
            .await
            .map_err(|_| unavailable(ctx))?
    {
        assignees.entry(id).or_default().push(username);
    }
    Ok(assignees)
}

pub(crate) fn pull_item(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull: codeyard_db_core::models::PullRequest,
    author: String,
    head_repository: HeadRepository,
) -> Result<view::Item, Box<Response>> {
    // Presentation only: the store decides again when the form is sent.
    let can_ready = pull.is_draft
        && pull.state == "open"
        && !repo.record.is_archived
        && (repo.view.can_write
            || ctx
                .user
                .as_ref()
                .is_some_and(|user| user.id == pull.author_id));
    let (head_label, head_url) = match pull.head_repo_id {
        Some(id) if id == repo.record.id => (
            pull.head_branch.clone(),
            Some(codeyard_webui::pages::repos::ref_path::branch_url(
                &repo.view.url(),
                "tree",
                &pull.head_branch,
                "",
            )),
        ),
        Some(_) => match head_repository {
            HeadRepository::Readable(head) => {
                let owner = head.owner_name.ok_or_else(|| Box::new(unavailable(ctx)))?;
                let label = format!("{owner}/{}:{}", head.name, pull.head_branch);
                let url = codeyard_webui::pages::repos::ref_path::branch_url(
                    &view::repo_url(&owner, &head.name),
                    "tree",
                    &pull.head_branch,
                    "",
                );
                (label, Some(url))
            }
            HeadRepository::Unreadable => ("unavailable repository".into(), None),
            HeadRepository::Deleted => ("deleted repository".into(), None),
        },
        None => ("deleted repository".into(), None),
    };
    let may_execute = ctx.user.as_ref().is_some_and(|user| {
        user.is_admin
            || ctx
                .state
                .ci_runs
                .config
                .trusted_actor_ids
                .contains(&user.id)
    });
    let can_run_workflows =
        repo.view.can_write && may_execute && pull.state == "open" && !pull.is_draft;
    // Presentation only: the review handler checks read access and the author again.
    let can_vote = ctx
        .user
        .as_ref()
        .is_some_and(|user| user.id != pull.author_id);
    let can_edit = can_edit_entry(ctx, repo, pull.author_id);
    let can_tick = can_edit;
    Ok(view::Item {
        id: pull.id,
        number: pull.number,
        title: pull.title,
        body: pull.body,
        state: pull.state,
        author,
        created: pull.created_at.to_rfc3339(),
        updated: pull.updated_at.to_rfc3339(),
        labels: vec![],
        stacked: pull.stack_id.is_some(),
        stack: None,
        stack_candidate: None,
        assignees: Vec::new(),
        milestone: None,
        comment_count: 0,
        can_edit,
        can_tick,
        tasks: Default::default(),
        commit_count: None,
        file_count: None,
        check_count: 0,
        review_state: None,
        check_state: None,
        reactions: Vec::new(),
        projects: Vec::new(),
        comparison: Some(view::Comparison {
            head_label,
            head_url,
            can_run_workflows,
            head: pull.head_branch,
            merge_target: pull.base_branch.clone(),
            merge_with: Vec::new(),
            blockers: Vec::new(),
            base: pull.base_branch,
            head_sha: pull.head_sha,
            base_sha: pull.base_sha,
            draft: pull.is_draft,
            can_ready,
            merge_commit_only: false,
            merge_commit_refused: false,
            merged_commit: pull.merge_commit_sha,
            can_vote,
            merge_methods: allowed_merge_methods(&repo.record)
                .into_iter()
                .map(|method| method.as_str().to_owned())
                .collect(),
            has_checks: false,
            failed_checks: 0,
            checks_running: false,
            has_workflows: false,
        }),
    })
}
/// The stack a pull request page shows, with the viewed pull request marked.
fn stack_map(stack: &codeyard_repos::stacks::Stack, current: i64, restack: bool) -> view::StackMap {
    view::StackMap {
        trunk: stack.trunk().to_owned(),
        position: stack.position(current),
        restack,
        layers: stack
            .layers
            .iter()
            .map(|layer| view::StackLayer {
                number: layer.number,
                title: layer.title.clone(),
                state: layer.state.clone(),
                draft: layer.is_draft,
                current: layer.id == current,
            })
            .collect(),
    }
}

/// The milestone an issue or pull request names, as its page shows it.
pub(crate) async fn named_milestone(
    ctx: &PageContext,
    repo: &RepositoryData,
    milestone_id: Option<i64>,
) -> ResultPage<Option<view::Milestone>> {
    let Some(id) = milestone_id else {
        return Ok(None);
    };
    Ok(
        codeyard_db_repos::milestones::get_milestone_by_id(&ctx.state.pool, repo.record.id, id)
            .await
            .map_err(|_| unavailable(ctx))?
            .map(milestone_view),
    )
}

pub(crate) async fn item(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    number: i32,
) -> ResultPage<view::Item> {
    item_with_comparison(ctx, repo, kind, number, true).await
}

/// Mutation admission needs the stored text and revision, not Git comparisons or CI verdicts.
pub(crate) async fn item_metadata(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    number: i32,
) -> ResultPage<view::Item> {
    item_with_comparison(ctx, repo, kind, number, false).await
}

async fn item_with_comparison(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    number: i32,
    comparison: bool,
) -> ResultPage<view::Item> {
    if number <= 0 {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Entry not found."));
    }
    match kind {
        Kind::Issue => {
            let value =
                codeyard_db_repos::issues::get_issue(&ctx.state.pool, repo.record.id, number)
                    .await
                    .map_err(|_| unavailable(ctx))?
                    .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found."))?;
            let author = author(ctx, value.author_id).await?;
            let labels = codeyard_db_repos::labels::get_issue_labels(
                &ctx.state.pool,
                repo.record.id,
                value.id,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            let assignees = codeyard_db_repos::assignees::list_assignees(
                &ctx.state.pool,
                codeyard_db_repos::assignees::AssignedItem::Issue(value.id),
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            let milestone = named_milestone(ctx, repo, value.milestone_id).await?;
            let comment_count =
                codeyard_db_repos::issues::count_issue_comments(&ctx.state.pool, &[value.id])
                    .await
                    .map_err(|_| unavailable(ctx))?
                    .first()
                    .map_or(0, |(_, count)| *count);
            let can_edit = can_edit_entry(ctx, repo, value.author_id);
            let mut shown = issue_item(value, author, labels);
            shown.can_edit = can_edit;
            shown.can_tick = can_edit;
            shown.assignees = assignees;
            shown.milestone = milestone;
            shown.comment_count = comment_count;
            Ok(shown)
        }
        Kind::Pull => {
            let value = codeyard_db_repos::pull_requests::records::get_pull_request(
                &ctx.state.pool,
                repo.record.id,
                number,
            )
            .await
            .map_err(|_| unavailable(ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))?;
            let author = author(ctx, value.author_id).await?;
            if !comparison {
                return pull_item(ctx, repo, value, author, HeadRepository::Unreadable)
                    .map_err(|response| *response);
            }
            let head = match value.head_repo_id {
                Some(id) if id != repo.record.id => {
                    codeyard_db_repos::repositories::records::get_repository_by_id(
                        &ctx.state.pool,
                        id,
                    )
                    .await
                    .map_err(|_| unavailable(ctx))?
                }
                _ => None,
            };
            let head_repository = head_for_viewer(ctx, head).await?;
            let current = value.id;
            let stack = codeyard_repos::stacks::stack_of(&ctx.state.pool, &value)
                .await
                .map_err(|_| unavailable(ctx))?;
            let stack_candidate = codeyard_repos::stacks::join_candidate(&ctx.state.pool, &value)
                .await
                .map_err(|_| unavailable(ctx))?;
            let (merge_target, _, merge_with) =
                codeyard_repos::stacks::merge_target(&ctx.state.pool, &value)
                    .await
                    .map_err(|_| unavailable(ctx))?;
            // Only a writer sees the button, so readers do not pay for the check.
            let restack = match &stack {
                Some(stack) if repo.view.can_write => {
                    codeyard_repos_pulls::repositories::pull_requests::restack::restack_pending(
                        &ctx.state.ci_runs,
                        &repo.record,
                        &value,
                        &stack.layers,
                    )
                    .await
                }
                _ => false,
            };
            let milestone_id = value.milestone_id;
            let labels = codeyard_db_repos::labels::get_pull_request_labels(
                &ctx.state.pool,
                repo.record.id,
                current,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            let head_sha = value.head_sha.clone();
            let diff_base = value.diff_base().to_owned();
            let mut view_item = pull_item(ctx, repo, value, author, head_repository)
                .map_err(|response| *response)?;
            view_item.labels = labels.into_iter().map(label).collect();
            view_item.milestone = named_milestone(ctx, repo, milestone_id).await?;
            view_item.stack = stack
                .as_ref()
                .map(|stack| stack_map(stack, current, restack));
            view_item.stack_candidate = stack_candidate;
            view_item.assignees = codeyard_db_repos::assignees::list_assignees(
                &ctx.state.pool,
                codeyard_db_repos::assignees::AssignedItem::PullRequest(current),
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            view_item.comment_count =
                codeyard_db_repos::pull_requests::reviews::count_pull_request_comments(
                    &ctx.state.pool,
                    current,
                )
                .await
                .map_err(|_| unavailable(ctx))?;
            // A check run or a commit status on the head commit is what makes a green merge box mean
            // something; the merge box says so when there is none.
            let runs = codeyard_db_actions::ci::latest_ci_runs_for_commit(
                &ctx.state.pool,
                repo.record.id,
                &head_sha,
                current,
                &merge_target,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            let statuses = codeyard_db_repos::repositories::protection::list_commit_statuses(
                &ctx.state.pool,
                repo.record.id,
                &head_sha,
            )
            .await
            .map_err(|_| unavailable(ctx))?
            .into_iter()
            .filter(|status| {
                !codeyard_db_repos::repositories::protection::server_owned_status_context(
                    &status.context,
                )
            })
            .collect::<Vec<_>>();
            let integration =
                codeyard_db_actions::checks::summary(&ctx.state.pool, repo.record.id, &head_sha)
                    .await
                    .map_err(|_| unavailable(ctx))?;
            view_item.check_count = runs.len() as i64
                + integration.total
                + statuses
                    .iter()
                    .filter(|status| {
                        !runs
                            .iter()
                            .any(|run| run.commit_status_context() == status.context)
                    })
                    .count() as i64;
            // The Commits tab counts the commits of the pull request, up to one more than it lists so
            // the tab can say "100+". When Git cannot count them the count is left out, so the tab
            // shows no number instead of a wrong one.
            let commit_count = match view_item.comparison.as_ref() {
                Some(compare) => repo_store(&repo.record, ctx.state.cluster.as_ref())
                    .branch_statuses(&diff_base, std::slice::from_ref(&compare.head_sha))
                    .await
                    .ok()
                    .and_then(|heads| {
                        heads
                            .first()
                            .map(|head| head.ahead.min(MAX_LISTED_COMMITS + 1) as i64)
                    }),
                None => None,
            };
            view_item.commit_count = commit_count;
            // Tab counts use metadata only; their full payloads are loaded by the selected tab.
            let file_count = match view_item.comparison.as_ref() {
                Some(compare) => repo_store(&repo.record, ctx.state.cluster.as_ref())
                    .diff_paths(&diff_base, &compare.head_sha)
                    .await
                    .ok()
                    .map(|paths| paths.len() as i64),
                None => None,
            };
            view_item.file_count = file_count;
            if let Some(compare) = view_item.comparison.as_mut() {
                compare.merge_commit_only = merge_with.len() > 1;
                compare.merge_target = merge_target;
                compare.merge_with = merge_with;
                compare.has_checks =
                    !runs.is_empty() || !statuses.is_empty() || integration.total > 0;
                // Each check counts once. A run writes its commit status under its own context, so only
                // the statuses of other producers are added. Runs are the latest attempt of each workflow.
                let states: Vec<&str> = runs
                    .iter()
                    .map(|run| run.status.as_str())
                    .chain(
                        statuses
                            .iter()
                            .filter(|status| {
                                !runs
                                    .iter()
                                    .any(|run| run.commit_status_context() == status.context)
                            })
                            .map(|status| status.state.as_str()),
                    )
                    .collect();
                // A check that is queued or pending has not failed yet.
                compare.failed_checks = states
                    .iter()
                    .filter(|state| {
                        !matches!(**state, "success" | "queued" | "running" | "pending")
                    })
                    .count() as i64
                    + integration.failed;
                compare.checks_running = integration.pending > 0
                    || states
                        .iter()
                        .any(|state| matches!(*state, "queued" | "running" | "pending"));
            }
            Ok(view_item)
        }
    }
}
/// Comment authors and repository writers may moderate visible comments. The mutation transaction
/// remains authoritative; this view uses the repository page's existing permission projection.
pub(crate) struct CommentRights {
    viewer: Option<i64>,
    writes: bool,
    open: bool,
}

impl CommentRights {
    pub(crate) fn for_repository(ctx: &PageContext, repo: &RepositoryData, open: bool) -> Self {
        Self {
            viewer: ctx.user.as_ref().map(|user| user.id),
            writes: repo.view.can_write,
            open: (open || repo.view.can_write) && !repo.record.is_archived,
        }
    }

    pub(crate) fn apply(
        &self,
        comment: &mut view::Comment,
        author_id: i64,
        created: DateTime<Utc>,
        updated: DateTime<Utc>,
    ) {
        let mine = self.viewer == Some(author_id);
        comment.can_edit = self.open && (mine || self.writes);
        comment.can_delete = self.open && (mine || self.writes);
        comment.edited = (updated > created).then(|| updated.to_rfc3339());
    }
}

/// One page of the discussion, from the `page` query value (default 1). One extra
/// row is read to tell whether a next page exists.
pub(crate) async fn discussion(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    number: i32,
    edit_page: Option<usize>,
) -> ResultPage<view::CommentPage> {
    let page = match edit_page {
        Some(page) => page,
        None => crate::paging::number(ctx, "page", 1, 1..=MAX_COMMENT_PAGE)?,
    };
    if page == 0 {
        return Err(ctx.error(StatusCode::BAD_REQUEST, "Page numbers start at 1."));
    }
    let offset = (page - 1)
        .checked_mul(COMMENTS_PER_PAGE)
        .and_then(|offset| i64::try_from(offset).ok())
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid comment page."))?;
    let fetch = (COMMENTS_PER_PAGE + 1) as i64;
    let mut comments = Vec::new();
    let mut reviews = Vec::new();
    let has_more: bool;
    let locked: bool;
    let lock_reason: Option<String>;
    // The item and the span of time this page covers, for the events that fall in it.
    let subject: (codeyard_db_repos::timeline::Subject, i64);
    let span: (Option<DateTime<Utc>>, Option<DateTime<Utc>>);
    match kind {
        Kind::Issue => {
            let issue =
                codeyard_db_repos::issues::get_issue(&ctx.state.pool, repo.record.id, number)
                    .await
                    .map_err(|_| unavailable(ctx))?
                    .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found."))?;
            let rows = codeyard_db_repos::issues::list_issue_comments(
                &ctx.state.pool,
                issue.id,
                fetch,
                offset,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            locked = issue.locked;
            lock_reason = issue.lock_reason;
            let rights = CommentRights::for_repository(ctx, repo, !locked);
            has_more =
                crate::paging::has_next_page(page, rows.len(), COMMENTS_PER_PAGE, MAX_COMMENT_PAGE);
            subject = (codeyard_db_repos::timeline::Subject::Issue, issue.id);
            span = (
                (page > 1)
                    .then(|| rows.first().map(|row| row.comment.created_at))
                    .flatten(),
                rows.get(COMMENTS_PER_PAGE)
                    .map(|row| row.comment.created_at),
            );
            for comment in rows.into_iter().take(COMMENTS_PER_PAGE) {
                let author = comment.author.username;
                let comment = comment.comment;
                let mut shown = view::Comment {
                    id: comment.id,
                    author,
                    revision: comment_revision(&comment.body),
                    body: comment.body,
                    created: comment.created_at.to_rfc3339(),
                    location: None,
                    conversation: None,
                    reactions: Vec::new(),
                    can_edit: false,
                    can_delete: false,
                    edited: None,
                    edit: None,
                };
                rights.apply(
                    &mut shown,
                    comment.author_id,
                    comment.created_at,
                    comment.updated_at,
                );
                comments.push(shown);
            }
        }
        Kind::Pull => {
            let pull = codeyard_db_repos::pull_requests::records::get_pull_request(
                &ctx.state.pool,
                repo.record.id,
                number,
            )
            .await
            .map_err(|_| unavailable(ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))?;
            // The page is a run of Conversation items, so the count, the links and the reviews all
            // agree. The page's span runs from its first item up to the next page's first item.
            let rows = codeyard_db_repos::pull_requests::reviews::list_discussion_items(
                &ctx.state.pool,
                pull.id,
                fetch,
                offset,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            has_more =
                crate::paging::has_next_page(page, rows.len(), COMMENTS_PER_PAGE, MAX_COMMENT_PAGE);
            let first = rows.first().map(DiscussionItem::created_at);
            span = (
                (page > 1).then_some(first).flatten(),
                rows.get(COMMENTS_PER_PAGE).map(DiscussionItem::created_at),
            );
            subject = (codeyard_db_repos::timeline::Subject::PullRequest, pull.id);
            let mut page_reviews = Vec::new();
            locked = pull.locked;
            lock_reason = pull.lock_reason.clone();
            let rights = CommentRights::for_repository(ctx, repo, !locked);
            for row in rows.into_iter().take(COMMENTS_PER_PAGE) {
                match row {
                    DiscussionItem::Comment(entry) => {
                        let (author_id, created, updated) = (
                            entry.comment.author_id,
                            entry.comment.created_at,
                            entry.comment.updated_at,
                        );
                        let mut shown =
                            crate::pulls::review_forms::discussion_comment(ctx, repo, &pull, entry);
                        rights.apply(&mut shown, author_id, created, updated);
                        comments.push(shown);
                    }
                    DiscussionItem::Review(entry) => page_reviews.push(entry),
                }
            }
            reviews =
                crate::pulls::review_forms::page_reviews(ctx, repo, &pull, page_reviews, &rights)
                    .await?;
        }
    }
    let max_event_page = (i64::MAX / codeyard_db_repos::timeline::EVENT_LIMIT) as usize;
    let event_page = crate::paging::number(ctx, "event_page", 1, 1..=max_event_page)?;
    let event_offset = (event_page as i64 - 1) * codeyard_db_repos::timeline::EVENT_LIMIT;
    let mut event_rows = codeyard_db_repos::timeline::list_events(
        &ctx.state.pool,
        repo.record.id,
        subject.0.name(),
        subject.1,
        span.0,
        span.1,
        codeyard_db_repos::timeline::EVENT_LIMIT + 1,
        event_offset,
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    let events_next = crate::paging::has_next_page(
        event_page,
        event_rows.len(),
        codeyard_db_repos::timeline::EVENT_LIMIT as usize,
        max_event_page,
    )
    .then_some(event_page + 1);
    event_rows.truncate(codeyard_db_repos::timeline::EVENT_LIMIT as usize);
    let events = event_rows
        .into_iter()
        .map(|event| view::Event {
            actor: event.actor.unwrap_or_else(|| "Deleted user".to_owned()),
            kind: event.kind,
            detail: event.detail,
            previous: event.previous,
            created: event.created_at.to_rfc3339(),
        })
        .collect();
    let item = match kind {
        Kind::Issue => codeyard_db_repos::assignees::AssignedItem::Issue(subject.1),
        Kind::Pull => codeyard_db_repos::assignees::AssignedItem::PullRequest(subject.1),
    };
    let pinned = crate::issues::pins::pinned_comment(ctx, repo, kind, number, item).await?;
    let relationships = if kind == Kind::Issue {
        crate::issues::relationships::load(ctx, subject.1).await?
    } else {
        Vec::new()
    };
    let attributes = if kind == Kind::Issue {
        Some(
            crate::issues::attributes::load(
                ctx,
                repo.record.id,
                subject.1,
                repo.record.owner_kind == "org",
            )
            .await?,
        )
    } else {
        None
    };
    let suggestions = if kind == Kind::Issue {
        crate::issues::suggestions::load(ctx, &repo.record, subject.1, locked, repo.view.can_admin)
            .await?
    } else {
        None
    };
    let issue_management = if kind == Kind::Issue {
        let issue = codeyard_db_repos::issues::get_issue_by_id(&ctx.state.pool, subject.1)
            .await
            .map_err(|_| unavailable(ctx))?
            .ok_or_else(|| unavailable(ctx))?;
        Some(crate::issues::management::load(ctx, repo, &issue).await?)
    } else {
        None
    };
    Ok(view::CommentPage {
        can_triage: match &ctx.user {
            Some(user) => codeyard_identity::repository_access::can_triage(
                &ctx.state.pool,
                user,
                &repo.record,
            )
            .await
            .map_err(|_| unavailable(ctx))?,
            None => false,
        },
        issue_management,
        suggestions,
        attributes,
        relationships,
        pinned,
        locked,
        lock_reason,
        comments,
        reviews,
        page,
        has_more,
        events,
        events_next,
    })
}
pub(crate) fn require_item_binding(
    ctx: &PageContext,
    form: &HashMap<String, String>,
    repository_id: i64,
    item_id: i64,
) -> ResultPage<()> {
    if form.get("repository_id").and_then(|id| id.parse().ok()) != Some(repository_id)
        || form.get("item_id").and_then(|id| id.parse().ok()) != Some(item_id)
    {
        return Err(ctx.error(
            StatusCode::CONFLICT,
            "The repository or entry changed. Reload before changing metadata.",
        ));
    }
    Ok(())
}

pub(crate) async fn render_detail(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    shown: &view::Item,
    draft: &DetailDraft,
    error: Option<&str>,
    status: StatusCode,
) -> ResultPage<Response> {
    let mut value = shown.clone();
    value.projects =
        codeyard_api_http::shared::projects::sidebar(ctx, repo.record.id, kind, shown.id)
            .await
            .map_err(|_| unavailable(ctx))?;
    fill_merge_blockers(ctx, repo, kind, draft, &mut value).await;
    let edit_page = match draft.editing {
        Some(id) => {
            let position = match kind {
                Kind::Issue => match codeyard_db_repos::issues::get_issue_comment(&ctx.state.pool, id).await {
                    Ok(Some(comment)) if comment.issue_id == shown.id => {
                        codeyard_db_repos::issues::issue_comment_position(&ctx.state.pool, id).await
                    }
                    _ => Err(anyhow::anyhow!("Comment not found")),
                },
                Kind::Pull => match codeyard_db_repos::pull_requests::reviews::get_pull_request_comment_by_id(
                    &ctx.state.pool,
                    id,
                )
                .await
                {
                    Ok(Some(comment)) if comment.pull_request_id == shown.id => {
                        codeyard_db_repos::pull_requests::reviews::count_discussion_items_through(
                            &ctx.state.pool,
                            comment.pull_request_id,
                            codeyard_db_repos::pull_requests::reviews::DiscussionRef::of_comment(&comment),
                        )
                        .await
                    }
                    _ => Err(anyhow::anyhow!("Comment not found")),
                },
            };
            Some(codeyard_repos_pulls::issues::discussion_page(position))
        }
        None => None,
    };
    let mut comments = discussion(ctx, repo, kind, value.number, edit_page).await?;
    // A comment whose edit was previewed or refused keeps the text the author typed.
    if let Some(id) = draft.editing {
        // The comment may be a top-level one or one filed in a submitted review.
        if let Some(comment) = comments
            .comments
            .iter_mut()
            .chain(
                comments
                    .reviews
                    .iter_mut()
                    .flat_map(|review| review.comments.iter_mut()),
            )
            .find(|comment| comment.id == id)
        {
            comment.edit = Some(view::CommentEdit {
                original_body: serde_json::from_str(&draft.expected_body).unwrap_or_default(),
                body: draft.comment.clone(),
                preview: draft.preview,
            });
        }
    }
    crate::issues::reactions::attach(ctx, kind, &mut value, &mut comments).await?;
    // Every text this page renders as Markdown, including what a form re-renders.
    let mut texts = vec![value.body.as_str(), draft.comment.as_str()];
    texts.extend(draft.body.as_deref());
    texts.extend(
        comments
            .pinned
            .as_ref()
            .map(|comment| comment.body.as_str()),
    );
    texts.extend(
        comments
            .comments
            .iter()
            .map(|comment| comment.body.as_str()),
    );
    texts.extend(
        comments
            .reviews
            .iter()
            .filter_map(|review| review.body.as_deref()),
    );
    texts.extend(
        comments
            .reviews
            .iter()
            .flat_map(|review| review.comments.iter().map(|comment| comment.body.as_str())),
    );
    // A detail page also re-renders the user's submitted draft, so a failed lookup links nothing.
    // Every text here renders as a comment, so the references are resolved in comment mode.
    let references = resolve_references_or_none(
        ctx,
        &repo.record,
        &repo.view.owner,
        &repo.view.name,
        &texts,
        false,
    )
    .await;
    // The label picker lists every label of the repository. A label the item has is removed by
    // clicking it, and any other label is added the same way.
    let mut labels: Vec<_> = if comments.can_triage {
        codeyard_db_repos::labels::list_repo_labels_page(
            &ctx.state.pool,
            repo.record.id,
            ctx.query.get("label_search").map_or("", String::as_str),
            "name",
            100,
            0,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        .into_iter()
        .map(label)
        .collect()
    } else {
        Vec::new()
    };
    for applied in &value.labels {
        if !labels.iter().any(|label| label.id == applied.id) {
            labels.push(applied.clone());
        }
    }
    // Choices are offered only when the viewer may triage.
    let (assignable, milestones): (Vec<String>, Vec<view::Milestone>) = if comments.can_triage {
        let assignable = if (value.assignees.len() as i64)
            < codeyard_db_repos::assignees::MAX_ASSIGNEES
        {
            codeyard_db_repos::assignees::list_assignable_usernames(&ctx.state.pool, repo.record.id)
                .await
                .map_err(|_| unavailable(ctx))?
                .into_iter()
                .filter(|name| !value.assignees.contains(name))
                .collect()
        } else {
            Vec::new()
        };
        let milestones =
            codeyard_db_repos::milestones::list_milestone_choices(&ctx.state.pool, repo.record.id)
                .await
                .map_err(|_| unavailable(ctx))?
                .into_iter()
                .map(milestone_choice_view)
                .collect();
        (assignable, milestones)
    } else {
        (Vec::new(), Vec::new())
    };
    let mut ctx = ctx.clone();
    ctx.return_to = Some(codeyard_repos_pulls::issues::discussion_link(
        &format!("{}/{}", repo.view.family_url(kind), value.number),
        comments.page,
        "",
    ));
    let ctx = &ctx;
    let mut reviewer_choices: Vec<String> = if kind == Kind::Pull && repo.view.can_write {
        codeyard_db_repos::repositories::access::list_write_access_users(
            &ctx.state.pool,
            repo.record.id,
            -1,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        .into_iter()
        .map(|(name, _)| name)
        .collect()
    } else {
        Vec::new()
    };
    if kind == Kind::Pull && repo.view.can_write {
        let teams = codeyard_db_repos::pull_requests::team_reviewers::list(
            &ctx.state.pool,
            repo.record.id,
            None,
            ctx.user.as_ref().map(|user| user.id),
        )
        .await
        .map_err(|_| unavailable(ctx))?;
        if let Some(owner) =
            codeyard_db_actions::ci::ci_repository_owner(&ctx.state.pool, repo.record.id)
                .await
                .map_err(|_| unavailable(ctx))?
        {
            reviewer_choices.extend(
                teams
                    .into_iter()
                    .map(|team| format!("@{owner}/{}", team.team.slug)),
            );
        }
    }
    let reviewers = match &value.comparison {
        Some(compare) if kind == Kind::Pull => {
            crate::pulls::review_forms::reviewers(
                ctx,
                repo,
                value.id,
                (&compare.base_sha, &compare.head_sha),
            )
            .await?
        }
        _ => Vec::new(),
    };
    if kind == Kind::Issue {
        codeyard_db_repos::issues::get_issue_by_id(&ctx.state.pool, value.id)
            .await
            .map_err(|_| unavailable(ctx))?
            .filter(|issue| issue.repo_id == repo.record.id)
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found."))?;
    }
    check_repo_read_access(&ctx.state.pool, &repo.record, &ctx.headers)
        .await
        .map_err(|(status, _)| ctx.error(status, "Repository not found."))?;
    Ok(with_status(
        ctx.page_model(
            &format!("{} #{}", value.title, value.number),
            page(view::Page::Detail {
                repository: repo.view.clone(),
                kind,
                item: value.clone(),
                comments,
                available_labels: labels,
                draft: draft.clone(),
                error: error.map(str::to_owned),
                references: Box::new(references),
                reviewers,
                reviewer_choices,
                assignable,
                milestones,
            }),
        ),
        status,
    ))
}

/// The page after a form that changes an issue or pull request: the item again, or the item
/// with the reason the change was refused.
pub(crate) async fn detail_outcome(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    shown: &view::Item,
    number: i32,
    outcome: Result<(), ApiFailure>,
) -> ResultPage<Response> {
    match outcome {
        Ok(()) => Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(kind), number))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(
                ctx,
                repo,
                kind,
                shown,
                &DetailDraft::default(),
                Some(&message),
                status,
            )
            .await
        }
    }
}

/// Lists what blocks a merge in the merge panel. Only the detail page shows the panel, so only
/// its render asks, and only a writer gets the list. The list is for the method the form starts on,
/// which is the submitted one when a refused merge shows the form again. The list is advisory, so a
/// failed read leaves it empty.
async fn fill_merge_blockers(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    draft: &DetailDraft,
    value: &mut view::Item,
) {
    let Some(viewer) = ctx.user.as_ref().filter(|_| repo.view.can_write) else {
        return;
    };
    if kind != Kind::Pull || value.state != "open" {
        return;
    }
    let number = value.number;
    let Some(compare) = value.comparison.as_mut() else {
        return;
    };
    // The merge box links "No checks configured" to the Actions page only when the head commit has
    // workflows. A failed read leaves the link out.
    if !compare.has_checks {
        compare.has_workflows = repo_store(&repo.record, ctx.state.cluster.as_ref())
            .workflow_files(&compare.head_sha)
            .await
            .is_ok_and(|files| !files.is_empty());
    }
    let Ok(Some(pull_request)) = codeyard_db_repos::pull_requests::records::get_pull_request(
        &ctx.state.pool,
        repo.record.id,
        number,
    )
    .await
    else {
        return;
    };
    let blockers = |method| {
        codeyard_repos_pulls::repositories::pull_requests::merge::page_blockers(
            &ctx.state.pool,
            &ctx.state.ci_runs,
            &repo.record,
            viewer,
            &pull_request,
            method,
        )
    };
    // A branch that requires a linear history refuses the merge commit method for this viewer, so
    // the form starts on squash or rebase. A failed read leaves the method on offer, and the merge
    // checks again.
    let merge_blockers = blockers(MergeMethod::Merge).await;
    compare.merge_commit_refused = merge_blockers
        .iter()
        .any(|message| message.ends_with(view::MERGE_COMMIT_REFUSED));
    let allowed = allowed_merge_methods(&repo.record);
    let preselected = view::preselected_merge_method(compare, &draft.merge_method)
        .and_then(|name| allowed.into_iter().find(|method| method.as_str() == name));
    let Some(method) = preselected else {
        // No method can be chosen, for example a stack on a branch that requires a linear
        // history, so the page says why instead of offering a merge that cannot start.
        compare.blockers = merge_blockers;
        if compare.blockers.is_empty() {
            compare
                .blockers
                .push("No merge method is available for this pull request.".to_owned());
        }
        return;
    };
    compare.blockers = if method == MergeMethod::Merge {
        merge_blockers
    } else {
        blockers(method).await
    };
}

/// The branch names of a repository, sorted, and all its refs.
pub(crate) async fn branches(
    ctx: &PageContext,
    repository: &codeyard_db_core::models::Repository,
) -> ResultPage<(Vec<String>, HashMap<String, String>)> {
    let refs = repo_store(repository, ctx.state.cluster.as_ref())
        .refs()
        .await
        .map_err(|error| {
            api_error(
                ctx,
                crate::responses::repo_store_error(error, StatusCode::SERVICE_UNAVAILABLE),
            )
        })?;
    let mut branches: Vec<_> = refs
        .keys()
        .filter_map(|r| r.strip_prefix("refs/heads/").map(str::to_owned))
        .collect();
    branches.sort();
    Ok((branches, refs))
}

pub(crate) async fn head_choices(
    ctx: &PageContext,
    base: &RepositoryData,
    draft: &mut NewDraft,
) -> ResultPage<(
    view::BranchChoices,
    codeyard_db_core::models::Repository,
    HashMap<String, String>,
)> {
    let mut candidates = vec![base.record.clone()];
    let mut forks = codeyard_db_repos::repositories::discovery::list_forks(
        &ctx.state.pool,
        base.record.id,
        100,
        0,
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    // The user's own fork and an explicitly selected head stay reachable beyond the first
    // page. The batch below still checks read access for each of them.
    let own_fork = match &ctx.user {
        Some(user) => codeyard_db_repos::repositories::discovery::get_user_fork(
            &ctx.state.pool,
            user.id,
            base.record.id,
        )
        .await
        .map_err(|_| unavailable(ctx))?,
        None => None,
    };
    let selected = match draft.head_repo.parse::<i64>() {
        Ok(id) => {
            codeyard_db_repos::repositories::records::get_repository_by_id(&ctx.state.pool, id)
                .await
                .map_err(|_| unavailable(ctx))?
        }
        Err(_) => None,
    };
    for fork in own_fork
        .into_iter()
        .chain(selected.filter(|repo| repo.forked_from_id == Some(base.record.id)))
    {
        if !forks.iter().any(|candidate| candidate.id == fork.id) {
            forks.push(fork);
        }
    }
    let ids: Vec<_> = forks.iter().map(|fork| fork.id).collect();
    let mut readable = Vec::new();
    for ids in ids.chunks(100) {
        readable.extend(
            codeyard_db_repos::repositories::access::readable_repository_ids(
                &ctx.state.pool,
                ctx.user.as_ref().map(|user| user.id),
                ids,
            )
            .await
            .map_err(|_| unavailable(ctx))?,
        );
    }
    candidates.extend(forks.into_iter().filter(|fork| readable.contains(&fork.id)));
    if draft.head_repo.is_empty() {
        let own = candidates.iter().find(|head| {
            head.id != base.record.id
                && ctx
                    .user
                    .as_ref()
                    .is_some_and(|user| head.is_personal_owner(user.id))
        });
        draft.head_repo = own.unwrap_or(&base.record).id.to_string();
    }
    let selected = candidates
        .iter()
        .find(|head| head.id.to_string() == draft.head_repo)
        .cloned()
        .ok_or_else(|| {
            ctx.error(
                StatusCode::NOT_FOUND,
                "Head repository not found or unavailable.",
            )
        })?;
    let ids = candidates
        .iter()
        .map(|candidate| candidate.id)
        .collect::<Vec<_>>();
    let mut owners =
        codeyard_db_repos::repositories::records::repository_owner_names(&ctx.state.pool, &ids)
            .await
            .map_err(|_| unavailable(ctx))?;
    let mut repositories = Vec::new();
    for candidate in candidates {
        let owner = owners
            .remove(&candidate.id)
            .ok_or_else(|| unavailable(ctx))?;
        repositories.push((candidate.id, format!("{owner}/{}", candidate.name)));
    }
    let (base_branches, _) = branches(ctx, &base.record).await?;
    let (head_branches, refs) = branches(ctx, &selected).await?;
    Ok((
        view::BranchChoices {
            repositories,
            base: base_branches,
            head: head_branches,
        },
        selected,
        refs,
    ))
}

pub(crate) fn expected(
    form: &HashMap<String, String>,
    value: &view::Item,
) -> Result<(), &'static str> {
    if form.get("expected_state") != Some(&value.state)
        || form.get("expected_updated") != Some(&value.updated)
    {
        Err("This entry changed after the form was opened. Review the current entry and your preserved submission before retrying.")
    } else {
        Ok(())
    }
}

use codeyard_api_http::shared::repos::discussion::page;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use codeyard_app::app::AppState;
    use std::sync::Arc;

    #[tokio::test]
    async fn listed_heads_reuse_the_base_and_each_distinct_visibility_check() {
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
        for (name, private) in [("proj", false), ("vault", true)] {
            codeyard_repos_core::repositories::lifecycle::provision(
                (&pool, None),
                root.path(),
                codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(
                    owner.id,
                ),
                name,
                None,
                private,
                None,
            )
            .await
            .unwrap();
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
        let ctx = PageContext::load(
            state,
            HeaderMap::new(),
            &"/alice/proj/pulls".parse().unwrap(),
        )
        .await
        .unwrap();
        let repo = repository(&ctx, "alice", "proj").await.unwrap();
        let private =
            codeyard_db_repos::repositories::records::get_repository(&pool, "alice", "vault")
                .await
                .unwrap()
                .unwrap();
        let display =
            |id, name: &str| codeyard_db_repos::pull_requests::records::PullRequestHeadDisplay {
                id,
                name: name.into(),
                owner_name: Some("alice".into()),
            };
        let mut cache = HashMap::new();
        assert!(matches!(
            listed_head(
                &ctx,
                repo.record.id,
                &mut cache,
                Some(display(private.id, "vault"))
            )
            .await
            .unwrap(),
            HeadRepository::Unreadable
        ));
        pool.close().await;
        for _ in 0..100 {
            assert!(matches!(
                listed_head(
                    &ctx,
                    repo.record.id,
                    &mut cache,
                    Some(display(repo.record.id, "proj"))
                )
                .await
                .unwrap(),
                HeadRepository::Deleted
            ));
            assert!(matches!(
                listed_head(
                    &ctx,
                    repo.record.id,
                    &mut cache,
                    Some(display(private.id, "vault"))
                )
                .await
                .unwrap(),
                HeadRepository::Unreadable
            ));
        }
    }
}
