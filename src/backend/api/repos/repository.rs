use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::{repository_settings_error, ApiError};
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::{
    check_repo_admin_access, check_repo_read_access, check_repo_write_access,
};
use crate::backend::api::repos::chrome::ApiResult;
use crate::backend::api::repos::chrome::{
    git_unavailable_response, load_metadata, repository_default_branch_or_db, safe_error,
    unavailable, Loaded,
};
use crate::backend::api::repos::code::contents::{
    delete_form, replaced_repository_editor, write_form,
};
use crate::backend::api::repos::responses::{
    repository_owner, repository_topics, ForkParent, RepoResponse,
};
use crate::backend::api::repos::settings::branch_protection;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::lifecycle;
use crate::backend::webhooks::{panel, settings::HookError};
use crate::webui::pages::repos as view;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::{Form, Json};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const REPO_ADMIN_REQUIRED: &str = "Only repository administrators can manage these settings.";

pub async fn get_repo(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<RepoResponse>, (StatusCode, Json<ApiError>)> {
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
    repository_response(&state, &headers, &repository)
        .await
        .map(Json)
}

/// The repository as GET shows it. Mutations build their responses from the row they
/// persisted through this same function, so every field matches GET.
async fn repository_response(
    state: &AppState,
    headers: &HeaderMap,
    repository: &db::models::Repository,
) -> Result<RepoResponse, (StatusCode, Json<ApiError>)> {
    let owner = repository_owner(&state.pool, repository).await?;
    let default_branch = repository_default_branch_or_db(state, repository).await?;
    let topics = repository_topics(&state.pool, &[repository.id])
        .await?
        .remove(&repository.id)
        .unwrap_or_default();

    let forked_from = if let Some(parent_id) = repository.forked_from_id {
        match db::repositories::records::get_repository_by_id(&state.pool, parent_id)
            .await
            .map_err(|_| {
                crate::backend::api::repos::access::access_error(
                    crate::backend::repositories::access::AccessError::Unavailable,
                )
            })? {
            Some(parent) => match check_repo_read_access(&state.pool, &parent, headers).await {
                Ok(()) => {
                    let parent_owner = repository_owner(&state.pool, &parent).await?;
                    Some(ForkParent {
                        id: parent.id,
                        owner: parent_owner.clone(),
                        name: parent.name.clone(),
                        full_name: format!("{}/{}", parent_owner, parent.name),
                    })
                }
                Err((StatusCode::NOT_FOUND, _)) => None,
                Err(error) => return Err(error),
            },
            None => None,
        }
    } else {
        None
    };

    Ok(RepoResponse {
        id: repository.id,
        owner_id: repository.owner_id(),
        owner: owner.clone(),
        name: repository.name.clone(),
        full_name: format!("{}/{}", owner, repository.name),
        description: repository.description.clone(),
        website: repository.website.clone(),
        topics,
        is_private: repository.is_private,
        is_archived: repository.is_archived,
        allow_merge_commit: repository.allow_merge_commit,
        allow_squash_merge: repository.allow_squash_merge,
        allow_rebase_merge: repository.allow_rebase_merge,
        default_branch,
        stars_count: repository.stars_count,
        forks_count: repository.forks_count,
        watchers_count: repository.watchers_count,
        forked_from,
        created_at: repository.created_at.to_rfc3339(),
        updated_at: repository.updated_at.to_rfc3339(),
    })
}

#[derive(Deserialize)]
pub struct RenameRepoRequest {
    pub new_name: String,
}

#[derive(Deserialize)]
pub struct UpdateRepoRequest {
    pub description: Option<String>,
    pub is_private: bool,
    pub allow_merge_commit: Option<bool>,
    pub allow_squash_merge: Option<bool>,
    pub allow_rebase_merge: Option<bool>,
}

pub async fn rename_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<RenameRepoRequest>,
) -> Result<Json<RepoResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;

    if req.new_name.is_empty() || req.new_name.len() > 100 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Repository name must be 1-100 characters",
            )),
        ));
    }

    if !req
        .new_name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Repository name can only contain letters, numbers, hyphens, underscores, and dots",
            )),
        ));
    }

    if req.new_name == "." || req.new_name == ".." {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("validation_error", "Invalid repository name")),
        ));
    }

    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(repository_settings_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_admin_access(&state.pool, &repository, &headers).await?;

    // Unique owner/name reservation is authoritative. A rename only changes the
    // DB alias; stable operation-owned storage is never moved or compensated.
    let updated = db::repositories::records::rename_repository(
        &state.pool,
        user.id,
        repository.id,
        &req.new_name,
    )
    .await
    .map_err(repository_settings_error)?;

    Ok(Json(repository_response(&state, &headers, &updated).await?))
}

#[derive(Deserialize)]
pub struct TransferRepoRequest {
    pub new_owner: String,
}

pub async fn transfer_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<TransferRepoRequest>,
) -> Result<Json<RepoResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(repository_settings_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;
    check_repo_admin_access(&state.pool, &repository, &headers).await?;
    let updated = lifecycle::transfer(&state.pool, user.id, repository.id, &req.new_owner)
        .await
        .map_err(crate::backend::api::repos::responses::lifecycle_error)?;
    Ok(Json(repository_response(&state, &headers, &updated).await?))
}

#[derive(Deserialize)]
pub struct AboutRequest {
    pub description: Option<String>,
    pub website: Option<String>,
    /// The complete topic list. Each name is lowercased, and the list replaces the old one.
    pub topics: Vec<String>,
}

