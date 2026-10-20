use super::issue::{load_issue_response, IssueResponse};
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome;
use crate::backend::api::repos::issues::assignees::user_id;
use crate::backend::api::repos::milestones::{milestone_choice_view, milestone_view};
use crate::backend::api::repos::pulls::pull::*;
use crate::backend::api::repos::refs::compare::comparison;
use crate::backend::api::repos::refs::references::resolve_references_or_none;
use crate::backend::api::repos::repository::repeated;
use crate::backend::api::user::responses::PublicUserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::assignees::AssignedItem;
use crate::backend::db::discussions::{ListFilter, ListSort, Pick};
use crate::backend::issues::mutations::CreateIssueRequest;
use crate::backend::notifications::assignments::{notify_assignees, Assignment};
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::repositories::issue_templates::{
    compose_form_body, first_missing_answer, issue_templates, pull_request_template, FormElement,
    FormKind, IssueTemplate, IssueTemplates,
};
use crate::backend::repositories::store::repo_store;
use crate::webui::components::references::References;
use crate::webui::components::task_lists::count_tasks;
use crate::webui::pages::repos::issues as view;
use crate::webui::pages::repos::issues::Kind;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
pub async fn create_issue(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<CreateIssueRequest>,
) -> Result<(StatusCode, Json<IssueResponse>), (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let issue = crate::backend::issues::mutations::create_issue(
        &state.pool,
        &state.notification_broadcaster,
        current_actor.id,
        owner,
        repo,
        req,
    )
    .await
    .map_err(super::operation_error)?;
    let response = load_issue_response(
        &state.pool,
        issue,
        Some(PublicUserResponse::from(current_actor)),
    )
    .await
    .map_err(crate::backend::api::http::error::discussion_write_error)?;

    Ok((StatusCode::CREATED, Json(response)))
}

#[derive(Deserialize)]
pub struct IssueQuery {
    pub state: Option<String>,
    pub labels: Option<String>,
    /// One label name, matched as typed, as the HTML list takes it.
    pub label: Option<String>,
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

/// The most label names one list may filter by.
const MAX_FILTER_LABELS: usize = 10;

/// The label names an issue list must all carry. `labels` is comma-separated; each `typed` value
/// is one name, matched as typed, so a name may contain commas. Names are trimmed, without empty
/// names or names repeated ignoring case, and at most ten of them.
fn filter_label_names<'a>(
    labels: Option<&'a str>,
    typed: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<String>, &'static str> {
    let mut names: Vec<String> = Vec::new();
    for name in labels
        .unwrap_or_default()
        .split(',')
        .chain(typed)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        if !names.iter().any(|known| known.eq_ignore_ascii_case(name)) {
            names.push(name.to_owned());
        }
    }
    if names.len() > MAX_FILTER_LABELS {
        return Err("Filter by at most 10 labels.");
    }
    Ok(names)
}
pub async fn list_issues(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<IssueQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<IssueResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Collaboration storage is temporarily unavailable",
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

    let issue_state = match query.state.as_deref() {
        Some("open") | Some("closed") => query.state.as_deref(),
        Some("all") | None => None,
        Some(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(
                    "validation_error",
                    "state must be one of: open, closed, all",
                )),
            ))
        }
    };
    let labels =
        filter_label_names(query.labels.as_deref(), query.label.as_deref()).map_err(|message| {
            (
                StatusCode::BAD_REQUEST,
                Json(ApiError::new("validation_error", message)),
            )
        })?;

    let (offset, limit) = chrome::json_page(query.page, Some(query.per_page.unwrap_or(100)))?;
    let issues = db::issues::list_repo_issues(
        &state.pool,
        repository.id,
        &db::discussions::ListFilter {
            state: issue_state,
            labels: &labels,
            offset: offset as i64,
            ..Default::default()
        },
        limit as i64,
    )
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new(
                "database_error",
                "Collaboration storage is temporarily unavailable",
            )),
        )
    })?;

    let ids = issues
        .iter()
        .map(|entry| entry.issue.id)
        .collect::<Vec<_>>();
    let mut labels = db::labels::get_issues_labels(&state.pool, &ids)
        .await
        .map_err(crate::backend::api::http::error::discussion_write_error)?;
    let mut comments: HashMap<i64, i64> = db::issues::count_issue_comments(&state.pool, &ids)
        .await
        .map_err(crate::backend::api::http::error::discussion_write_error)?
        .into_iter()
        .collect();
    let mut responses = Vec::with_capacity(issues.len());
    for entry in issues {
        let author = Some(PublicUserResponse::from_display(&entry.author));
        let issue_labels = labels.remove(&entry.issue.id).unwrap_or_default();
        let comments_count = comments.remove(&entry.issue.id).unwrap_or(0);
        responses.push(IssueResponse::from_issue(
            entry.issue,
            author,
            issue_labels,
            comments_count,
        ));
    }

    Ok(Json(responses))
}

