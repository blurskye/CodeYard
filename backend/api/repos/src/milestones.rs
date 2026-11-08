//! Milestones: the list and page of a repository's milestones, their plain forms, and the
//! milestone an issue or pull request is given. Writers manage milestones; the database checks that
//! again.
use crate::discussion::api_error;
use crate::discussion::detail_outcome;
use crate::discussion::item;
use crate::discussion::kind;
use crate::discussion::label;
use crate::discussion::listed_assignees;
use crate::discussion::listed_head;
use crate::discussion::pull_item;
use crate::discussion::repository;
use crate::discussion::safe_api_error;
use crate::discussion::unavailable;
use crate::discussion::RepositoryData;
use crate::discussion::ResultPage;
use crate::issues::assignees::assigned_item;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::error::ApiFailure;
use codeyard_api_http::shared::repos::discussion::page;
use codeyard_api_http::shared::repos::forms::authorize_post;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_api_http::shared::repos::forms::with_status;
use codeyard_app::app::AppState;
use codeyard_database::models::MilestoneSummary;
use codeyard_db_repos::assignees::AssignedItem;
use codeyard_db_repos::discussions::ListFilter;
use codeyard_db_repos::discussions::Pick;
use codeyard_db_repos::milestones::MilestoneWriteRefusal;
use codeyard_types::issue_input::parse_due_date;
use codeyard_types::issue_input::validate_milestone;
use codeyard_webui::pages::repos::issues as view;
use codeyard_webui::pages::repos::issues::ListCounts;
use codeyard_webui::pages::repos::issues::MilestoneDraft;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) fn milestone_view(summary: MilestoneSummary) -> view::Milestone {
    view::Milestone {
        id: summary.milestone.id,
        number: summary.milestone.number,
        title: summary.milestone.title,
        description: summary.milestone.description,
        due_date: summary.milestone.due_date,
        state: summary.milestone.state,
        open_items: summary.open_items,
        closed_items: summary.closed_items,
    }
}

/// Picker choices do not show progress counts.
pub(crate) fn milestone_choice_view(
    milestone: codeyard_database::models::Milestone,
) -> view::Milestone {
    milestone_view(MilestoneSummary {
        milestone,
        open_items: 0,
        closed_items: 0,
    })
}

fn draft_of(milestone: &view::Milestone) -> MilestoneDraft {
    MilestoneDraft {
        repository_id: None,
        original: serde_json::to_string(&(
            &milestone.title,
            &milestone.description,
            &milestone.due_date,
            &milestone.state,
        ))
        .unwrap_or_default(),
        title: milestone.title.clone(),
        description: milestone.description.clone().unwrap_or_default(),
        due_date: milestone.due_date.clone().unwrap_or_default(),
    }
}

pub(crate) fn milestone_write_error(error: anyhow::Error) -> ApiFailure {
    let (status, code, message) = match error.downcast_ref::<MilestoneWriteRefusal>() {
        Some(MilestoneWriteRefusal::Denied) => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "You cannot manage the milestones of this repository.",
        ),
        Some(MilestoneWriteRefusal::NotFound) => (
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested resource was not found or you do not have access.",
        ),
        Some(MilestoneWriteRefusal::Conflict) => (
            StatusCode::CONFLICT,
            "milestone_changed",
            "The milestone changed. Your draft is preserved; reload before saving.",
        ),
        Some(MilestoneWriteRefusal::DuplicateTitle) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "milestone_title_taken",
            "A milestone with this title already exists.",
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "Milestone storage unavailable",
        ),
    };
    (status, Json(ApiError::new(code, message)))
}

/// A milestone form as typed.
fn submitted_milestone(form: &HashMap<String, String>) -> MilestoneDraft {
    MilestoneDraft {
        repository_id: form.get("repository_id").cloned(),
        original: field(form, "original"),
        title: field(form, "title"),
        description: field(form, "description"),
        due_date: field(form, "due_date"),
    }
}

/// The checked fields of a milestone form, as they are stored.
struct MilestoneInput {
    title: String,
    description: Option<String>,
    due_date: Option<String>,
}