/// Saves the About text: description, website and topics. Anyone with write access may save
/// it, and an archived repository refuses it.
pub async fn about_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<AboutRequest>,
) -> Result<Json<RepoResponse>, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(repository_settings_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;
    let user = check_repo_write_access(&state.pool, &repository, &headers).await?;
    let description = req.description.unwrap_or_default();
    lifecycle::validate_description(&description)
        .map_err(crate::backend::api::repos::responses::lifecycle_error)?;
    let website =
        db::repositories::topics::parse_website(req.website.as_deref().unwrap_or_default())
            .map_err(repository_settings_error)?;
    let topics = db::repositories::topics::parse_topics(req.topics.iter().map(String::as_str))
        .map_err(repository_settings_error)?;
    db::repositories::topics::update_about(
        &state.pool,
        user.id,
        repository.id,
        &description,
        website.as_deref(),
        &topics,
    )
    .await
    .map_err(repository_settings_error)?;
    let updated = db::repositories::records::get_repository_by_id(&state.pool, repository.id)
        .await
        .map_err(repository_settings_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;
    Ok(Json(repository_response(&state, &headers, &updated).await?))
}

pub async fn update_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<UpdateRepoRequest>,
) -> Result<Json<RepoResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(repository_settings_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_admin_access(&state.pool, &repository, &headers).await?;
    lifecycle::validate_description(req.description.as_deref().unwrap_or(""))
        .map_err(crate::backend::api::repos::responses::lifecycle_error)?;
    if !req.is_private
        && lifecycle::visibility_locked(&state.pool, &repository)
            .await
            .map_err(|error| {
                (
                    crate::backend::api::repos::responses::lifecycle_status(&error),
                    Json(ApiError::new("storage_unavailable", error.message)),
                )
            })?
    {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiError::new(
                "visibility_locked",
                "A fork of nonpublic history cannot be made public.",
            )),
        ));
    }
    // The merge methods, description and visibility save in one transaction, so a refusal or a
    // failed write leaves all of them unchanged.
    db::repositories::records::update_repository(
        &state.pool,
        user.id,
        repository.id,
        req.description.as_deref(),
        req.is_private,
        db::repositories::records::MergeMethodUpdate {
            merge: req.allow_merge_commit,
            squash: req.allow_squash_merge,
            rebase: req.allow_rebase_merge,
        },
    )
    .await
    .map_err(repository_settings_error)?;
    let updated = db::repositories::records::get_repository_by_id(&state.pool, repository.id)
        .await
        .map_err(repository_settings_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    Ok(Json(repository_response(&state, &headers, &updated).await?))
}

pub async fn delete_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(repository_settings_error)?
        .ok_or_else(|| {
            crate::backend::api::repos::access::access_error(
                crate::backend::repositories::access::AccessError::NotFound,
            )
        })?;
    check_repo_admin_access(&state.pool, &repository, &headers).await?;
    let actor = get_current_user(&state.pool, &headers).await?;
    lifecycle::delete(&state.pool, state.cluster.as_ref(), actor.id, repository.id)
        .await
        .map_err(crate::backend::api::repos::responses::lifecycle_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The access level as the settings page names it.
fn access_label(level: db::models::AccessLevel) -> &'static str {
    match level {
        db::models::AccessLevel::Read => "read",
        db::models::AccessLevel::Triage => "triage",
        db::models::AccessLevel::Write => "write",
        db::models::AccessLevel::Maintain => "maintain",
        db::models::AccessLevel::Admin => "admin",
        db::models::AccessLevel::Owner => "owner",
        db::models::AccessLevel::None => "none",
    }
}

pub(super) async fn settings_model(
    ctx: &PageContext,
    loaded: &Loaded,
    tab: &str,
) -> Result<view::settings::Settings, Response> {
    let viewer = ctx.require_user()?;
    if !loaded.chrome.can_admin {
        return Err(ctx.repo_error(&loaded.chrome, StatusCode::FORBIDDEN, REPO_ADMIN_REQUIRED));
    }
    // The branches the default-branch picker offers, and the one the repository opens on.
    let mut branches: Vec<String> = loaded
        .refs
        .keys()
        .filter_map(|name| name.strip_prefix("refs/heads/").map(str::to_owned))
        .collect();
    branches.sort();
    let mut settings = view::settings::Settings {
        tab: tab.to_owned(),
        branches,
        default_branch: loaded.chrome.default_branch.clone(),
        allow_merge_commit: loaded.repository.allow_merge_commit,
        allow_squash_merge: loaded.repository.allow_squash_merge,
        allow_rebase_merge: loaded.repository.allow_rebase_merge,
        viewer: viewer.username.clone(),
        ..view::settings::Settings::default()
    };
    // The people a rule may let force push, offered as checkboxes beside the viewer's own.
    settings.force_push_suggestions = db::repositories::access::list_write_access_users(
        &ctx.state.pool,
        loaded.repository.id,
        viewer.id,
    )
    .await
    .map_err(|_| unavailable(ctx))?
    .into_iter()
    .map(|(username, level)| view::settings::Collaborator {
        username,
        access: access_label(level).into(),
    })
    .collect();
    if loaded.chrome.can_manage_collaborators {
        // Only personal owners manage collaborators, as the JSON listing does.
        let accesses = db::repositories::access::list_repo_collaborators(
            &ctx.state.pool,
            loaded.repository.id,
        )
        .await
        .map_err(|_| unavailable(ctx))?;
        for access in accesses {
            let user = db::users::accounts::get_user_by_id(&ctx.state.pool, access.user_id)
                .await
                .map_err(|_| unavailable(ctx))?
                .ok_or_else(|| unavailable(ctx))?;
            settings.collaborators.push(view::settings::Collaborator {
                username: user.username,
                access: access_label(db::models::AccessLevel::from(access.access_level)).into(),
            });
        }
    }
    settings.branch_rules = branch_rules(ctx, loaded).await?;
    if tab == "deploy-keys" {
        settings.deploy_keys =
            Some(super::deploy_keys::settings_section(ctx, loaded, viewer).await?);
    }
    if tab == "webhooks" {
        let base = format!("{}/settings?tab=webhooks", loaded.chrome.base());
        let action_base = format!("{}/_forms", loaded.chrome.base());
        let place = panel::Place {
            owner: db::webhooks::HookOwner::Repository(loaded.repository.id),
            base: &base,
            action_base: &action_base,
            csrf_token: &loaded.chrome.csrf_token,
            repository_id: Some(loaded.repository.id),
        };
        let built = panel::build(&ctx.state.pool, viewer, &place, &ctx.query)
            .await
            .map_err(|error| webhook_page_error(ctx, loaded, error))?;
        settings.webhooks = Some(Box::new(built));
    }
    Ok(settings)
}

