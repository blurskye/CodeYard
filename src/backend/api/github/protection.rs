//! Classic branch protection shares the rules enforced by Git pushes and merges.
use super::{auth::read_method, contents::store_error, repo_url, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        models::BranchProtectionRule,
        repositories::protection::{self, ProtectionUpdate},
    },
    repositories::{access, store::repo_store},
    webhooks::payloads,
};
use axum::{
    extract::{FromRequest, Path, Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn write_error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<protection::ProtectionWriteRefusal>() {
        Some(protection::ProtectionWriteRefusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Branch protection write denied")
        }
        Some(protection::ProtectionWriteRefusal::NotFound) => Error::missing(),
        Some(_) => Error::invalid("Invalid branch protection"),
        None => Error::unavailable(()),
    }
}

pub(super) async fn protected(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    branch: &str,
    write: bool,
) -> Result<(BranchProtectionRule, i64, String)> {
    let (repo, auth) = repository(state, headers, owner, name, write).await?;
    let actor = auth.ok_or_else(super::auth::required)?.user;
    if !access::can_admin(&state.pool, &actor, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    let store = repo_store(&repo, state.cluster.as_ref());
    let refs = if write {
        store.refs_from_primary().await
    } else {
        store.refs().await
    }
    .map_err(store_error)?;
    if !refs.contains_key(&format!("refs/heads/{branch}")) {
        return Err(Error::missing());
    }
    let rule = protection::list_branch_protection_rules(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .find(|rule| rule.pattern == branch)
        .ok_or_else(Error::missing)?;
    let url = format!(
        "{}/branches/{}/protection",
        repo_url(owner, name),
        crate::webui::encode_component(branch)
    );
    Ok((rule, actor.id, url))
}

fn checks(rule: &BranchProtectionRule, url: &str) -> Value {
    let url = format!("{url}/required_status_checks");
    json!({"url": url, "strict": rule.require_up_to_date_branch, "contexts_url": format!("{url}/contexts"),
        "contexts": rule.required_status_checks,
        "checks": rule.required_status_checks.iter().map(|context| rule.github_settings.checks.iter().find(|check| check["context"] == *context).cloned().unwrap_or_else(|| json!({"context": context, "app_id": null}))).collect::<Vec<_>>()})
}

fn admins(rule: &BranchProtectionRule, url: &str) -> Value {
    json!({"url": format!("{url}/enforce_admins"), "enabled": rule.enforce_admins})
}

pub(super) fn review_document(rule: &BranchProtectionRule, url: &str) -> Value {
    let mut value = json!({"url": format!("{url}/required_pull_request_reviews"),
        "dismiss_stale_reviews": rule.dismiss_stale_reviews, "require_code_owner_reviews": rule.require_code_owner_review,
        "required_approving_review_count": rule.required_approving_reviews,
        "require_last_push_approval": rule.github_settings.require_last_push_approval});
    if let Some(restrictions) = &rule.github_settings.dismissal_restrictions {
        let mut restrictions = restrictions.clone();
        let path = format!("{url}/required_pull_request_reviews/dismissal_restrictions");
        restrictions["url"] = json!(path);
        restrictions["users_url"] = json!(format!("{path}/users"));
        restrictions["teams_url"] = json!(format!("{path}/teams"));
        value["dismissal_restrictions"] = restrictions;
    }
    if let Some(allowances) = &rule.github_settings.bypass_pull_request_allowances {
        value["bypass_pull_request_allowances"] = allowances.clone();
    }
    value
}

async fn restriction_document(
    state: &AppState,
    rule: &BranchProtectionRule,
    url: &str,
    actor: i64,
) -> Result<Value> {
    let users = db::users::accounts::get_users_by_ids(&state.pool, &rule.push_allowances)
        .await
        .map_err(Error::unavailable)?;
    let teams = super::push_teams::documents(state, rule, actor).await?;
    Ok(json!({"url": format!("{url}/restrictions"),
        "users_url": format!("{url}/restrictions/users"), "teams_url": format!("{url}/restrictions/teams"), "apps_url": format!("{url}/restrictions/apps"),
        "users": users.iter().map(|user| payloads::person(&payloads::Person::of(user))).collect::<Vec<_>>(), "teams": teams, "apps": super::protection_operations::app_documents(state, &rule.github_settings.push_apps).await?}))
}

pub(super) async fn document(
    state: &AppState,
    rule: &BranchProtectionRule,
    url: &str,
    actor: i64,
) -> Result<Value> {
    let mut value = json!({"url": url, "enforce_admins": admins(rule, url),
        "required_linear_history": {"enabled": rule.require_linear_history},
        "allow_force_pushes": {"enabled": !rule.block_force_pushes},
        "allow_deletions": {"enabled": !rule.block_deletions},
        "required_conversation_resolution": {"enabled": rule.require_conversation_resolution},
        "required_signatures": {"url": format!("{url}/required_signatures"), "enabled": rule.github_settings.required_signatures},
        "block_creations": {"enabled": rule.github_settings.block_creations},
        "lock_branch": {"enabled": rule.github_settings.lock_branch},
        "allow_fork_syncing": {"enabled": rule.github_settings.allow_fork_syncing}});
    if rule.require_status_checks {
        value["required_status_checks"] = checks(rule, url);
    }
    if protection::has_review_requirements(rule) {
        value["required_pull_request_reviews"] = review_document(rule, url);
    }
    if rule.restrict_pushes {
        value["restrictions"] = restriction_document(state, rule, url, actor).await?;
    }
    Ok(value)
}

async fn branch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
    method: Method,
) -> Result<Response> {
    let (rule, actor, url) = protected(
        &state,
        &headers,
        &owner,
        &name,
        &branch,
        !read_method(&method),
    )
    .await?;
    if method == Method::DELETE {
        if !protection::delete_branch_protection_rule(
            &state.pool,
            actor,
            rule.repo_id,
            &rule.pattern,
        )
        .await
        .map_err(write_error)?
        {
            return Err(Error::missing());
        }
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(Json(document(&state, &rule, &url, actor).await?).into_response())
}

async fn admin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
    method: Method,
) -> Result<Response> {
    let (mut rule, actor, url) = protected(
        &state,
        &headers,
        &owner,
        &name,
        &branch,
        !read_method(&method),
    )
    .await?;
    if !read_method(&method) {
        rule = protection::update_branch_protection_rule(
            &state.pool,
            actor,
            rule.repo_id,
            &rule.pattern,
            ProtectionUpdate::EnforceAdmins(method == Method::POST),
        )
        .await
        .map_err(write_error)?;
    }
    if method == Method::DELETE {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(Json(admins(&rule, &url)).into_response())
}

#[derive(Deserialize)]
struct StatusChecks {
    strict: Option<bool>,
    contexts: Option<Vec<String>>,
    checks: Option<Vec<super::protection_operations::Check>>,
}

async fn status_checks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
    request: Request,
) -> Result<Response> {
    let method = request.method().clone();
    let (mut rule, actor, url) = protected(
        &state,
        &headers,
        &owner,
        &name,
        &branch,
        !read_method(&method),
    )
    .await?;
    if !rule.require_status_checks {
        return Err(Error::missing());
    }
    if method == Method::PATCH {
        let Json(input) = Json::<StatusChecks>::from_request(request, &state)
            .await
            .map_err(|error| Error::new(error.status(), "Invalid status check policy"))?;
        let mut contexts = input.contexts;
        let checks = input.checks.unwrap_or_default();
        let bindings = super::protection_operations::check_bindings(
            &state,
            rule.repo_id,
            contexts.as_deref().unwrap_or_default(),
            &checks,
            &rule.github_settings.checks,
        )
        .await?;
        if !checks.is_empty() {
            contexts
                .get_or_insert_with(Vec::new)
                .extend(checks.into_iter().map(|check| check.context));
        }
        rule = protection::update_branch_protection_rule(
            &state.pool,
            actor,
            rule.repo_id,
            &rule.pattern,
            ProtectionUpdate::StatusChecks {
                strict: input.strict,
                contexts,
                bindings,
            },
        )
        .await
        .map_err(write_error)?;
    } else if method == Method::DELETE {
        protection::update_branch_protection_rule(
            &state.pool,
            actor,
            rule.repo_id,
            &rule.pattern,
            ProtectionUpdate::RemoveStatusChecks,
        )
        .await
        .map_err(write_error)?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(Json(checks(&rule, &url)).into_response())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Contexts {
    Array(Vec<String>),
    Object { contexts: Vec<String> },
}

async fn contexts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
    request: Request,
) -> Result<Json<Value>> {
    let method = request.method().clone();
    let (mut rule, actor, _) = protected(
        &state,
        &headers,
        &owner,
        &name,
        &branch,
        !read_method(&method),
    )
    .await?;
    if !rule.require_status_checks {
        return Err(Error::missing());
    }
    if !read_method(&method) {
        let Json(input) = Json::<Contexts>::from_request(request, &state)
            .await
            .map_err(|error| Error::new(error.status(), "Invalid status check contexts"))?;
        let values = match input {
            Contexts::Array(values) | Contexts::Object { contexts: values } => values,
        };
        let bindings = if method == Method::DELETE {
            Vec::new()
        } else {
            super::protection_operations::check_bindings(
                &state,
                rule.repo_id,
                &values,
                &[],
                &rule.github_settings.checks,
            )
            .await?
        };
        let update = match method {
            Method::POST => ProtectionUpdate::AddContexts {
                contexts: values,
                bindings,
            },
            Method::PUT => ProtectionUpdate::SetContexts {
                contexts: values,
                bindings,
            },
            _ => ProtectionUpdate::RemoveContexts(values),
        };
        rule = protection::update_branch_protection_rule(
            &state.pool,
            actor,
            rule.repo_id,
            &rule.pattern,
            update,
        )
        .await
        .map_err(write_error)?;
    }
    Ok(Json(json!(rule.required_status_checks)))
}

async fn review_requirements(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
    method: Method,
) -> Result<Response> {
    let (rule, actor, url) = protected(
        &state,
        &headers,
        &owner,
        &name,
        &branch,
        !read_method(&method),
    )
    .await?;
    if !protection::has_review_requirements(&rule) {
        return Err(Error::missing());
    }
    if method == Method::DELETE {
        protection::update_branch_protection_rule(
            &state.pool,
            actor,
            rule.repo_id,
            &rule.pattern,
            ProtectionUpdate::RemovePullRequestReviews,
        )
        .await
        .map_err(write_error)?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(Json(review_document(&rule, &url)).into_response())
}

async fn restrictions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
    method: Method,
) -> Result<Response> {
    let (rule, actor, url) = protected(
        &state,
        &headers,
        &owner,
        &name,
        &branch,
        !read_method(&method),
    )
    .await?;
    if !rule.restrict_pushes {
        return Err(Error::missing());
    }
    if method == Method::DELETE {
        protection::update_branch_protection_rule(
            &state.pool,
            actor,
            rule.repo_id,
            &rule.pattern,
            ProtectionUpdate::RemovePushRestrictions,
        )
        .await
        .map_err(write_error)?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(Json(restriction_document(&state, &rule, &url, actor).await?).into_response())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Users {
    Array(Vec<String>),
    Object { users: Vec<String> },
}

async fn users(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
    method: Method,
    request: Request,
) -> Result<Response> {
    let (mut rule, actor, url) = protected(
        &state,
        &headers,
        &owner,
        &name,
        &branch,
        !read_method(&method),
    )
    .await?;
    if !rule.restrict_pushes {
        return Err(Error::missing());
    }
    if !read_method(&method) {
        let Json(input) = Json::<Users>::from_request(request, &state)
            .await
            .map_err(|error| Error::new(error.status(), "Invalid user restrictions"))?;
        let names = match input {
            Users::Array(names) | Users::Object { users: names } => names,
        };
        let ids = super::protection_operations::user_ids(&state, &names).await?;
        use protection::ActorListUpdate;
        let update = match method {
            Method::POST => ActorListUpdate::Add(ids),
            Method::PUT => ActorListUpdate::Set(ids),
            Method::DELETE => ActorListUpdate::Remove(ids),
            _ => unreachable!(),
        };
        rule = protection::update_branch_protection_rule(
            &state.pool,
            actor,
            rule.repo_id,
            &rule.pattern,
            ProtectionUpdate::PushUsers(update),
        )
        .await
        .map_err(write_error)?;
    }
    Ok(Json(restriction_document(&state, &rule, &url, actor).await?["users"].clone()).into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "repos/get-pull-request-review-protection",
            get(review_requirements),
        ),
        (
            "repos/delete-pull-request-review-protection",
            axum::routing::delete(review_requirements),
        ),
        ("repos/get-access-restrictions", get(restrictions)),
        (
            "repos/delete-access-restrictions",
            axum::routing::delete(restrictions),
        ),
        (
            "repos/get-users-with-access-to-protected-branch",
            get(users),
        ),
        (
            "repos/add-user-access-restrictions",
            axum::routing::post(users),
        ),
        (
            "repos/set-user-access-restrictions",
            axum::routing::put(users),
        ),
        (
            "repos/remove-user-access-restrictions",
            axum::routing::delete(users),
        ),
        ("repos/get-branch-protection", get(branch)),
        (
            "repos/delete-branch-protection",
            axum::routing::delete(branch),
        ),
        ("repos/get-admin-branch-protection", get(admin)),
        (
            "repos/set-admin-branch-protection",
            axum::routing::post(admin),
        ),
        (
            "repos/delete-admin-branch-protection",
            axum::routing::delete(admin),
        ),
        ("repos/get-status-checks-protection", get(status_checks)),
        (
            "repos/update-status-check-protection",
            axum::routing::patch(status_checks),
        ),
        (
            "repos/remove-status-check-protection",
            axum::routing::delete(status_checks),
        ),
        ("repos/get-all-status-check-contexts", get(contexts)),
        (
            "repos/add-status-check-contexts",
            axum::routing::post(contexts),
        ),
        (
            "repos/set-status-check-contexts",
            axum::routing::put(contexts),
        ),
        (
            "repos/remove-status-check-contexts",
            axum::routing::delete(contexts),
        ),
    ]
}