/// The longest search text a list accepts, in characters.
const MAX_SEARCH_CHARS: usize = 200;

/// A query value, trimmed. A missing value is empty.
fn query_value(ctx: &PageContext, name: &str) -> String {
    ctx.query
        .get(name)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

/// The narrowing a list query asks for, with the names resolved to ids. A name that matches
/// no user or milestone is a bad request.
async fn query_narrowing(
    ctx: &PageContext,
    repo: &RepositoryData,
) -> ResultPage<ListFilter<'static>> {
    let unknown = |message: &str| ctx.error(StatusCode::BAD_REQUEST, message);
    let assignee = match query_value(ctx, "assignee").as_str() {
        "" => Pick::Any,
        "none" => Pick::Unset,
        name => match user_id(ctx, name).await? {
            Some(id) => Pick::Is(id),
            None => return Err(unknown("Choose an assignee from the list.")),
        },
    };
    let milestone = match query_value(ctx, "milestone").as_str() {
        "" => Pick::Any,
        "none" => Pick::Unset,
        text => {
            let found = match text.parse::<i32>() {
                Ok(number) => {
                    db::milestones::get_milestone(&ctx.state.pool, repo.record.id, number)
                        .await
                        .map_err(|_| unavailable(ctx))?
                }
                Err(_) => None,
            };
            match found {
                Some(milestone) => Pick::Is(milestone.milestone.id),
                None => return Err(unknown("Choose a milestone from the list.")),
            }
        }
    };
    let author_name = query_value(ctx, "author");
    let author_id = if author_name.is_empty() {
        None
    } else {
        match user_id(ctx, &author_name).await? {
            Some(id) => Some(id),
            None => return Err(unknown("Choose an author from the list.")),
        }
    };
    let sort = match query_value(ctx, "sort").as_str() {
        "" => ListSort::Newest,
        text => ListSort::parse(text).ok_or_else(|| {
            unknown("Sort must be newest, oldest, recently-updated, least-recently-updated, most-commented or least-commented.")
        })?,
    };
    Ok(ListFilter {
        state: None,
        include_merged: false,
        search: None,
        author_id,
        assignee,
        milestone,
        labels: &[],
        sort,
        offset: 0,
    })
}

fn list_milestone(summary: db::models::MilestoneSummary) -> view::Milestone {
    let mut milestone = milestone_view(summary);
    milestone.description = None;
    milestone
}

/// The choices of the list filters, and the values the query selected.
async fn list_filters(
    ctx: &PageContext,
    repo: &RepositoryData,
    milestones: Vec<db::models::MilestoneSummary>,
    selected_labels: Vec<String>,
) -> ResultPage<view::ListFilters> {
    let labels = db::labels::list_repo_labels(&ctx.state.pool, repo.record.id)
        .await
        .map_err(|_| unavailable(ctx))?
        .into_iter()
        .map(label)
        .collect();
    Ok(view::ListFilters {
        labels,
        selected_labels,
        assignees: db::assignees::list_assignable_usernames(&ctx.state.pool, repo.record.id)
            .await
            .map_err(|_| unavailable(ctx))?,
        assignee: query_value(ctx, "assignee"),
        milestones: milestones.into_iter().map(list_milestone).collect(),
        milestone: query_value(ctx, "milestone"),
        authors: db::discussions::list_repo_authors(&ctx.state.pool, repo.record.id)
            .await
            .map_err(|_| unavailable(ctx))?,
        author: query_value(ctx, "author"),
        sort: query_value(ctx, "sort"),
        ..Default::default()
    })
}