fn webhook_page_error(ctx: &PageContext, loaded: &Loaded, error: HookError) -> Response {
    let (status, message) = panel::refusal(&error);
    ctx.repo_error(&loaded.chrome, status, message)
}

async fn settings_page(ctx: &PageContext, loaded: &Loaded) -> Result<Response, Response> {
    let tab = view::settings::settings_tab(
        ctx.query.get("tab").map_or("", String::as_str),
        loaded.chrome.can_manage_collaborators,
    );
    let mut settings = settings_model(ctx, loaded, tab).await?;
    settings.saved = ctx.query.get("saved").map(String::as_str) == Some("general");
    Ok(settings_response(ctx, loaded, settings, StatusCode::OK))
}

/// The settings page on the section that a settings action posts from, so its redirect returns
/// there.
fn settings_section_url(base: &str, action: &str) -> String {
    format!(
        "{base}/settings?tab={}",
        view::settings::settings_section(action).unwrap_or("general")
    )
}

pub(super) fn settings_response(
    ctx: &PageContext,
    loaded: &Loaded,
    settings: view::settings::Settings,
    status: StatusCode,
) -> Response {
    with_status(
        ctx.page_model(
            "Repository settings",
            crate::webui::app::Page::Repositories(view::page::Page::Settings {
                repo: loaded.chrome.clone(),
                settings,
            }),
        ),
        status,
    )
}

/// The settings page again, on `tab`, with a refusal at its top.
async fn settings_refusal(
    ctx: &PageContext,
    loaded: &Loaded,
    tab: &str,
    status: StatusCode,
    message: &str,
) -> Response {
    let mut settings = match settings_model(ctx, loaded, tab).await {
        Ok(settings) => settings,
        Err(response) => return response,
    };
    settings.error = Some(message.to_owned());
    settings_response(ctx, loaded, settings, status)
}

/// The repository's branch protection rules, read the way the JSON API reads them.
async fn branch_rules(
    ctx: &PageContext,
    loaded: &Loaded,
) -> Result<Vec<view::settings::BranchRule>, Response> {
    let rules = db::repositories::protection::list_branch_protection_rules(
        &ctx.state.pool,
        loaded.repository.id,
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    let mut listed = Vec::with_capacity(rules.len());
    for rule in rules {
        let rule = branch_protection::rule_response(&ctx.state.pool, rule)
            .await
            .map_err(|_| unavailable(ctx))?;
        listed.push(view::settings::BranchRule {
            pattern: rule.pattern,
            require_pull_request: rule.require_pull_request,
            required_approving_reviews: rule.required_approving_reviews.to_string(),
            dismiss_stale_reviews: rule.dismiss_stale_reviews,
            require_status_checks: rule.require_status_checks,
            required_status_checks: rule.required_status_checks,
            require_up_to_date_branch: rule.require_up_to_date_branch,
            require_linear_history: rule.require_linear_history,
            require_conversation_resolution: rule.require_conversation_resolution,
            require_code_owner_review: rule.require_code_owner_reviews,
            enforce_admins: rule.enforce_admins,
            allow_force_pushes: rule.allow_force_pushes,
            allow_deletions: rule.allow_deletions,
            restrict_pushes: rule.restrict_pushes,
            push_allowances: rule.push_allowances,
            force_push_allowances: rule.force_push_allowances,
        });
    }
    Ok(listed)
}

/// Names from a text field, split at any of the separators. Blank pieces are dropped.
fn names(text: &str, separator: impl Fn(char) -> bool) -> Vec<String> {
    text.split(separator)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Who a rule lets force push: the checked people, then the names typed in the extra field. Each
/// name counts once, first spelling kept, and the API still checks every name.
fn force_push_names(form: &HashMap<String, String>, pairs: &[(String, String)]) -> Vec<String> {
    let typed = names(&field(form, "force_push_extra"), |c: char| {
        c == ',' || c.is_whitespace()
    });
    let mut seen = HashSet::new();
    repeated(pairs, "force_push_allowances")
        .into_iter()
        .chain(typed)
        .filter(|name| seen.insert(name.to_lowercase()))
        .collect()
}

/// The rule a branch protection form submitted, kept as typed so a refusal can show it again.
fn submitted_rule(
    form: &HashMap<String, String>,
    pairs: &[(String, String)],
) -> view::settings::BranchRule {
    view::settings::BranchRule {
        pattern: field(form, "pattern"),
        require_pull_request: checkbox(form, "require_pull_request"),
        required_approving_reviews: field(form, "required_approving_reviews"),
        dismiss_stale_reviews: checkbox(form, "dismiss_stale_reviews"),
        require_status_checks: checkbox(form, "require_status_checks"),
        required_status_checks: view::settings::split_status_checks(&field(
            form,
            "required_status_checks",
        )),
        require_up_to_date_branch: checkbox(form, "require_up_to_date_branch"),
        require_linear_history: checkbox(form, "require_linear_history"),
        require_conversation_resolution: checkbox(form, "require_conversation_resolution"),
        require_code_owner_review: checkbox(form, "require_code_owner_review"),
        enforce_admins: checkbox(form, "enforce_admins"),
        // The form asks to block these; the rule stores what they allow.
        allow_force_pushes: !checkbox(form, "block_force_pushes"),
        allow_deletions: !checkbox(form, "block_deletions"),
        restrict_pushes: checkbox(form, "restrict_pushes"),
        push_allowances: names(&field(form, "push_allowances"), |c: char| {
            c == ',' || c.is_whitespace()
        }),
        force_push_allowances: force_push_names(form, pairs),
    }
}

/// The JSON API request for a submitted rule.
fn rule_request(
    rule: &view::settings::BranchRule,
) -> Result<branch_protection::UpsertBranchProtectionRuleRequest, (StatusCode, Json<ApiError>)> {
    let required_approving_reviews = rule
        .required_approving_reviews
        .trim()
        .parse::<i32>()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(
                    "validation_error",
                    "Required approvals must be a whole number.",
                )),
            )
        })?;
    Ok(branch_protection::UpsertBranchProtectionRuleRequest {
        pattern: rule.pattern.clone(),
        require_pull_request: rule.require_pull_request,
        required_approving_reviews,
        dismiss_stale_reviews: rule.dismiss_stale_reviews,
        require_status_checks: rule.require_status_checks,
        required_status_checks: rule.required_status_checks.clone(),
        require_up_to_date_branch: rule.require_up_to_date_branch,
        require_linear_history: rule.require_linear_history,
        require_conversation_resolution: rule.require_conversation_resolution,
        require_code_owner_reviews: rule.require_code_owner_review,
        enforce_admins: rule.enforce_admins,
        allow_force_pushes: rule.allow_force_pushes,
        allow_deletions: rule.allow_deletions,
        restrict_pushes: rule.restrict_pushes,
        push_allowances: rule.push_allowances.clone(),
        force_push_allowances: rule.force_push_allowances.clone(),
        // The form never offers signed commits, so a submitted rule asks for none.
        require_signed_commits: false,
    })
}