fn milestone_input(draft: &MilestoneDraft) -> Result<MilestoneInput, ApiFailure> {
    let invalid = |message: &str| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError::new("validation_error", message)),
        )
    };
    let title = draft.title.trim().to_owned();
    validate_milestone(&title, &draft.description).map_err(invalid)?;
    let due_date = parse_due_date(draft.due_date.trim()).map_err(invalid)?;
    let description = (!draft.description.trim().is_empty()).then(|| draft.description.clone());
    Ok(MilestoneInput {
        title,
        description,
        due_date,
    })
}

/// The state tab of a list or a milestone page, from its query. A bad value is a bad request.
#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
fn state_tab<'a>(ctx: &'a PageContext, allowed: &[&str]) -> Result<&'a str, Response> {
    let value = ctx.query.get("state").map(String::as_str).unwrap_or("open");
    if allowed.contains(&value) {
        Ok(value)
    } else {
        Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "State must be open, closed or all.",
        ))
    }
}

/// The milestones of a repository on its list page, one tab at a time, with the open and
/// closed counts.
pub(crate) async fn native_index(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    let repo = repository(&ctx, &owner, &name).await?;
    let tab = state_tab(&ctx, &["open", "closed"])?;
    let (open, closed) =
        codeyard_db_repos::milestones::count_milestones(&ctx.state.pool, repo.record.id)
            .await
            .map_err(|_| unavailable(&ctx))?;
    let counts = ListCounts { open, closed };
    let sort = ctx.query.get("sort").cloned().unwrap_or_default();
    let page_number = crate::paging::number(&ctx, "page", 1, 1..=crate::paging::MAX_PAGE)?;
    let offset = (page_number - 1) as i64 * codeyard_db_repos::milestones::MILESTONES_PER_PAGE;
    let items = codeyard_db_repos::milestones::list_milestones_page(
        &ctx.state.pool,
        repo.record.id,
        Some(tab),
        &sort,
        codeyard_db_repos::milestones::MILESTONES_PER_PAGE,
        offset,
    )
    .await
    .map_err(|_| unavailable(&ctx))?
    .into_iter()
    .map(milestone_view)
    .collect();
    Ok(ctx.page_model(
        "Milestones",
        page(view::Page::Milestones {
            repository: repo.view,
            state: tab.to_owned(),
            counts,
            items,
            sort,
            page_number,
            has_next: page_number < crate::paging::MAX_PAGE
                && offset + codeyard_db_repos::milestones::MILESTONES_PER_PAGE
                    < if tab == "open" { open } else { closed },
        }),
    ))
}

/// Refuses the page to anyone who cannot manage milestones.
#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
fn require_writer(ctx: &PageContext, repo: &RepositoryData) -> ResultPage<()> {
    ctx.require_user()?;
    if repo.view.can_write {
        Ok(())
    } else {
        Err(ctx.error(
            StatusCode::FORBIDDEN,
            "You do not have permission to manage milestones here.",
        ))
    }
}

pub(crate) async fn native_new_page(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    let repo = repository(&ctx, &owner, &name).await?;
    require_writer(&ctx, &repo)?;
    Ok(ctx.page_model(
        "New milestone",
        page(view::Page::MilestoneNew {
            repository: repo.view,
            draft: MilestoneDraft::default(),
            error: None,
        }),
    ))
}

/// The new-milestone page again, with the typed fields and the reason the form was refused.
fn new_form_refused(
    ctx: &PageContext,
    repo: &RepositoryData,
    draft: &MilestoneDraft,
    failure: &ApiFailure,
) -> Response {
    let (status, message) = safe_api_error(failure);
    with_status(
        ctx.page_model(
            "New milestone",
            page(view::Page::MilestoneNew {
                repository: repo.view.clone(),
                draft: draft.clone(),
                error: Some(message),
            }),
        ),
        status,
    )
}