pub(crate) async fn native_index(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let mut ctx = PageContext::load(state, headers, &uri).await?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    // The milestones name each row and feed the milestone filter, so they are read once.
    let milestones = db::milestones::list_repo_milestones(&ctx.state.pool, repo.record.id, None)
        .await
        .map_err(|_| unavailable(&ctx))?;
    // The search box holds the filters as qualifiers (`state:closed label:bug`); they replace the
    // parameters the menus and links use, and the words left over are the search.
    let mut typed_search = view::parse_search(ctx.query.get("q").map(String::as_str).unwrap_or(""));
    if let Some(view::MilestoneFilter::Title(title)) = typed_search.milestone.as_mut() {
        if let Some(found) = milestones
            .iter()
            .find(|summary| summary.milestone.title.eq_ignore_ascii_case(title))
        {
            *title = found.milestone.number.to_string();
        }
    }
    typed_search.apply_to(&mut ctx.query);
    let search_labels = std::mem::take(&mut typed_search.labels);
    let filter = ctx.query.get("state").map(String::as_str).unwrap_or("open");
    if !matches!(filter, "open" | "closed" | "all") {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "State must be open, closed or all.",
        ));
    }
    let search = ctx
        .query
        .get("q")
        .map(String::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty());
    if search.is_some_and(|text| text.chars().count() > MAX_SEARCH_CHARS) {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "Search must be 200 characters or fewer.",
        ));
    }
    let state = if filter == "all" { None } else { Some(filter) };
    let narrowing = query_narrowing(&ctx, &repo).await?;
    // The page takes `labels=a,b` and any number of `label=` values, and the list keeps the items
    // that carry all of them.
    let typed = repeated(&raw_query_pairs(&uri), "label");
    let labels = filter_label_names(
        ctx.query.get("labels").map(String::as_str),
        typed
            .iter()
            .map(String::as_str)
            .chain(search_labels.iter().map(String::as_str)),
    )
    .map_err(|message| ctx.error(StatusCode::BAD_REQUEST, message))?;
    let page_number = if kind == Kind::Issue {
        chrome::number(&ctx, "page", 1, chrome::MAX_PAGE)?
    } else {
        1
    };
    let (offset, _) = chrome::page_window(page_number, 100)
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid issue page."))?;
    let list = ListFilter {
        offset: offset as i64,
        state,
        search,
        labels: &labels,
        ..narrowing
    };
    let named_milestones: HashMap<i64, view::Milestone> = milestones
        .iter()
        .map(|summary| (summary.milestone.id, list_milestone(summary.clone())))
        .collect();
    let mut filters = list_filters(&ctx, &repo, milestones, labels.clone()).await?;
    filters.page = page_number;
    let mut items = Vec::new();
    let totals;
    match kind {
        Kind::Issue => {
            totals = db::issues::count_repo_issues(&ctx.state.pool, repo.record.id, search)
                .await
                .map_err(|_| unavailable(&ctx))?;
            let mut entries =
                db::issues::list_repo_issues(&ctx.state.pool, repo.record.id, &list, 101)
                    .await
                    .map_err(|_| unavailable(&ctx))?;
            filters.has_more =
                chrome::has_next_page(page_number, entries.len(), 100, chrome::MAX_PAGE);
            entries.truncate(100);
            let ids = entries
                .iter()
                .map(|entry| entry.issue.id)
                .collect::<Vec<_>>();
            let mut labels = db::labels::get_issues_labels(&ctx.state.pool, &ids)
                .await
                .map_err(|_| unavailable(&ctx))?;
            let mut assignees = listed_assignees(&ctx, &ids, false).await?;
            let mut comments: HashMap<i64, i64> =
                db::issues::count_issue_comments(&ctx.state.pool, &ids)
                    .await
                    .map_err(|_| unavailable(&ctx))?
                    .into_iter()
                    .collect();
            for entry in entries {
                let id = entry.issue.id;
                let author = entry.author.username;
                let issue_labels = labels.remove(&id).unwrap_or_default();
                let issue_assignees = assignees.remove(&id).unwrap_or_default();
                let milestone = entry
                    .issue
                    .milestone_id
                    .and_then(|milestone| named_milestones.get(&milestone).cloned());
                let mut item = issue_item(entry.issue, author, issue_labels);
                item.assignees = issue_assignees;
                item.milestone = milestone;
                item.comment_count = comments.remove(&id).unwrap_or(0);
                items.push(item);
            }
        }
        Kind::Pull => {
            totals = db::pull_requests::records::count_repo_pull_requests(
                &ctx.state.pool,
                repo.record.id,
                search,
            )
            .await
            .map_err(|_| unavailable(&ctx))?;
            let pulls = db::pull_requests::records::list_repo_pull_requests(
                &ctx.state.pool,
                repo.record.id,
                &list,
                100,
            )
            .await
            .map_err(|_| unavailable(&ctx))?;
            let ids = pulls
                .iter()
                .map(|value| value.pull_request.id)
                .collect::<Vec<_>>();
            let mut labels = db::labels::get_pull_requests_labels(&ctx.state.pool, &ids)
                .await
                .map_err(|_| unavailable(&ctx))?;
            let mut assignees = listed_assignees(&ctx, &ids, true).await?;
            let mut comments: HashMap<i64, i64> =
                db::pull_requests::reviews::count_pull_request_comments_for(&ctx.state.pool, &ids)
                    .await
                    .map_err(|_| unavailable(&ctx))?
                    .into_iter()
                    .collect();
            let reviews =
                db::pull_requests::reviews::list_pull_request_review_states(&ctx.state.pool, &ids)
                    .await
                    .map_err(|_| unavailable(&ctx))?;
            let checks = db::ci::list_pull_request_check_statuses(&ctx.state.pool, &ids)
                .await
                .map_err(|_| unavailable(&ctx))?;
            let mut heads = HashMap::new();
            for value in pulls {
                let author = value.author.username;
                let head =
                    listed_head(&ctx, repo.record.id, &mut heads, value.head_repository).await?;
                let id = value.pull_request.id;
                let head_sha = value.pull_request.head_sha.clone();
                let milestone = value
                    .pull_request
                    .milestone_id
                    .and_then(|milestone| named_milestones.get(&milestone).cloned());
                let mut item = pull_item(&ctx, &repo, value.pull_request, author, head)
                    .map_err(|response| *response)?;
                item.labels = labels
                    .remove(&id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(label)
                    .collect();
                item.assignees = assignees.remove(&id).unwrap_or_default();
                item.milestone = milestone;
                item.comment_count = comments.remove(&id).unwrap_or(0);
                item.review_state = review_state(&reviews, id, &head_sha);
                item.check_state = check_state(&checks, id);
                items.push(item);
            }
        }
    }
    // The list shows titles, labels and assignees, not bodies. Bodies stay on the detail page
    // and in JSON.
    for item in &mut items {
        item.tasks = count_tasks(&item.body);
        item.body.clear();
    }
    Ok(ctx.page_model(
        kind.title(),
        page(view::Page::List {
            repository: repo.view,
            kind,
            state: filter.to_owned(),
            query: search.unwrap_or_default().to_owned(),
            counts: view::ListCounts {
                open: totals.open,
                closed: totals.closed,
                all: totals.all,
            },
            items,
            filters,
        }),
    ))
}