/// The branch protection forms: save a rule (the add form and each rule's edit form) and delete
/// one. A refused save shows the settings page again with the rule as submitted, next to the form
/// that sent it. A refused delete puts its message at the top of the page.
async fn protection_form(
    ctx: &PageContext,
    loaded: &Loaded,
    action: &str,
    form: &HashMap<String, String>,
    pairs: &[(String, String)],
) -> Response {
    let actor_id = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    let pool = &ctx.state.pool;
    let repository = &loaded.repository;
    let section = view::settings::settings_section(action).unwrap_or("general");
    let settings_url = format!("{}/settings?tab={section}", loaded.chrome.base());
    if action == "protection-delete" {
        let outcome = match confirmation(form) {
            Ok(()) => {
                branch_protection::delete_rule(
                    pool,
                    actor_id,
                    repository.id,
                    &field(form, "pattern"),
                )
                .await
            }
            Err(message) => Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError::new("validation_error", message)),
            )),
        };
        return match outcome {
            Ok(()) => ctx.redirect(&settings_url),
            Err((status, Json(error))) => {
                settings_refusal(ctx, loaded, section, status, &error.message).await
            }
        };
    }
    let submitted = submitted_rule(form, pairs);
    let outcome = match rule_request(&submitted) {
        Ok(request) => branch_protection::save_rule(pool, actor_id, repository, request)
            .await
            .map(|_| ()),
        Err(refusal) => Err(refusal),
    };
    match outcome {
        Ok(()) => ctx.redirect(&settings_url),
        Err((status, Json(error))) => {
            let mut settings = match settings_model(ctx, loaded, section).await {
                Ok(settings) => settings,
                Err(response) => return response,
            };
            settings.rule_draft = Some(view::settings::RuleDraft {
                rule: submitted,
                message: error.message,
            });
            settings_response(ctx, loaded, settings, status)
        }
    }
}

/// The webhook section's forms. A saved or changed hook returns to the section's list with a
/// notice. A refused save shows the section again with the submitted hook still in its form.
async fn webhook_form(
    ctx: &PageContext,
    loaded: &Loaded,
    action: &str,
    form: &HashMap<String, String>,
    pairs: &[(String, String)],
) -> Response {
    let actor = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let pool = &ctx.state.pool;
    let owner = db::webhooks::HookOwner::Repository(loaded.repository.id);
    let base = format!("{}/settings?tab=webhooks", loaded.chrome.base());
    let outcome = panel::apply(
        pool,
        ctx.state.secret_encryptor.as_deref(),
        actor,
        owner,
        action,
        form,
        pairs,
    )
    .await;
    match outcome {
        Ok(notice) => ctx.redirect(&format!("{base}&notice={notice}")),
        Err((error, draft)) => {
            let (status, message) = panel::refusal(&error);
            let mut settings = match settings_model(ctx, loaded, "webhooks").await {
                Ok(settings) => settings,
                Err(response) => return response,
            };
            if let (Some(panel), Some(mut draft)) = (settings.webhooks.as_mut(), draft) {
                if let Some(id) = draft.id {
                    draft.has_secret = db::webhooks::get_webhook(pool, owner, id)
                        .await
                        .ok()
                        .flatten()
                        .is_some_and(|hook| hook.encrypted_secret.is_some());
                }
                panel.editing = Some(draft);
            }
            if let Some(panel) = settings.webhooks.as_mut() {
                panel.error = Some(message.to_owned());
            }
            settings_response(ctx, loaded, settings, status)
        }
    }
}

pub(super) fn field(form: &HashMap<String, String>, name: &str) -> String {
    form.get(name).cloned().unwrap_or_default()
}

/// The values a repeated form field was sent with, in the order they were sent.
pub(super) fn repeated(pairs: &[(String, String)], name: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
        .collect()
}

/// A native form sends a checked checkbox as "on" and an unchecked one not at all.
/// The Stars list unstars in place: its form names its own address, so the action returns there.
/// Any other `return_to` is ignored.
fn stars_return(form: &HashMap<String, String>) -> Option<String> {
    form.get("return_to")
        .filter(|target| target.as_str() == "/stars" || target.starts_with("/stars?"))
        .filter(|target| crate::backend::api::http::origin::safe_redirect_path(target))
        .cloned()
}

pub(super) fn checkbox(form: &HashMap<String, String>, name: &str) -> bool {
    matches!(form.get(name).map(String::as_str), Some("on" | "true"))
}

fn confirmation(form: &HashMap<String, String>) -> Result<(), &'static str> {
    if form.get("confirm").map(String::as_str) == Some("yes") {
        Ok(())
    } else {
        Err("Explicit confirmation is required.")
    }
}