pub(crate) async fn native_create(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    require_writer(&ctx, &repo)?;
    let draft = submitted_milestone(&form);
    if form
        .get("repository_id")
        .and_then(|id| id.parse::<i64>().ok())
        != Some(repo.record.id)
    {
        return Ok(new_form_refused(
            &ctx,
            &repo,
            &draft,
            &(
                StatusCode::CONFLICT,
                Json(ApiError::new(
                    "stale_target",
                    "The repository changed. Your draft is preserved; reload before submitting.",
                )),
            ),
        ));
    }
    let input = match milestone_input(&draft) {
        Ok(input) => input,
        Err(failure) => return Ok(new_form_refused(&ctx, &repo, &draft, &failure)),
    };
    let actor = ctx.database_actor(ctx.require_user()?.id);
    match codeyard_db_repos::milestones::create_milestone(
        &ctx.state.pool,
        actor,
        repo.record.id,
        &input.title,
        input.description.as_deref(),
        input.due_date.as_deref(),
    )
    .await
    {
        Ok(milestone) => Ok(ctx.redirect(&format!(
            "{}/milestones/{}",
            repo.view.url(),
            milestone.number
        ))),
        Err(error) => Ok(new_form_refused(
            &ctx,
            &repo,
            &draft,
            &milestone_write_error(error),
        )),
    }
}