/// The review state a pull request row shows, from each reviewer's latest decision. A change
/// request counts on any commit. An approval counts only on the head commit, so a new push asks
/// for a fresh approval.
fn review_state(
    reviews: &[(i64, String, Option<String>)],
    pull_request: i64,
    head_sha: &str,
) -> Option<String> {
    let mut approved = false;
    for (_, state, commit) in reviews.iter().filter(|row| row.0 == pull_request) {
        match state.as_str() {
            "changes_requested" => return Some("changes_requested".to_owned()),
            "approved" if commit.as_deref() == Some(head_sha) => approved = true,
            _ => {}
        }
    }
    approved.then(|| "approved".to_owned())
}

/// The check state a pull request row shows, from the latest run of each workflow on its head
/// commit. A run that failed or was cancelled decides it first, then a run still going, and only
/// all successful runs make it a success. No runs leave the state out.
fn check_state(checks: &[(i64, String)], pull_request: i64) -> Option<String> {
    let statuses: Vec<&str> = checks
        .iter()
        .filter(|row| row.0 == pull_request)
        .map(|row| row.1.as_str())
        .collect();
    if statuses.is_empty() {
        return None;
    }
    if statuses
        .iter()
        .any(|status| !matches!(*status, "success" | "queued" | "running"))
    {
        return Some("failure".to_owned());
    }
    if statuses
        .iter()
        .any(|status| matches!(*status, "queued" | "running"))
    {
        return Some("pending".to_owned());
    }
    Some("success".to_owned())
}

/// The references of a new entry's body: its preview, and any form that re-renders it, link them.
/// The body is text the user submitted, so a failed lookup links nothing instead of failing the form.
/// A body renders as a comment, in its preview too, so it is resolved in comment mode.
async fn body_references(ctx: &PageContext, repo: &RepositoryData, body: &str) -> References {
    resolve_references_or_none(
        ctx,
        &repo.record,
        &repo.view.owner,
        &repo.view.name,
        &[body],
        false,
    )
    .await
}

/// The decoded name and value pairs of the address's query string, repeats included.
fn raw_query_pairs(uri: &Uri) -> Vec<(String, String)> {
    url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

/// The assignees, labels and milestone a writer can pick on a new issue. Readers and pull requests
/// get none.
async fn issue_choices(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
) -> ResultPage<view::IssueChoices> {
    if kind != Kind::Issue || !repo.view.can_write {
        return Ok(view::IssueChoices::default());
    }
    let assignable = db::assignees::list_assignable_usernames(&ctx.state.pool, repo.record.id)
        .await
        .map_err(|_| unavailable(ctx))?;
    // Labels are applied by administrators, as on the issue page, so writers are not offered them.
    let labels = if repo.view.can_admin {
        db::labels::list_repo_labels(&ctx.state.pool, repo.record.id)
            .await
            .map_err(|_| unavailable(ctx))?
            .into_iter()
            .map(label)
            .collect()
    } else {
        Vec::new()
    };
    let milestones = db::milestones::list_milestone_choices(&ctx.state.pool, repo.record.id)
        .await
        .map_err(|_| unavailable(ctx))?
        .into_iter()
        .map(milestone_choice_view)
        .collect();
    Ok(view::IssueChoices {
        assignable,
        labels,
        milestones,
    })
}

/// The picks on a new issue must come from the lists on its page, and no more assignees than one
/// issue may have.
fn issue_choices_valid(
    draft: &view::NewDraft,
    choices: &view::IssueChoices,
) -> Result<(), &'static str> {
    let assignees_ok = (draft.assignees.len() as i64) <= db::assignees::MAX_ASSIGNEES
        && draft
            .assignees
            .iter()
            .all(|name| choices.assignable.contains(name));
    let labels_ok = draft.labels.iter().all(|id| {
        choices
            .labels
            .iter()
            .any(|label| label.id.to_string() == *id)
    });
    let milestone_ok = draft.milestone.is_empty()
        || choices
            .milestones
            .iter()
            .any(|milestone| milestone.number.to_string() == draft.milestone);
    if assignees_ok && labels_ok && milestone_ok {
        Ok(())
    } else {
        Err("Choose assignees, labels and a milestone from the lists.")
    }
}