pub(super) fn with_status(mut response: Response, status: StatusCode) -> Response {
    // Never turn a renderer/runtime 503 into a validation result or success.
    if response.status() == StatusCode::OK {
        *response.status_mut() = status;
    }
    response
}

/// Only declared non-secret form fields enter typed bootstrap/navigation state.
/// Arbitrary extra POST keys, credentials and the synchronizer token are never reflected.
fn safe_submission(action: &str, values: &HashMap<String, String>) -> HashMap<String, String> {
    if action == "merge-methods" {
        // An unchecked box is not posted at all, so the retry form takes each box's state from
        // `checkbox`, the same reading the save uses.
        return [
            "allow_merge_commit",
            "allow_squash_merge",
            "allow_rebase_merge",
        ]
        .into_iter()
        .map(|name| (name.to_owned(), checkbox(values, name).to_string()))
        .collect();
    }
    let fields: &[&str] = match action {
        "create-branch" => &["name", "sha"],
        "create-tag" => &["name", "sha", "message"],
        "delete-branch" | "delete-tag" | "rename" => &["name"],
        "transfer" => &["new_owner"],
        "about" => &["description", "website", "topics"],
        "settings" => &["description", "visibility"],
        "default-branch" => &["branch"],
        "collaborator" => &["username", "access"],
        "remove-collaborator" => &["username"],
        "label" => &["name", "color", "description"],
        "edit-label" => &["label_id", "name", "color", "description"],
        "delete-label" => &["label_id"],
        "delete" => &["confirmation"],
        _ => &[],
    };
    fields
        .iter()
        .filter_map(|key| {
            values
                .get(*key)
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .collect()
}

fn reject(
    ctx: &PageContext,
    loaded: &Loaded,
    action: &str,
    values: &HashMap<String, String>,
    status: StatusCode,
    message: &str,
) -> Response {
    with_status(
        ctx.page_model(
            "Action could not be completed",
            crate::webui::app::Page::Repositories(view::page::Page::RejectedForm {
                repo: loaded.chrome.clone(),
                action: action.to_owned(),
                message: message.to_owned(),
                values: safe_submission(action, values),
            }),
        ),
        status,
    )
}

/// The repository page that shows what a form action works on.
fn form_page_suffix(action: &str) -> &'static str {
    match action {
        "create-branch" | "delete-branch" => "/branches",
        "create-tag" | "delete-tag" => "/tags",
        "settings"
        | "merge-methods"
        | "rename"
        | "transfer"
        | "delete"
        | "archive"
        | "unarchive"
        | "default-branch"
        | "collaborator"
        | "remove-collaborator"
        | "protection-save"
        | "protection-delete" => "/settings",
        "label" | "edit-label" | "delete-label" => "/labels",
        _ => "",
    }
}

/// `%XX` escapes decoded once; bytes that are not valid escapes stay as they are.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escaped = (bytes[at] == b'%')
            .then(|| bytes.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The editor or delete page a rejected save names in `from`, when it is a page of this repository
/// and a browser cannot resolve it anywhere else: no dot segments (in any escaping), empty
/// segments, backslashes or control characters.
fn editor_return<'a>(from: &'a str, base: &str) -> Option<&'a str> {
    if !["/edit/", "/new/", "/delete/"]
        .iter()
        .any(|kind| from.starts_with(&format!("{base}{kind}")))
    {
        return None;
    }
    // The browser resolves `%2e%2e` as dot segments, so it is decoded once. `%252e` is a file
    // named `%2e` and stays allowed.
    let decoded = percent_decoded(from);
    let unsafe_text = decoded.contains("//")
        || decoded.chars().any(|c| c == '\\' || c.is_control())
        || decoded
            .split('/')
            .any(|segment| matches!(segment, "." | ".."));
    (!unsafe_text).then_some(from)
}

/// A rejected submission is shown at the form's own URL, which accepts only POST. A reload
/// or a copied address sends a GET there instead, and lands on the page the form belongs to.
/// The editor and delete forms name their page in `from`.
pub(super) async fn get_form(
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let base = format!(
        "/{}/{}",
        crate::webui::encode_component(&params["owner"]),
        crate::webui::encode_component(&params["repo"])
    );
    let editor = query
        .get("from")
        .and_then(|from| editor_return(from, &base));
    let target = match (params["action"].as_str(), editor) {
        ("write-file" | "delete-file", Some(from)) => from.to_owned(),
        (action, _) => format!("{base}{}", form_page_suffix(action)),
    };
    crate::backend::api::http::render::redirect_response(&target)
}

