use crate::backend::api::http::error::protection_write_error;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::{check_repo_admin_access, find_repository};
use crate::backend::api::repos::responses::storage_unavailable;
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
    pub push_team_allowances: Vec<String>,
    pub push_apps: Vec<i64>,
    pub force_push_allowances: Vec<String>,
}

fn yes() -> bool {
    true
}

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
    /// Slugs of organization teams with repository write access.
    #[serde(default)]
    pub push_team_allowances: Vec<String>,
    /// Usernames, each with write access, who may force push although `allow_force_pushes` is
    /// false.
    #[serde(default)]
    pub force_push_allowances: Vec<String>,
    /// Not enforceable here. Accepted only as false.
    #[serde(default)]
    pub require_signed_commits: bool,
}

pub(in crate::backend::api::repos) async fn rule_response(
    pool: &codeyard_db_core::DbPool,
    rule: codeyard_db_core::models::BranchProtectionRule,
    actor: i64,
) -> Result<BranchProtectionRuleResponse, (StatusCode, Json<ApiError>)> {
    Ok(rule_responses(pool, vec![rule], actor).await?.remove(0))
}

pub(in crate::backend::api::repos) async fn rule_responses(
    pool: &codeyard_db_core::DbPool,
    rules: Vec<codeyard_db_core::models::BranchProtectionRule>,
    actor: i64,
) -> Result<Vec<BranchProtectionRuleResponse>, (StatusCode, Json<ApiError>)> {
    let mut ids: Vec<_> = rules
        .iter()
        .flat_map(|rule| {
            rule.push_allowances
                .iter()
                .chain(&rule.force_push_allowances)
        })
        .copied()
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let users = db::users::accounts::get_users_by_ids(pool, &ids)
        .await
        .map_err(|_| storage_unavailable())?;
    let users: std::collections::HashMap<_, _> = users
        .into_iter()
        .map(|user| (user.id, user.username))
        .collect();
    let mut teams = std::collections::HashMap::new();
    for rule in &rules {
        let ids: Vec<_> = rule
            .push_team_allowances
            .iter()
            .filter(|id| !teams.contains_key(&(rule.repo_id, **id)))
            .copied()
            .collect();
        if ids.is_empty() {
            continue;
        }
        for team in db::repositories::push_teams::by_ids(pool, rule.repo_id, &ids)
            .await
            .map_err(|_| storage_unavailable())?
        {
            if db::organizations::teams::get_team_by_id(pool, actor, team.team.id)
                .await
                .map_err(|_| storage_unavailable())?
                .is_some()
            {
                teams.insert((rule.repo_id, team.team.id), team.team.slug);
            }
        }
    }
    Ok(project_rules(rules, &users, &teams))
}

fn project_rules(
    rules: Vec<codeyard_db_core::models::BranchProtectionRule>,
    users: &std::collections::HashMap<i64, String>,
    teams: &std::collections::HashMap<(i64, i64), String>,
) -> Vec<BranchProtectionRuleResponse> {
    let names = |ids: &[i64]| ids.iter().filter_map(|id| users.get(id)).cloned().collect();
    rules
        .into_iter()
        .map(|rule| {
            let push_allowances = names(&rule.push_allowances);
            let push_team_allowances = rule
                .push_team_allowances
                .iter()
                .filter_map(|id| teams.get(&(rule.repo_id, *id)))
                .cloned()
                .collect();
            let force_push_allowances = names(&rule.force_push_allowances);
            BranchProtectionRuleResponse {
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
                push_team_allowances,
                push_apps: rule.github_settings.push_apps,
                force_push_allowances,
            }
        })
        .collect()
}

pub(in crate::backend::api::repos) async fn authorized_rule_responses(
    pool: &codeyard_db_core::DbPool,
    actor: impl Into<db::credentials::Actor>,
    repository: i64,
) -> Result<Vec<BranchProtectionRuleResponse>, (StatusCode, Json<ApiError>)> {
    let actor = actor.into();
    let viewer = actor.user_id;
    let snapshot = db::repositories::protection::read_protection_snapshot(pool, actor, repository)
        .await
        .map_err(protection_write_error)?;
    let repository = snapshot.repository_id;
    // Secret team names stay with the people who may see the team, as in `rule_responses`.
    let mut teams = std::collections::HashMap::new();
    for (id, name) in snapshot.teams {
        if db::organizations::teams::get_team_by_id(pool, viewer, id)
            .await
            .map_err(|_| storage_unavailable())?
            .is_some()
        {
            teams.insert((repository, id), name);
        }
    }
    Ok(project_rules(snapshot.rules, &snapshot.users, &teams))
}

pub async fn list_branch_protection_rules(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<Vec<BranchProtectionRuleResponse>>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &user).await?;

    let session =
        crate::backend::api::http::session::extract_session_id(&headers).unwrap_or_default();
    Ok(Json(
        authorized_rule_responses(&state.pool, (user.id, session), repository.id).await?,
    ))
}