/// Puts the picks of a new issue on it through the store functions the issue page uses. Each one
/// checks the write right again. A user who is newly assigned hears about it.
async fn apply_issue_choices(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    assignment: &Assignment<'_>,
    issue_id: i64,
    draft: &view::NewDraft,
) -> anyhow::Result<()> {
    let actor_id = assignment.actor.id;
    let repo_id = assignment.repository.id;
    for username in &draft.assignees {
        if let Some(user) = db::users::accounts::get_user_by_username(pool, username).await? {
            let added = db::assignees::assign_user(
                pool,
                actor_id,
                repo_id,
                AssignedItem::Issue(issue_id),
                user.id,
            )
            .await?;
            if added {
                notify_assignees(pool, broadcaster, assignment, &[user.id]).await;
            }
        }
    }
    let labels = draft
        .labels
        .iter()
        .filter_map(|id| id.parse::<i64>().ok())
        .collect::<Vec<_>>();
    if !labels.is_empty() {
        db::labels::add_issue_labels(pool, actor_id, repo_id, issue_id, &labels).await?;
    }
    if let Ok(number) = draft.milestone.parse::<i32>() {
        db::milestones::set_issue_milestone(pool, actor_id, repo_id, issue_id, Some(number))
            .await?;
    }
    Ok(())
}

fn issue_choices_warning(draft: &view::NewDraft) -> String {
    format!("Issue created, but some details could not be applied. Check the sidebar and retry these selections: assignees [{}], label IDs [{}], milestone [{}].", draft.assignees.join(", "), draft.labels.join(", "), draft.milestone)
}

/// The repository's issue templates on its default branch.
async fn repository_issue_templates(
    ctx: &PageContext,
    repo: &RepositoryData,
) -> ResultPage<IssueTemplates> {
    let store = repo_store(&repo.record, ctx.state.cluster.as_ref());
    issue_templates(&store, &repo.record.default_branch)
        .await
        .map_err(|_| unavailable(ctx))
}

/// The fields of an issue form for the page, each with its answers when there are any.
fn form_fields(form: &[FormElement], answers: &[Vec<String>]) -> Vec<view::FormField> {
    form.iter()
        .enumerate()
        .map(|(index, element)| {
            let given = answers.get(index).cloned().unwrap_or_default();
            let values = match element.kind {
                // A text field starts with the file's text. A choice starts on its default option.
                FormKind::Input | FormKind::Textarea if given.is_empty() => {
                    vec![element.value.clone()]
                }
                FormKind::Dropdown if given.is_empty() && !element.multiple => element
                    .options
                    .get(element.default)
                    .map(|option| vec![option.label.clone()])
                    .unwrap_or_default(),
                _ => given,
            };
            view::FormField {
                kind: match element.kind {
                    FormKind::Markdown => view::FieldKind::Markdown,
                    FormKind::Input => view::FieldKind::Input,
                    FormKind::Textarea => view::FieldKind::Textarea,
                    FormKind::Dropdown => view::FieldKind::Dropdown,
                    FormKind::Checkboxes => view::FieldKind::Checkboxes,
                },
                label: element.label.clone(),
                description: element.description.clone(),
                placeholder: element.placeholder.clone(),
                text: if element.kind == FormKind::Markdown {
                    element.value.clone()
                } else {
                    String::new()
                },
                options: element
                    .options
                    .iter()
                    .map(|option| option.label.clone())
                    .collect(),
                multiple: element.multiple,
                required: element.required || element.options.iter().any(|option| option.required),
                values,
            }
        })
        .collect()
}