pub(super) async fn post_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<HashMap<String, String>>,
    // The pairs keep the repeated checkboxes of a rule's force push list.
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let form: HashMap<String, String> = pairs.iter().cloned().collect();
    let owner = &params["owner"];
    let name = &params["repo"];
    let action = &params["action"];
    // This URL accepts only POST, so every failure page must link its appearance switch to
    // the repository page instead. Set it before the checks that can fail.
    ctx.return_to = Some(format!(
        "/{}/{}{}",
        crate::webui::encode_component(owner),
        crate::webui::encode_component(name),
        form_page_suffix(action)
    ));
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    if let Err(response) = ctx.require_user() {
        return response;
    }
    if ![
        "star",
        "unstar",
        "watch",
        "unwatch",
        "watch-level",
        "fork",
        "create-branch",
        "delete-branch",
        "create-tag",
        "delete-tag",
        "settings",
        "merge-methods",
        "rename",
        "transfer",
        "delete",
        "collaborator",
        "remove-collaborator",
        "label",
        "edit-label",
        "delete-label",
        "about",
        "write-file",
        "delete-file",
        "protection-save",
        "protection-delete",
        "archive",
        "unarchive",
        "default-branch",
        "webhook-save",
        "webhook-delete",
        "webhook-ping",
        "webhook-redeliver",
        "deploy-key-add",
        "deploy-key-delete",
    ]
    .contains(&action.as_str())
    {
        return ctx.error(StatusCode::NOT_FOUND, "Repository action not found.");
    }
    let loaded = match load_metadata(&ctx, owner, name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    // A repository form names the repository it was rendered for. After a rename, another
    // repository can take the old owner and name, and the form must not reach that one.
    let repository_id = loaded.repository.id.to_string();
    if [
        "about",
        "settings",
        "merge-methods",
        "rename",
        "transfer",
        "delete",
        "collaborator",
        "remove-collaborator",
        "protection-save",
        "protection-delete",
        "delete-file",
        "archive",
        "unarchive",
        "default-branch",
        "webhook-save",
        "webhook-delete",
        "webhook-ping",
        "webhook-redeliver",
        "deploy-key-add",
        "deploy-key-delete",
    ]
    .contains(&action.as_str())
        && field(&form, "repository_id") != repository_id
    {
        return ctx.error(
            StatusCode::CONFLICT,
            "The repository changed. Reload the page and try again.",
        );
    }
    // A refused save returns the editor with the submitted text, not an error page.
    if action == "write-file" && field(&form, "repository_id") != repository_id {
        return replaced_repository_editor(ctx, &loaded, &form).await;
    }
    // Actions that write Git data or remove its storage wait for Git, as the page loads do.
    let needs_git = [
        "create-branch",
        "delete-branch",
        "default-branch",
        "create-tag",
        "delete-tag",
        "write-file",
        "delete-file",
        "fork",
        "delete",
    ];
    if loaded.git_unavailable && needs_git.contains(&action.as_str()) {
        return git_unavailable_response(&ctx);
    }
    if action == "write-file" {
        return write_form(ctx, &loaded, &form).await;
    }
    if action == "delete-file" {
        return delete_form(ctx, &loaded, &form).await;
    }
    let required_confirm = [
        "delete-branch",
        "delete-tag",
        "rename",
        "remove-collaborator",
        "delete-label",
        "archive",
        "unarchive",
        "deploy-key-delete",
    ];
    // Only a visibility change needs the confirmation. A description-only save does not.
    let changes_visibility = action == "settings"
        && (field(&form, "visibility") == "private") != loaded.repository.is_private;
    if changes_visibility || required_confirm.contains(&action.as_str()) {
        if let Err(message) = confirmation(&form) {
            return reject(
                &ctx,
                &loaded,
                action,
                &form,
                StatusCode::BAD_REQUEST,
                message,
            );
        }
    }
    if action == "delete" && field(&form, "confirmation") != format!("{owner}/{name}") {
        return reject(
            &ctx,
            &loaded,
            action,
            &form,
            StatusCode::BAD_REQUEST,
            "Type the complete owner/repository name to confirm permanent deletion.",
        );
    }
    if action == "transfer" && field(&form, "confirmation") != name.as_str() {
        return reject(
            &ctx,
            &loaded,
            action,
            &form,
            StatusCode::BAD_REQUEST,
            "Type the repository name to confirm the transfer.",
        );
    }
    let administration = [
        "settings",
        "merge-methods",
        "rename",
        "transfer",
        "delete",
        "label",
        "edit-label",
        "delete-label",
        "collaborator",
        "remove-collaborator",
        "protection-save",
        "protection-delete",
        "archive",
        "unarchive",
        "default-branch",
        "webhook-save",
        "webhook-delete",
        "webhook-ping",
        "webhook-redeliver",
        "deploy-key-add",
        "deploy-key-delete",
    ];
    if administration.contains(&action.as_str()) && !loaded.chrome.can_admin {
        return ctx.repo_error(&loaded.chrome, StatusCode::FORBIDDEN, REPO_ADMIN_REQUIRED);
    }
    if action.starts_with("protection-") {
        return protection_form(&ctx, &loaded, action, &form, &pairs).await;
    }
    if action.starts_with("webhook-") {
        return webhook_form(&ctx, &loaded, action, &form, &pairs).await;
    }
    if action.starts_with("deploy-key-") {
        return super::deploy_keys::form(&ctx, &loaded, action, &form).await;
    }
    if [
        "create-branch",
        "delete-branch",
        "create-tag",
        "delete-tag",
        "about",
    ]
    .contains(&action.as_str())
        && !loaded.chrome.can_write
    {
        return ctx.error(StatusCode::FORBIDDEN, "Write access is required.");
    }
    if action == "settings" && !matches!(field(&form, "visibility").as_str(), "public" | "private")
    {
        return reject(
            &ctx,
            &loaded,
            action,
            &form,
            StatusCode::BAD_REQUEST,
            "Choose public or private visibility.",
        );
    }
    if action == "collaborator"
        && !matches!(field(&form, "access").as_str(), "read" | "write" | "admin")
    {
        return reject(
            &ctx,
            &loaded,
            action,
            &form,
            StatusCode::BAD_REQUEST,
            "Choose a valid collaborator access level.",
        );
    }
    let repository = &loaded.repository;
    let result = mutate(&ctx, repository, owner, name, action, &form).await;
    match result {
        Ok(path) => ctx.redirect(&path),
        Err((status, Json(error))) => {
            let message = if matches!(
                error.error.as_str(),
                "followup_failed" | "storage_unavailable" | "validation_error"
            ) {
                error.message.as_str()
            } else {
                safe_error(status)
            };
            reject(&ctx, &loaded, action, &form, status, message)
        }
    }
}

/// The name, `#rrggbb` color and description of a label form. Creating and editing a label check
/// them the same way.
fn label_fields(form: &HashMap<String, String>) -> ApiResult<(String, String, String)> {
    let name = field(form, "name");
    let description = field(form, "description");
    let color = field(form, "color");
    let hex = color.trim_start_matches('#');
    if crate::backend::issues::validate_label(&name, Some(description.as_str())).is_err()
        || hex.len() != 6
        || !hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Invalid label name or color",
            )),
        ));
    }
    Ok((name, format!("#{hex}"), description))
}

/// The label that an edit or delete form names.
fn label_id(form: &HashMap<String, String>) -> ApiResult<i64> {
    field(form, "label_id").parse::<i64>().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("validation_error", "Invalid label ID")),
        )
    })
}