/// A milestone's page: its progress, its edit form and its issues in one state tab. `refused`
/// is the edit form as submitted, with the reason it was refused.
async fn milestone_page(
    ctx: &PageContext,
    repo: &RepositoryData,
    number: i32,
    tab: &str,
    refused: Option<(MilestoneDraft, ApiFailure)>,
) -> ResultPage<Response> {
    let summary =
        codeyard_db_repos::milestones::get_milestone(&ctx.state.pool, repo.record.id, number)
            .await
            .map_err(|_| unavailable(ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Milestone not found."))?;
    let milestone_id = summary.milestone.id;
    let milestone = milestone_view(summary);
    let page_number = crate::paging::number(ctx, "page", 1, 1..=crate::paging::MAX_PAGE)?;
    let (offset, limit) = crate::paging::page_window(page_number, 30)
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid page."))?;
    let issue_state = match tab {
        "all" => None,
        open_or_closed => Some(open_or_closed),
    };
    let mut entries = codeyard_db_repos::issues::list_repo_issues(
        &ctx.state.pool,
        repo.record.id,
        &ListFilter {
            state: issue_state,
            milestone: Pick::Is(milestone_id),
            offset: offset as i64,
            ..Default::default()
        },
        (limit + 1) as i64,
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    let more_issues = entries.len() > limit;
    entries.truncate(limit);
    let ids = entries
        .iter()
        .map(|entry| entry.issue.id)
        .collect::<Vec<_>>();
    let mut labels =
        codeyard_db_repos::labels::get_issues_labels(&ctx.state.pool, repo.record.id, &ids)
            .await
            .map_err(|_| unavailable(ctx))?;
    let mut assignees = listed_assignees(ctx, &ids, false).await?;
    let mut items = Vec::new();
    for entry in entries {
        let author = entry.author.username;
        let issue_labels = labels.remove(&entry.issue.id).unwrap_or_default();
        let issue_assignees = assignees.remove(&entry.issue.id).unwrap_or_default();
        let mut row = crate::discussion::issue_item(entry.issue, author, issue_labels);
        row.assignees = issue_assignees;
        // The rows show titles, labels and assignees only, as the issue list does.
        row.body.clear();
        items.push(row);
    }
    // Filter before the limit; merged pull requests belong to the closed tab.
    let mut pulls = codeyard_db_repos::pull_requests::records::list_repo_pull_requests(
        &ctx.state.pool,
        repo.record.id,
        &ListFilter {
            state: issue_state,
            include_merged: true,
            milestone: Pick::Is(milestone_id),
            offset: offset as i64,
            ..Default::default()
        },
        (limit + 1) as i64,
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    let has_next = (more_issues || pulls.len() > limit) && page_number < crate::paging::MAX_PAGE;
    pulls.truncate(limit);
    let pull_ids = pulls
        .iter()
        .map(|value| value.pull_request.id)
        .collect::<Vec<_>>();
    let mut pull_labels = codeyard_db_repos::labels::get_pull_requests_labels(
        &ctx.state.pool,
        repo.record.id,
        &pull_ids,
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    let mut pull_assignees = listed_assignees(ctx, &pull_ids, true).await?;
    let mut pull_items = Vec::new();
    let mut heads = HashMap::new();
    for value in pulls {
        let author = value.author.username;
        let head = listed_head(ctx, repo.record.id, &mut heads, value.head_repository).await?;
        let id = value.pull_request.id;
        let mut row =
            pull_item(ctx, repo, value.pull_request, author, head).map_err(|response| *response)?;
        row.labels = pull_labels
            .remove(&id)
            .unwrap_or_default()
            .into_iter()
            .map(label)
            .collect();
        row.assignees = pull_assignees.remove(&id).unwrap_or_default();
        row.body.clear();
        pull_items.push(row);
    }
    let title = milestone.title.clone();
    let draft = refused
        .as_ref()
        .map(|(draft, _)| draft.clone())
        .unwrap_or_else(|| draft_of(&milestone));
    let (error, status) = match &refused {
        Some((_, failure)) => {
            let (status, message) = safe_api_error(failure);
            (Some(message), status)
        }
        None => (None, StatusCode::OK),
    };
    // A failed form is shown at its POST address; appearance changes return to the milestone.
    let mut ctx = ctx.clone();
    if refused.is_some() {
        ctx.return_to = Some(format!("{}/milestones/{number}", repo.view.url()));
    }
    Ok(with_status(
        ctx.page_model(
            &title,
            page(view::Page::MilestoneDetail {
                repository: repo.view.clone(),
                milestone,
                state: tab.to_owned(),
                items,
                pull_requests: pull_items,
                pagination: (page_number, has_next),
                draft,
                error,
            }),
        ),
        status,
    ))
}

pub(crate) async fn native_detail(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    let repo = repository(&ctx, &owner, &name).await?;
    let tab = state_tab(&ctx, &["open", "closed", "all"])?;
    milestone_page(&ctx, &repo, number, tab, None).await
}

/// A milestone form names the repository it was rendered for. After a rename, another repository
/// can take the old address, and a stale form must not reach it.
fn require_current_repository(
    ctx: &PageContext,
    form: &HashMap<String, String>,
    repo: &RepositoryData,
) -> ResultPage<()> {
    if field(form, "repository_id").parse::<i64>().ok() != Some(repo.record.id) {
        return Err(ctx.error(
            StatusCode::CONFLICT,
            "The repository changed. Reload the page and try again.",
        ));
    }
    Ok(())
}

pub(crate) async fn native_update(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    require_writer(&ctx, &repo)?;
    require_current_repository(&ctx, &form, &repo)?;
    let draft = submitted_milestone(&form);
    let input = match milestone_input(&draft) {
        Ok(input) => input,
        Err(failure) => {
            return milestone_page(&ctx, &repo, number, "open", Some((draft, failure))).await
        }
    };
    let actor = ctx.database_actor(ctx.require_user()?.id);
    match codeyard_db_repos::milestones::save_milestone_checked(
        &ctx.state.pool,
        actor,
        repo.record.id,
        Some(number),
        codeyard_db_repos::milestones::MilestoneInput {
            title: Some(&input.title),
            description: Some(input.description.as_deref()),
            due_on: Some(
                codeyard_db_repos::milestones::due_on(input.due_date.as_deref())
                    .map_err(|_| unavailable(&ctx))?,
            ),
            state: None,
        },
        Some(&draft.original),
    )
    .await
    {
        Ok(milestone) => Ok(ctx.redirect(&format!(
            "{}/milestones/{}",
            repo.view.url(),
            milestone.number
        ))),
        Err(error) => {
            milestone_page(
                &ctx,
                &repo,
                number,
                "open",
                Some((draft, milestone_write_error(error))),
            )
            .await
        }
    }
}

async fn change_state(
    ctx: &PageContext,
    repo: &RepositoryData,
    number: i32,
    state: &str,
) -> ResultPage<Response> {
    let actor = ctx.database_actor(ctx.require_user()?.id);
    match codeyard_db_repos::milestones::set_milestone_state(
        &ctx.state.pool,
        actor,
        repo.record.id,
        number,
        state,
    )
    .await
    {
        Ok(_) => Ok(ctx.redirect(&format!("{}/milestones/{}", repo.view.url(), number))),
        Err(error) => Err(api_error(ctx, milestone_write_error(error))),
    }
}

pub(crate) async fn native_close(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    require_writer(&ctx, &repo)?;
    require_current_repository(&ctx, &form, &repo)?;
    change_state(&ctx, &repo, number, "closed").await
}

pub(crate) async fn native_reopen(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    require_writer(&ctx, &repo)?;
    require_current_repository(&ctx, &form, &repo)?;
    change_state(&ctx, &repo, number, "open").await
}

pub(crate) async fn native_delete(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    require_writer(&ctx, &repo)?;
    if field(&form, "confirm") != "yes" {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "Confirm the deletion before the milestone is deleted.",
        ));
    }
    let actor = ctx.database_actor(ctx.require_user()?.id);
    let expected_id = field(&form, "milestone_id").parse::<i64>().unwrap_or(0);
    require_current_repository(&ctx, &form, &repo)?;
    match codeyard_db_repos::milestones::delete_milestone(
        &ctx.state.pool,
        actor,
        repo.record.id,
        number,
        expected_id,
    )
    .await
    {
        Ok(()) => Ok(ctx.redirect(&format!("{}/milestones", repo.view.url()))),
        Err(error) => Err(api_error(&ctx, milestone_write_error(error))),
    }
}

/// Sets or clears the milestone of an issue or pull request from its sidebar. An empty choice
/// clears it.
pub(crate) async fn native_set_item_milestone(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, kind, number).await?;
    crate::discussion::require_item_binding(&ctx, &form, repo.record.id, value.id)?;
    let subject = assigned_item(&ctx, &repo, kind, number).await?;
    let choice = field(&form, "milestone_number");
    let chosen = match choice.trim() {
        "" => None,
        text => match text.parse::<i32>() {
            Ok(milestone) => Some(milestone),
            Err(_) => {
                return detail_outcome(
                    &ctx,
                    &repo,
                    kind,
                    &value,
                    number,
                    Err((
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(ApiError::new(
                            "validation_error",
                            "Choose a milestone belonging to this repository.",
                        )),
                    )),
                )
                .await
            }
        },
    };
    let actor = ctx.database_actor(ctx.require_user()?.id);
    let outcome = match subject {
        AssignedItem::Issue(item_id) => {
            codeyard_db_repos::milestones::set_issue_milestone(
                &ctx.state.pool,
                actor,
                repo.record.id,
                item_id,
                chosen,
            )
            .await
        }
        AssignedItem::PullRequest(item_id) => {
            codeyard_db_repos::milestones::set_pull_request_milestone(
                &ctx.state.pool,
                actor,
                repo.record.id,
                item_id,
                chosen,
            )
            .await
        }
    }
    .map_err(milestone_write_error);
    detail_outcome(&ctx, &repo, kind, &value, number, outcome).await
}

pub fn page_router() -> axum::Router<std::sync::Arc<codeyard_app::app::AppState>> {
    use axum::{
        routing::{get, post},
        Router,
    };
    Router::new()
        .cell_route("/:owner/:repo/milestones", get(native_index))
        .cell_route(
            "/:owner/:repo/milestones/new",
            get(native_new_page).post(native_create),
        )
        .cell_route("/:owner/:repo/milestones/:number", get(native_detail))
        .cell_route("/:owner/:repo/milestones/:number/edit", post(native_update))
        .cell_route("/:owner/:repo/milestones/:number/close", post(native_close))
        .cell_route(
            "/:owner/:repo/milestones/:number/reopen",
            post(native_reopen),
        )
        .cell_route(
            "/:owner/:repo/milestones/:number/delete",
            post(native_delete),
        )
        .cell_route(
            "/:owner/:repo/issues/:number/milestone",
            post(native_set_item_milestone),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/milestone",
            post(native_set_item_milestone),
        )
}