/// Fills a new issue's draft from its template. Text the draft already has stays. Assignees and
/// labels are taken only when the writer may pick them, by username and by label name.
fn apply_issue_template(
    draft: &mut view::NewDraft,
    template: &IssueTemplate,
    choices: &view::IssueChoices,
) {
    if draft.title.is_empty() {
        draft.title = template.title.clone();
    }
    if draft.body.is_empty() {
        draft.body = template.body.clone();
    }
    if !template.form.is_empty() {
        draft.template = template.path.clone();
        draft.template_revision = template.revision.clone();
        draft.form = form_fields(&template.form, &[]);
    }
    if draft.assignees.is_empty() {
        draft.assignees = template
            .assignees
            .iter()
            .filter(|name| choices.assignable.contains(name))
            .take(db::assignees::MAX_ASSIGNEES as usize)
            .cloned()
            .collect();
    }
    if draft.labels.is_empty() {
        draft.labels = template
            .labels
            .iter()
            .filter_map(|name| {
                choices
                    .labels
                    .iter()
                    .find(|label| label.name.eq_ignore_ascii_case(name))
            })
            .map(|label| label.id.to_string())
            .collect();
    }
}

const SAME_BRANCHES: &str = "Select two different, existing branches.";

pub(crate) async fn native_new_page(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    ctx.require_user()?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    let mut draft = submitted_new(&ctx.query);
    draft.repository_id = repo.record.id.to_string();
    let choices = issue_choices(&ctx, &repo, kind).await?;
    if kind == Kind::Issue {
        let found = repository_issue_templates(&ctx, &repo).await?;
        let picked = ctx.query.get("template").map(String::as_str);
        // Without a template choice, the chooser comes first. `template=` is the blank issue, and
        // it is refused when the repository's config turns blank issues off.
        let show_chooser = match picked {
            None => !found.templates.is_empty(),
            Some("") => !found.blank_allowed,
            Some(path) => found.templates.iter().all(|template| template.path != path),
        };
        if show_chooser && !found.templates.is_empty() {
            let templates = found
                .templates
                .iter()
                .map(|template| view::TemplateCard {
                    path: template.path.clone(),
                    name: template.name.clone(),
                    about: template.about.clone(),
                })
                .collect();
            return Ok(ctx.page_model(
                "New issue",
                page(view::Page::Choose {
                    repository: repo.view,
                    templates,
                    blank_allowed: found.blank_allowed,
                }),
            ));
        }
        if let Some(template) = picked.and_then(|path| {
            found
                .templates
                .iter()
                .find(|template| template.path == path)
        }) {
            apply_issue_template(&mut draft, template, &choices);
        }
    } else if draft.body.is_empty() {
        let store = repo_store(&repo.record, ctx.state.cluster.as_ref());
        if let Some(text) = pull_request_template(&store, &repo.record.default_branch)
            .await
            .map_err(|_| unavailable(&ctx))?
        {
            draft.body = text;
        }
    }
    let references = body_references(&ctx, &repo, &draft.body).await;
    let (branches, diff, error) = if kind == Kind::Pull {
        let (branches, head_repo, refs) = head_choices(&ctx, &repo, &mut draft).await?;
        if draft.base.is_empty() {
            draft.base = repo.record.default_branch.clone();
        }
        if draft.head.is_empty() {
            draft.head = branches
                .head
                .iter()
                .find(|b| head_repo.id != repo.record.id || *b != &draft.base)
                .cloned()
                .unwrap_or_default();
        }
        // The diff waits for a compare: the Compare button and the compare page's link both send
        // `intent=compare`. Naming one branch twice is refused here, not with a 400 page.
        let compared = ctx
            .query
            .get("intent")
            .is_some_and(|intent| intent == "compare");
        let same =
            head_repo.id == repo.record.id && !draft.head.is_empty() && draft.head == draft.base;
        if same {
            (branches, None, Some(SAME_BRANCHES.to_owned()))
        } else if compared && !draft.head.is_empty() && !draft.base.is_empty() {
            let diff = comparison(&ctx, &repo, &head_repo, &refs, &draft).await?;
            (branches, Some(diff), None)
        } else {
            (branches, None, None)
        }
    } else {
        (view::BranchChoices::default(), None, None)
    };
    Ok(ctx.page_model(
        &format!("New {}", kind.singular()),
        page(view::Page::New {
            repository: repo.view,
            kind,
            draft,
            branches,
            choices,
            diff,
            error,
            preview: false,
            references,
        }),
    ))
}

