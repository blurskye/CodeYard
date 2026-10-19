use crate::backend::api::http::error::protection_write_error;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::check_repo_admin_access;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::app::AppState;
use crate::backend::db;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
#[derive(Serialize)]
pub struct BranchProtectionRuleResponse {
    pub pattern: String,
    pub require_pull_request: bool,
    pub required_approving_reviews: i32,
    pub dismiss_stale_reviews: bool,
    pub require_status_checks: bool,
    pub required_status_checks: Vec<String>,
    pub require_up_to_date_branch: bool,
    pub require_linear_history: bool,
    pub require_conversation_resolution: bool,
    pub require_code_owner_reviews: bool,
    pub enforce_admins: bool,
    pub allow_force_pushes: bool,
    pub allow_deletions: bool,
    pub restrict_pushes: bool,
    pub push_allowances: Vec<String>,
    pub force_push_allowances: Vec<String>,
}

fn yes() -> bool {
    true
}

/// Each entry costs a user lookup and an access check, so one request may list only this many.
const MAX_ALLOWANCE_NAMES: usize = 100;

/// An omitted `require_pull_request`, `dismiss_stale_reviews` or `enforce_admins` means
/// true, the strict default. The settings form builds the same request, so both paths refuse
/// and store the same things.
#[derive(Deserialize)]
pub struct UpsertBranchProtectionRuleRequest {
    pub pattern: String,
    #[serde(default = "yes")]
    pub require_pull_request: bool,
    #[serde(default)]
    pub required_approving_reviews: i32,
    #[serde(default = "yes")]
    pub dismiss_stale_reviews: bool,
    #[serde(default)]
    pub require_status_checks: bool,
    #[serde(default)]
    pub required_status_checks: Vec<String>,
    #[serde(default)]
    pub require_up_to_date_branch: bool,
    #[serde(default)]
    pub require_linear_history: bool,
    #[serde(default)]
    pub require_conversation_resolution: bool,
    #[serde(default)]
    pub require_code_owner_reviews: bool,
    #[serde(default = "yes")]
    pub enforce_admins: bool,
    #[serde(default)]
    pub allow_force_pushes: bool,
    #[serde(default)]
    pub allow_deletions: bool,
    #[serde(default)]
    pub restrict_pushes: bool,
    /// Usernames. Each needs write access to the repository.
    #[serde(default)]
    pub push_allowances: Vec<String>,
    /// Usernames, each with write access, who may force push although `allow_force_pushes` is
    /// false.
    #[serde(default)]
    pub force_push_allowances: Vec<String>,
    /// Not enforceable here. Accepted only as false.
    #[serde(default)]
    pub require_signed_commits: bool,
}

pub(in crate::backend::api::repos) async fn rule_response(
    pool: &db::DbPool,
    rule: db::models::BranchProtectionRule,
) -> Result<BranchProtectionRuleResponse, (StatusCode, Json<ApiError>)> {
    let push_allowances = allowance_names(pool, &rule.push_allowances).await?;
    let force_push_allowances = allowance_names(pool, &rule.force_push_allowances).await?;
    Ok(BranchProtectionRuleResponse {
        pattern: rule.pattern,
        require_pull_request: rule.require_pull_request,
        required_approving_reviews: rule.required_approving_reviews,
        dismiss_stale_reviews: rule.dismiss_stale_reviews,
        require_status_checks: rule.require_status_checks,
        required_status_checks: rule.required_status_checks,
        require_up_to_date_branch: rule.require_up_to_date_branch,
        require_linear_history: rule.require_linear_history,
        require_conversation_resolution: rule.require_conversation_resolution,
        require_code_owner_reviews: rule.require_code_owner_review,
        enforce_admins: rule.enforce_admins,
        allow_force_pushes: !rule.block_force_pushes,
        allow_deletions: !rule.block_deletions,
        restrict_pushes: rule.restrict_pushes,
        push_allowances,
        force_push_allowances,
    })
}

/// The usernames of `ids`, in the order given. A deleted account has no name to show, so it is
/// left out.
async fn allowance_names(
    pool: &db::DbPool,
    ids: &[i64],
) -> Result<Vec<String>, (StatusCode, Json<ApiError>)> {
    let users = db::users::accounts::get_users_by_ids(pool, ids)
        .await
        .map_err(|_| unavailable())?;
    Ok(ids
        .iter()
        .filter_map(|id| users.iter().find(|user| user.id == *id))
        .map(|user| user.username.clone())
        .collect())
}

pub async fn list_branch_protection_rules(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<Vec<BranchProtectionRuleResponse>>, (StatusCode, Json<ApiError>)> {
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

    let rules =
        db::repositories::protection::list_branch_protection_rules(&state.pool, repository.id)
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

    let mut responses = Vec::with_capacity(rules.len());
    for rule in rules {
        responses.push(rule_response(&state.pool, rule).await?);
    }
    Ok(Json(responses))
}

pub async fn upsert_branch_protection_rule(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<UpsertBranchProtectionRuleRequest>,
) -> Result<Json<BranchProtectionRuleResponse>, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;

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

    check_repo_admin_access(&state.pool, &repository, &headers).await?;

    let rule = save_rule(&state.pool, actor.id, &repository, req).await?;
    Ok(Json(rule_response(&state.pool, rule).await?))
}