pub async fn upsert_branch_protection_rule(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<UpsertBranchProtectionRuleRequest>,
) -> Result<Json<BranchProtectionRuleResponse>, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &actor).await?;

    let session =
        crate::backend::api::http::session::extract_session_id(&headers).unwrap_or_default();
    let rule = save_rule(&state.pool, (actor.id, session), &repository, req).await?;
    Ok(Json(rule_response(&state.pool, rule, actor.id).await?))
}

/// Checks one rule and saves it. The JSON API and the settings form both save through this, so
/// they refuse the same input with the same error and store the same rule. Callers check that
/// the actor administers the repository first.
pub(in crate::backend::api::repos) async fn save_rule(
    pool: &codeyard_db_core::DbPool,
    actor: impl Into<db::credentials::Actor>,
    repository: &codeyard_db_core::models::Repository,
    req: UpsertBranchProtectionRuleRequest,
) -> Result<codeyard_db_core::models::BranchProtectionRule, (StatusCode, Json<ApiError>)> {
    let authority = actor.into();
    let actor_id = authority.user_id;
    let pattern = req.pattern.trim();
    db::repositories::protection::validate_branch_pattern(pattern)
        .map_err(|error| protection_write_error(error.into()))?;

    let max_approvals = db::repositories::protection::MAX_REQUIRED_APPROVALS;
    if !(0..=max_approvals).contains(&req.required_approving_reviews) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                &format!("required_approving_reviews must be between 0 and {max_approvals}"),
            )),
        ));
    }

    db::repositories::protection::validate_required_checks(&req.required_status_checks)
        .map_err(|error| protection_write_error(error.into()))?;
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

    let push_team_allowances =
        db::repositories::push_teams::resolve_names(pool, repository.id, &req.push_team_allowances)
            .await
            .map_err(protection_write_error)?;
    let [push_allowances, force_push_allowances] = write_user_ids(
        pool,
        repository,
        [&req.push_allowances, &req.force_push_allowances],
    )
    .await?;

    let rule = db::repositories::protection::upsert_branch_protection_rule(
        pool,
        db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id,
            credential: authority.credential(),
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
            push_team_allowances,
            force_push_allowances,
            enforce_admins: req.enforce_admins,
            require_conversation_resolution: req.require_conversation_resolution,
            require_code_owner_review: req.require_code_owner_reviews,
            github_settings: None,
        },
    )
    .await
    .map_err(protection_write_error)?;

    Ok(rule)
}

/// Resolve both lists together, then preserve each list's first-occurrence ordering.
async fn write_user_ids(
    pool: &codeyard_db_core::DbPool,
    repository: &codeyard_db_core::models::Repository,
    lists: [&[String]; 2],
) -> Result<[Vec<i64>; 2], (StatusCode, Json<ApiError>)> {
    let fields = ["push_allowances", "force_push_allowances"];
    for (names, field) in lists.iter().zip(fields) {
        if names.len() > db::repositories::protection::MAX_ALLOWANCE_ACTORS {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(
                    "validation_error",
                    &format!(
                        "{field} can list at most {} entries",
                        db::repositories::protection::MAX_ALLOWANCE_ACTORS
                    ),
                )),
            ));
        }
    }
    let names: HashSet<String> = lists
        .iter()
        .flat_map(|names| names.iter())
        .map(|name| name.trim().to_owned())
        .collect();
    let users =
        db::users::accounts::get_users_by_usernames(pool, &names.into_iter().collect::<Vec<_>>())
            .await
            .map_err(|_| storage_unavailable())?;
    let ids: Vec<_> = users.iter().map(|user| user.id).collect();
    let allowed = db::repositories::access::write_access_user_ids(pool, repository.id, &ids)
        .await
        .map_err(|_| storage_unavailable())?;
    let users: std::collections::HashMap<_, _> = users
        .into_iter()
        .filter(|user| allowed.contains(&user.id))
        .map(|user| (user.username, user.id))
        .collect();
    let mut result = [Vec::new(), Vec::new()];
    for ((names, ids), field) in lists.into_iter().zip(&mut result).zip(fields) {
        for name in names {
            let Some(id) = users.get(name.trim()) else {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ApiError::new(
                        "validation_error",
                        &format!("{field} must name users with write access to this repository"),
                    )),
                ));
            };
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
    }
    Ok(result)
}

pub async fn delete_branch_protection_rule(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, pattern)): Path<(String, String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &actor).await?;

    delete_rule(
        &state.pool,
        crate::backend::db::credentials::Actor::from((
            actor.id,
            crate::backend::api::http::session::extract_session_id(&headers).unwrap_or_default(),
        )),
        repository.id,
        &pattern,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes the rule saved for `pattern`. The JSON API and the settings form both delete through
/// this, so a missing rule is the same not-found answer in both.
pub(in crate::backend::api::repos) async fn delete_rule(
    pool: &codeyard_db_core::DbPool,
    actor_id: impl Into<db::credentials::Actor>,
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