pub(crate) async fn native_create(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
    // The pairs keep repeated fields, such as the assignees and labels checked on a new issue.
    Form(pairs): Form<Vec<(String, String)>>,
) -> ResultPage<Response> {
    let mut ctx = PageContext::load(state, headers, &uri).await?;
    let form: HashMap<String, String> = pairs.iter().cloned().collect();
    authorize_post(&ctx, &form)?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    let mut draft = submitted_new(&form);
    let mut submission_error = if kind == Kind::Issue
        && draft.repository_id != repo.record.id.to_string()
    {
        Some("The repository changed. Your draft has not been published; review its destination before trying again.".to_owned())
    } else {
        None
    };
    draft.assignees = repeated(&pairs, "assignees");
    draft.labels = repeated(&pairs, "labels");
    draft.milestone = field(&form, "milestone");
    let choices = issue_choices(&ctx, &repo, kind).await?;
    // An issue form writes its answers into the body.
    let mut missing_answer = None;
    let form_path = field(&form, "template");
    if kind == Kind::Issue && !form_path.is_empty() {
        let found = repository_issue_templates(&ctx, &repo).await?;
        if let Some(template) = found.templates.iter().find(|template| {
            template.path == form_path
                && !template.form.is_empty()
                && template.revision == draft.template_revision
        }) {
            let answers: Vec<Vec<String>> = (0..template.form.len())
                .map(|index| repeated(&pairs, &format!("form_{index}")))
                .collect();
            missing_answer = first_missing_answer(&template.form, &answers).map(str::to_owned);
            draft.body = compose_form_body(&template.form, &answers);
            draft.form = form_fields(&template.form, &answers);
            draft.template = form_path;
        } else {
            draft.body = pairs
                .iter()
                .filter(|(name, _)| name.starts_with("form_"))
                .map(|(name, value)| format!("{name}: {value}"))
                .collect::<Vec<_>>()
                .join("\n\n");
            submission_error = Some("The issue template changed or was removed. Your answers are preserved below; review them before creating the issue.".to_owned());
            draft.template.clear();
            draft.template_revision.clear();
        }
    }
    let references = body_references(&ctx, &repo, &draft.body).await;
    let (branches, head_repo, refs) = if kind == Kind::Pull {
        head_choices(&ctx, &repo, &mut draft).await?
    } else {
        (
            view::BranchChoices::default(),
            repo.record.clone(),
            HashMap::new(),
        )
    };
    let intent = field(&form, "intent");
    // A new head repository brings its own branches. Compare then starts from the first of them,
    // and the page shows which branch it compared.
    if kind == Kind::Pull && intent == "compare" && !branches.head.contains(&draft.head) {
        draft.head = branches.head.first().cloned().unwrap_or_default();
    }
    let compare_only = kind == Kind::Pull && intent == "compare";
    let validation = (if compare_only && draft.title.is_empty() {
        Ok(())
    } else {
        crate::backend::issues::validate_title(&draft.title)
    })
    .and_then(|_| crate::backend::issues::validate_body(&draft.body, false))
    .and_then(|_| issue_choices_valid(&draft, &choices))
    .and_then(|_| {
        if kind == Kind::Pull
            && ((head_repo.id == repo.record.id && draft.head == draft.base)
                || !branches.head.contains(&draft.head)
                || !branches.base.contains(&draft.base))
        {
            Err(SAME_BRANCHES)
        } else {
            Ok(())
        }
    })
    .map_err(str::to_owned)
    .and_then(|()| match &submission_error {
        Some(message) => Err(message.clone()),
        None => Ok(()),
    })
    .and_then(|()| match &missing_answer {
        Some(label) if intent == "create" => Err(format!("{label} is required.")),
        _ => Ok(()),
    });
    let preview = field(&form, "intent") == "preview";
    if let Err(message) = validation {
        return Ok(with_status(
            ctx.page_model(
                "Check your submission",
                page(view::Page::New {
                    repository: repo.view,
                    kind,
                    draft,
                    branches,
                    choices,
                    diff: None,
                    error: Some(message.to_owned()),
                    preview,
                    references,
                }),
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
        ));
    }
    if compare_only {
        return match comparison(&ctx, &repo, &head_repo, &refs, &draft).await {
            Ok(diff) => Ok(ctx.page_model(
                "Branch comparison",
                page(view::Page::New {
                    repository: repo.view,
                    kind,
                    draft,
                    branches,
                    choices,
                    diff: Some(diff),
                    error: None,
                    preview: false,
                    references,
                }),
            )),
            Err(response) => Ok(with_status(
                ctx.page_model(
                    "Comparison unavailable",
                    page(view::Page::New {
                        repository: repo.view,
                        kind,
                        draft,
                        branches,
                        choices,
                        diff: None,
                        error: Some(
                            "Git could not load the selected comparison. Your draft is preserved."
                                .to_owned(),
                        ),
                        preview: false,
                        references,
                    }),
                ),
                response.status(),
            )),
        };
    }
    if preview {
        return Ok(ctx.page_model(
            "Preview",
            page(view::Page::New {
                repository: repo.view,
                kind,
                draft,
                branches,
                choices,
                diff: None,
                error: None,
                preview: true,
                references,
            }),
        ));
    }
    if intent != "create" {
        return Ok(with_status(
            ctx.page_model(
                "Check your submission",
                page(view::Page::New {
                    repository: repo.view,
                    kind,
                    draft,
                    branches,
                    choices,
                    diff: None,
                    error: Some("Choose Create, Preview or Compare.".to_owned()),
                    preview: false,
                    references,
                }),
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
        ));
    }
    let result: Result<i32, ApiFailure> = match kind {
        Kind::Issue => {
            let actor = ctx.require_user()?;
            let created = crate::backend::issues::mutations::create_issue(
                &ctx.state.pool,
                &ctx.state.notification_broadcaster,
                actor.id,
                owner.clone(),
                name.clone(),
                crate::backend::issues::mutations::CreateIssueRequest {
                    title: draft.title.clone(),
                    body: Some(draft.body.clone()),
                    repository_id: Some(repo.record.id),
                },
            )
            .await
            .map_err(crate::backend::api::repos::issues::operation_error);
            // The issue exists now, so a failure here cannot undo it. The redirect opens the issue,
            // where the missing assignees, labels or milestone can be added again.
            if let Ok(issue) = &created {
                let link = format!("/{owner}/{name}/issues/{}", issue.number);
                let assignment = Assignment {
                    actor,
                    repository: &repo.record,
                    owner: &owner,
                    repo: &name,
                    number: issue.number,
                    pull_request: false,
                    title: &issue.title,
                    link: &link,
                };
                if let Err(error) = apply_issue_choices(
                    &ctx.state.pool,
                    &ctx.state.notification_broadcaster,
                    &assignment,
                    issue.id,
                    &draft,
                )
                .await
                {
                    tracing::warn!(number = issue.number, %error, "a new issue lost its assignees, labels or milestone");
                    let warning = issue_choices_warning(&draft);
                    ctx.location = link.clone();
                    ctx.path = link;
                    let value = item(&ctx, &repo, kind, issue.number).await?;
                    return render_detail(
                        &ctx,
                        &repo,
                        kind,
                        &value,
                        &view::DetailDraft::default(),
                        Some(&warning),
                        StatusCode::OK,
                    )
                    .await;
                }
            }
            created.map(|value| value.number)
        }
        Kind::Pull => crate::backend::repositories::pull_requests::mutations::create_pull_request(
            &ctx.state.pool,
            &ctx.state.ci_runs,
            &ctx.state.notification_broadcaster,
            ctx.require_user()?.id,
            owner,
            name,
            crate::backend::repositories::pull_requests::mutations::CreatePullRequestRequest {
                head_repo_id: Some(head_repo.id),
                title: draft.title.clone(),
                body: Some(draft.body.clone()),
                head_branch: draft.head.clone(),
                base_branch: draft.base.clone(),
                is_draft: draft.draft,
            },
        )
        .await
        .map_err(crate::backend::api::repos::pulls::operation_error)
        .map(|value| value.number),
    };
    match result {
        Ok(number) => Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(kind), number))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            Ok(with_status(
                ctx.page_model(
                    "Submission failed",
                    page(view::Page::New {
                        repository: repo.view,
                        kind,
                        draft,
                        branches,
                        choices,
                        diff: None,
                        error: Some(message),
                        preview: false,
                        references,
                    }),
                ),
                status,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{check_state, issue_choices_warning, review_state};

    #[test]
    fn partial_creation_warning_keeps_the_selections_for_retry() {
        let draft = crate::webui::pages::repos::issues::NewDraft {
            assignees: vec!["alice".into(), "bob".into()],
            labels: vec!["10".into(), "20".into()],
            milestone: "3".into(),
            ..Default::default()
        };
        let warning = issue_choices_warning(&draft);
        for text in [
            "Issue created",
            "could not be applied",
            "alice, bob",
            "10, 20",
            "milestone [3]",
        ] {
            assert!(warning.contains(text));
        }
    }

    #[test]
    fn a_change_request_counts_on_any_commit_and_an_approval_only_on_the_head() {
        let reviews = vec![
            (7, "approved".to_owned(), Some("old".to_owned())),
            (7, "changes_requested".to_owned(), Some("old".to_owned())),
            (8, "approved".to_owned(), Some("head".to_owned())),
        ];
        assert_eq!(
            review_state(&reviews, 7, "head").as_deref(),
            Some("changes_requested")
        );
        assert_eq!(
            review_state(&reviews, 8, "head").as_deref(),
            Some("approved")
        );
        let stale = vec![(7, "approved".to_owned(), Some("old".to_owned()))];
        assert_eq!(review_state(&stale, 7, "head"), None);
    }

    #[test]
    fn a_failed_run_decides_the_checks_state_before_a_running_one() {
        let checks = vec![
            (7, "success".to_owned()),
            (7, "running".to_owned()),
            (8, "success".to_owned()),
        ];
        assert_eq!(check_state(&checks, 7).as_deref(), Some("pending"));
        assert_eq!(check_state(&checks, 8).as_deref(), Some("success"));
        assert_eq!(check_state(&checks, 9), None);
        let failed = vec![(7, "running".to_owned()), (7, "failed".to_owned())];
        assert_eq!(check_state(&failed, 7).as_deref(), Some("failure"));
    }
}