/// Checks one rule and saves it. The JSON API and the settings form both save through this, so
/// they refuse the same input with the same error and store the same rule. Callers check that
/// the actor administers the repository first.
pub(in crate::backend::api::repos) async fn save_rule(
    pool: &db::DbPool,
    actor_id: i64,
    repository: &db::models::Repository,
    req: UpsertBranchProtectionRuleRequest,
) -> Result<db::models::BranchProtectionRule, (StatusCode, Json<ApiError>)> {
    let pattern = req.pattern.trim();
    db::repositories::protection::validate_branch_pattern(pattern)
        .map_err(|error| protection_write_error(error.into()))?;

    if req.required_approving_reviews < 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "required_approving_reviews must be >= 0",
            )),
        ));
    }

    let mut required_status_checks = Vec::new();
    let mut seen_status_checks = HashSet::new();
    for check in &req.required_status_checks {
        let trimmed = check.trim();
        if trimmed.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(
                    "validation_error",
                    "required_status_checks entries cannot be empty",
                )),
            ));
        }
        // A single-line field cannot hold a line break or tab, so such a name could not be edited.
        if trimmed.chars().any(char::is_control) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(
                    "validation_error",
                    "required_status_checks entries cannot contain control characters",
                )),
            ));
        }

        if seen_status_checks.insert(trimmed.to_string()) {
            required_status_checks.push(trimmed.to_string());
        }
    }

    if req.require_status_checks && required_status_checks.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "required_status_checks must contain at least one context when require_status_checks is enabled",
            )),
        ));
    }

    if !req.require_status_checks && !required_status_checks.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "require_status_checks must be true when required_status_checks is provided",
            )),
        ));
    }

    if req.require_signed_commits {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "unsupported_protection",
                "Signed-commit enforcement is not available",
            )),
        ));
    }

    let push_allowances =
        write_user_ids(pool, repository, "push_allowances", &req.push_allowances).await?;
    let force_push_allowances = write_user_ids(
        pool,
        repository,
        "force_push_allowances",
        &req.force_push_allowances,
    )
    .await?;

    let rule = db::repositories::protection::upsert_branch_protection_rule(
        pool,
        db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id,
            repo_id: repository.id,
            pattern,
            require_pull_request: req.require_pull_request,
            required_approving_reviews: req.required_approving_reviews,
            dismiss_stale_reviews: req.dismiss_stale_reviews,
            require_status_checks: req.require_status_checks,
            required_status_checks,
            require_up_to_date_branch: req.require_up_to_date_branch,
            require_linear_history: req.require_linear_history,
            block_force_pushes: !req.allow_force_pushes,
            block_deletions: !req.allow_deletions,
            restrict_pushes: req.restrict_pushes,
            push_allowances,
            force_push_allowances,
            enforce_admins: req.enforce_admins,
            require_conversation_resolution: req.require_conversation_resolution,
            require_code_owner_review: req.require_code_owner_reviews,
        },
    )
    .await
    .map_err(protection_write_error)?;

    Ok(rule)
}

/// The ids of the users named in an allowance list, each of which needs write access to
/// `repository`. `field` names the list in the refusals.
async fn write_user_ids(
    pool: &db::DbPool,
    repository: &db::models::Repository,
    field: &str,
    names: &[String],
) -> Result<Vec<i64>, (StatusCode, Json<ApiError>)> {
    // The cap counts repeated names too, so a long duplicate list is refused before any lookup.
    if names.len() > MAX_ALLOWANCE_NAMES {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                &format!("{field} can list at most {MAX_ALLOWANCE_NAMES} entries"),
            )),
        ));
    }
    // Each name is looked up once, in the order it was first given.
    let mut usernames = Vec::new();
    let mut seen_usernames = HashSet::new();
    for username in names {
        let username = username.trim();
        if seen_usernames.insert(username) {
            usernames.push(username);
        }
    }
    let mut ids = Vec::new();
    for username in usernames {
        let allowed = match db::users::accounts::get_user_by_username(pool, username).await {
            Ok(Some(user)) => {
                match crate::backend::repositories::access::can_write(pool, &user, repository).await
                {
                    Ok(true) => Some(user.id),
                    Ok(false) => None,
                    Err(_) => return Err(unavailable()),
                }
            }
            Ok(None) => None,
            Err(_) => return Err(unavailable()),
        };
        let Some(id) = allowed else {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(
                    "validation_error",
                    &format!("{field} must name users with write access to this repository"),
                )),
            ));
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn unavailable() -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError::new(
            "database_error",
            "Collaboration storage is temporarily unavailable",
        )),
    )
}

pub async fn delete_branch_protection_rule(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, pattern)): Path<(String, String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;

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

    check_repo_admin_access(&state.pool, &repository, &headers).await?;

    delete_rule(&state.pool, actor.id, repository.id, &pattern).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes the rule saved for `pattern`. The JSON API and the settings form both delete through
/// this, so a missing rule is the same not-found answer in both.
pub(in crate::backend::api::repos) async fn delete_rule(
    pool: &db::DbPool,
    actor_id: i64,
    repo_id: i64,
    pattern: &str,
) -> Result<(), (StatusCode, Json<ApiError>)> {
    let deleted = db::repositories::protection::delete_branch_protection_rule(
        pool, actor_id, repo_id, pattern,
    )
    .await
    .map_err(protection_write_error)?;
    if deleted {
        Ok(())
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(ApiError::new(
                "not_found",
                "Branch protection rule not found",
            )),
        ))
    }
}

pub fn router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use axum::{
        routing::{delete, get},
        Router,
    };
    Router::new()
        .route(
            "/api/repos/:owner/:repo/branch-protection",
            get(list_branch_protection_rules).put(upsert_branch_protection_rule),
        )
        .route(
            "/api/repos/:owner/:repo/branch-protection/:pattern",
            delete(delete_branch_protection_rule),
        )
}