/// A label write that the database refused or could not finish.
fn label_mutation_error(_error: anyhow::Error) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError::new(
            "unavailable",
            "Label mutation could not be confirmed",
        )),
    )
}

async fn mutate(
    ctx: &PageContext,
    repository: &db::models::Repository,
    owner: &str,
    name: &str,
    action: &str,
    form: &HashMap<String, String>,
) -> ApiResult<String> {
    let pool = &ctx.state.pool;
    let actor = ctx.user.as_ref().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ApiError::new(
                "unauthorized",
                "Sign in to perform this action",
            )),
        )
    })?;
    let base = format!(
        "/{}/{}",
        crate::webui::encode_component(owner),
        crate::webui::encode_component(name)
    );
    let cluster = ctx.state.cluster.as_ref();
    let ci = &ctx.state.ci_runs;
    match action {
        "star" | "unstar" | "watch" | "unwatch" => {
            use crate::backend::repositories::engagement::{change, Engagement};
            let kind = if matches!(action, "star" | "unstar") {
                Engagement::Star
            } else {
                Engagement::Watch
            };
            change(
                pool,
                &ctx.state.notification_broadcaster,
                actor.id,
                repository.id,
                kind,
                !action.starts_with("un"),
            )
            .await
            .map_err(crate::backend::api::repos::responses::engagement_error)?;
            Ok(stars_return(form).unwrap_or(base))
        }
        "watch-level" => {
            let events: Vec<&str> = ["issues", "pulls", "releases"]
                .into_iter()
                .filter(|name| checkbox(form, &format!("events_{name}")))
                .collect();
            crate::backend::repositories::engagement::set_level(
                pool,
                actor.id,
                repository.id,
                &field(form, "level"),
                &events,
            )
            .await
            .map_err(crate::backend::api::repos::responses::engagement_error)?;
            Ok(base)
        }
        "fork" => {
            let fork = lifecycle::fork(
                pool,
                cluster,
                &ctx.state.repos_path,
                &ctx.state.notification_broadcaster,
                actor.id,
                repository.id,
                owner,
            )
            .await
            .map_err(crate::backend::api::repos::responses::lifecycle_error)?;
            Ok(format!(
                "/{}/{}",
                crate::webui::encode_component(&actor.username),
                crate::webui::encode_component(&fork.name)
            ))
        }
        "create-branch" => {
            crate::backend::repositories::refs::create_branch(
                pool,
                cluster,
                ci,
                actor.id,
                owner,
                name,
                &field(form, "name"),
                &field(form, "sha"),
            )
            .await
            .map_err(crate::backend::api::repos::responses::ref_error)?;
            Ok(format!("{base}/branches"))
        }
        "default-branch" => {
            crate::backend::repositories::access::check_admin(pool, repository.id, actor.id)
                .await
                .map_err(crate::backend::api::repos::access::access_error)?;
            crate::backend::repositories::refs::set_default_branch(
                pool,
                cluster,
                actor.id,
                repository,
                &field(form, "branch"),
            )
            .await
            .map_err(crate::backend::api::repos::responses::ref_error)?;
            Ok(settings_section_url(&base, action))
        }
        "delete-branch" => {
            crate::backend::repositories::refs::delete_branch(
                pool,
                cluster,
                ci,
                actor.id,
                owner,
                name,
                &field(form, "name"),
            )
            .await
            .map_err(crate::backend::api::repos::responses::ref_error)?;
            Ok(format!("{base}/branches"))
        }
        "create-tag" => {
            let message = field(form, "message");
            crate::backend::repositories::refs::create_tag(
                pool,
                cluster,
                ci,
                actor.id,
                owner,
                name,
                &field(form, "name"),
                &field(form, "sha"),
                (!message.is_empty()).then_some(message.as_str()),
            )
            .await
            .map_err(crate::backend::api::repos::responses::ref_error)?;
            Ok(format!("{base}/tags"))
        }
        "delete-tag" => {
            crate::backend::repositories::refs::delete_tag(
                pool,
                cluster,
                actor.id,
                owner,
                name,
                &field(form, "name"),
            )
            .await
            .map_err(crate::backend::api::repos::responses::ref_error)?;
            Ok(format!("{base}/tags"))
        }
        "settings" => {
            crate::backend::repositories::access::check_admin(pool, repository.id, actor.id)
                .await
                .map_err(crate::backend::api::repos::access::access_error)?;
            lifecycle::validate_description(&field(form, "description"))
                .map_err(crate::backend::api::repos::responses::lifecycle_error)?;
            let private = field(form, "visibility") == "private";
            if !private
                && lifecycle::visibility_locked(pool, repository)
                    .await
                    .map_err(crate::backend::api::repos::responses::lifecycle_error)?
            {
                return Err((
                    StatusCode::FORBIDDEN,
                    Json(ApiError::new(
                        "visibility_locked",
                        "A fork of nonpublic history cannot be made public.",
                    )),
                ));
            }
            db::repositories::records::update_repository(
                pool,
                actor.id,
                repository.id,
                Some(&field(form, "description")),
                private,
                db::repositories::records::MergeMethodUpdate {
                    merge: None,
                    squash: None,
                    rebase: None,
                },
            )
            .await
            .map_err(repository_settings_error)?;
            Ok(format!("{base}/settings?tab=general&saved=general"))
        }
        "merge-methods" => {
            db::repositories::records::set_merge_methods(
                pool,
                actor.id,
                repository.id,
                db::repositories::records::MergeMethodUpdate {
                    merge: Some(checkbox(form, "allow_merge_commit")),
                    squash: Some(checkbox(form, "allow_squash_merge")),
                    rebase: Some(checkbox(form, "allow_rebase_merge")),
                },
            )
            .await
            .map_err(repository_settings_error)?;
            Ok(settings_section_url(&base, action))
        }
        "about" => {
            let description = field(form, "description");
            lifecycle::validate_description(&description)
                .map_err(crate::backend::api::repos::responses::lifecycle_error)?;
            let website = db::repositories::topics::parse_website(&field(form, "website"))
                .map_err(repository_settings_error)?;
            // Topics are typed as a list, separated by commas or spaces.
            let topics = db::repositories::topics::parse_topics(
                field(form, "topics")
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .collect::<Vec<_>>(),
            )
            .map_err(repository_settings_error)?;
            db::repositories::topics::update_about(
                pool,
                actor.id,
                repository.id,
                &description,
                website.as_deref(),
                &topics,
            )
            .await
            .map_err(repository_settings_error)?;
            Ok(base)
        }
        "rename" => {
            crate::backend::repositories::access::check_admin(pool, repository.id, actor.id)
                .await
                .map_err(crate::backend::api::repos::access::access_error)?;
            let updated = db::repositories::records::rename_repository(
                pool,
                actor.id,
                repository.id,
                &field(form, "name"),
            )
            .await
            .map_err(repository_settings_error)?;
            Ok(format!(
                "/{}/{}",
                crate::webui::encode_component(owner),
                crate::webui::encode_component(&updated.name)
            ))
        }
        "transfer" => {
            let updated =
                lifecycle::transfer(pool, actor.id, repository.id, &field(form, "new_owner"))
                    .await
                    .map_err(crate::backend::api::repos::responses::lifecycle_error)?;
            let new_owner = repository_owner(pool, &updated).await?;
            Ok(format!(
                "/{}/{}",
                crate::webui::encode_component(&new_owner),
                crate::webui::encode_component(&updated.name)
            ))
        }
        "archive" | "unarchive" => {
            crate::backend::repositories::access::check_admin(pool, repository.id, actor.id)
                .await
                .map_err(crate::backend::api::repos::access::access_error)?;
            // Archiving an archived repository, or the reverse, changes nothing and is not an error.
            db::repositories::records::set_repository_archived(
                pool,
                repository.id,
                action == "archive",
            )
            .await
            .map_err(repository_settings_error)?;
            Ok(settings_section_url(&base, action))
        }
        "delete" => {
            lifecycle::delete(pool, cluster, actor.id, repository.id)
                .await
                .map_err(crate::backend::api::repos::responses::lifecycle_error)?;
            Ok(format!("/{}", crate::webui::encode_component(owner)))
        }
        "collaborator" | "remove-collaborator" => {
            let permission = if action == "remove-collaborator" {
                None
            } else {
                Some(match field(form, "access").as_str() {
                    "admin" => db::models::AccessLevel::Admin,
                    "write" => db::models::AccessLevel::Write,
                    _ => db::models::AccessLevel::Read,
                })
            };
            crate::backend::repositories::access::change_collaborator(
                pool,
                actor.id,
                repository.id,
                &field(form, "username"),
                permission,
            )
            .await
            .map_err(crate::backend::api::repos::access::access_error)?;
            Ok(settings_section_url(&base, action))
        }
        "label" => {
            let (name, color, description) = label_fields(form)?;
            db::labels::create_label(
                pool,
                actor.id,
                repository.id,
                &name,
                &color,
                Some(&description),
            )
            .await
            .map_err(label_mutation_error)?;
            Ok(format!("{base}/labels"))
        }
        "edit-label" => {
            let id = label_id(form)?;
            let (name, color, description) = label_fields(form)?;
            db::labels::update_label(
                pool,
                actor.id,
                repository.id,
                id,
                Some(&name),
                Some(&color),
                Some(&description),
            )
            .await
            .map_err(label_mutation_error)?;
            Ok(format!("{base}/labels"))
        }
        "delete-label" => {
            let id = label_id(form)?;
            db::labels::delete_label(pool, actor.id, repository.id, id)
                .await
                .map_err(label_mutation_error)?;
            Ok(format!("{base}/labels"))
        }
        _ => Err((
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Unknown repository action")),
        )),
    }
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
    let loaded = match load_metadata(&ctx, &params["owner"], &params["repo"]).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    match settings_page(&ctx, &loaded).await {
        Ok(response) | Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::api::repos::chrome::hex_revision;
    use crate::backend::api::repos::code::contents::editor_from;
    #[test]
    fn confirmations_are_explicit_not_truthy() {
        let mut form = HashMap::new();
        assert!(confirmation(&form).is_err());
        form.insert("confirm".into(), "false".into());
        assert!(confirmation(&form).is_err());
        form.insert("confirm".into(), "yes".into());
        assert!(confirmation(&form).is_ok());
    }
    #[test]
    fn editor_redisplay_retains_text_and_expected_revision() {
        let form = HashMap::from([
            ("content".into(), "</textarea><script>x</script>".into()),
            ("expected_revision".into(), "old".into()),
            ("creating".into(), "yes".into()),
            ("intent".into(), "preview".into()),
        ]);
        let editor = editor_from(&form);
        assert_eq!(editor.content, "</textarea><script>x</script>");
        assert_eq!(editor.expected_revision, "old");
        assert!(editor.creating && editor.preview);
    }
    #[test]
    fn validation_models_never_reflect_undeclared_secrets_or_authority() {
        let values = HashMap::from([
            ("name".into(), "feature/x".into()),
            ("sha".into(), "abcdef1234".into()),
            ("password".into(), "must-not-appear".into()),
            ("disk_path".into(), "must-not-appear".into()),
            ("csrf_token".into(), "must-not-appear".into()),
            ("confirm".into(), "yes".into()),
        ]);
        let safe = safe_submission("create-branch", &values);
        assert_eq!(safe.len(), 2);
        assert_eq!(safe["name"], "feature/x");
        assert_eq!(safe["sha"], "abcdef1234");
        assert!(!serde_json::to_string(&safe)
            .unwrap()
            .contains("must-not-appear"));
        assert!(safe_submission("star", &values).is_empty());
    }
    #[test]
    fn command_option_like_revisions_are_not_accepted() {
        assert!(!hex_revision("--all"));
        assert!(!hex_revision("main~1"));
        assert!(hex_revision("abcd1234"));
    }
}
