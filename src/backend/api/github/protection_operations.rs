//! Additional classic protection settings and the rules derived from them.
use super::{
    auth::read_method,
    contents::store_error,
    protection::{document, protected, review_document, write_error},
    repo_url, repository, Error, Result,
};
use crate::backend::{
    app::AppState,
    db::{
        self,
        models::BranchProtectionSettings,
        repositories::protection::{
            self, ActorListUpdate, BranchProtectionRuleUpsert, ProtectionUpdate,
            ReviewProtectionUpdate,
        },
    },
    repositories::{access, store::repo_store},
    webhooks::payloads,
};
use axum::{
    extract::{FromRequest, Path, Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Default, Deserialize)]
struct Actors {
    #[serde(default)]
    users: Vec<String>,
    #[serde(default)]
    teams: Vec<String>,
    #[serde(default)]
    apps: Vec<String>,
}

#[derive(Default, Deserialize)]
struct Reviews {
    dismiss_stale_reviews: Option<bool>,
    require_code_owner_reviews: Option<bool>,
    required_approving_review_count: Option<i32>,
    require_last_push_approval: Option<bool>,
    dismissal_restrictions: Option<Actors>,
    bypass_pull_request_allowances: Option<Actors>,
}

#[derive(Deserialize)]
struct Checks {
    strict: bool,
    #[serde(default)]
    contexts: Vec<String>,
    #[serde(default)]
    checks: Vec<Check>,
}

#[derive(Deserialize)]
pub(super) struct Check {
    context: String,
    app_id: Option<i64>,
}

#[derive(Deserialize)]
struct Protection {
    required_status_checks: Option<Checks>,
    enforce_admins: Option<bool>,
    required_pull_request_reviews: Option<Reviews>,
    restrictions: Option<Actors>,
    #[serde(default)]
    required_linear_history: bool,
    allow_force_pushes: Option<bool>,
    #[serde(default)]
    allow_deletions: bool,
    #[serde(default)]
    block_creations: bool,
    #[serde(default)]
    required_conversation_resolution: bool,
    #[serde(default)]
    lock_branch: bool,
    #[serde(default)]
    allow_fork_syncing: bool,
}

fn valid_names(names: &[String]) -> Result<()> {
    if names.len() > 100
        || names
            .iter()
            .any(|name| name.is_empty() || name.len() > 255 || name.chars().any(char::is_control))
    {
        return Err(Error::invalid("Supply at most 100 valid names"));
    }
    Ok(())
}

/// The accounts named by a protection allowance list. Usernames are exact, as the native
/// account namespace stores and compares them.
pub(super) async fn user_ids(state: &AppState, names: &[String]) -> Result<Vec<i64>> {
    valid_names(names)?;
    let mut names = names.to_vec();
    names.sort_unstable();
    names.dedup();
    let users = db::users::accounts::get_users_by_usernames(&state.pool, &names)
        .await
        .map_err(Error::unavailable)?;
    if users.len() != names.len() {
        return Err(Error::invalid("Unknown user"));
    }
    Ok(users.into_iter().map(|user| user.id).collect())
}

async fn app_ids(state: &AppState, repo: i64, names: &[String]) -> Result<Vec<i64>> {
    valid_names(names)?;
    let mut ids = Vec::new();
    for name in names {
        let app = db::github_apps::app(&state.pool, name)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(|| Error::invalid("Unknown app"))?;
        if db::github_apps::repository_installation(&state.pool, app.id, repo, true, None)
            .await
            .map_err(Error::unavailable)?
            .is_none()
        {
            return Err(Error::invalid("App is not installed on this repository"));
        }
        ids.push(app.id);
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

pub(super) async fn app_documents(state: &AppState, ids: &[i64]) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    for id in ids {
        if let Some(app) = db::github_apps::app(&state.pool, &id.to_string())
            .await
            .map_err(Error::unavailable)?
        {
            values.push(super::apps::installations::app_document(state, &app).await?);
        }
    }
    Ok(values)
}

async fn actor_document(state: &AppState, repo: i64, actors: Actors) -> Result<Value> {
    if actors.users.len() + actors.teams.len() + actors.apps.len() > 100 {
        return Err(Error::invalid("Supply at most 100 actors"));
    }
    let ids = user_ids(state, &actors.users).await?;
    let users = db::users::accounts::get_users_by_ids(&state.pool, &ids)
        .await
        .map_err(Error::unavailable)?;
    let teams = db::repositories::push_teams::resolve_names(&state.pool, repo, &actors.teams)
        .await
        .map_err(write_error)?;
    let rows = db::repositories::push_teams::by_ids(&state.pool, repo, &teams)
        .await
        .map_err(Error::unavailable)?;
    let mut teams = Vec::new();
    for row in rows {
        teams.push(super::teams::summary(state, &row.team, &row.org_slug, row.permission).await?);
    }
    let apps = app_documents(state, &app_ids(state, repo, &actors.apps).await?).await?;
    Ok(
        json!({"users": users.iter().map(payloads::user).collect::<Vec<_>>(), "teams": teams, "apps": apps}),
    )
}

async fn review_update(
    state: &AppState,
    repo: i64,
    input: Reviews,
) -> Result<ReviewProtectionUpdate> {
    if input
        .required_approving_review_count
        .is_some_and(|count| !(0..=6).contains(&count))
    {
        return Err(Error::invalid("Review count must be between 0 and 6"));
    }
    Ok(ReviewProtectionUpdate {
        dismiss_stale_reviews: input.dismiss_stale_reviews,
        require_code_owner_reviews: input.require_code_owner_reviews,
        required_approving_review_count: input.required_approving_review_count,
        require_last_push_approval: input.require_last_push_approval,
        dismissal_restrictions: match input.dismissal_restrictions {
            Some(actors) => Some(actor_document(state, repo, actors).await?),
            None => None,
        },
        bypass_pull_request_allowances: match input.bypass_pull_request_allowances {
            Some(actors) => Some(actor_document(state, repo, actors).await?),
            None => None,
        },
    })
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
    request: Request,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(super::auth::required)?;
    let actor = auth.user;
    if !access::can_admin(&state.pool, &actor, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    if !repo_store(&repo, state.cluster.as_ref())
        .refs_from_primary()
        .await
        .map_err(store_error)?
        .contains_key(&format!("refs/heads/{branch}"))
    {
        return Err(Error::missing());
    }
    let Json(value) = Json::<Value>::from_request(request, &state)
        .await
        .map_err(|_| Error::invalid_simple("Invalid branch protection"))?;
    for field in [
        "required_status_checks",
        "enforce_admins",
        "required_pull_request_reviews",
        "restrictions",
    ] {
        if value.get(field).is_none() {
            return Err(Error::invalid_simple(format!("Missing field: {field}")));
        }
    }
    let input: Protection = serde_json::from_value(value)
        .map_err(|_| Error::invalid_simple("Invalid branch protection"))?;
    let mut settings = BranchProtectionSettings {
        block_creations: input.block_creations,
        lock_branch: input.lock_branch,
        allow_fork_syncing: input.allow_fork_syncing,
        ..Default::default()
    };
    let mut upsert = BranchProtectionRuleUpsert {
        actor_id: actor.id,
        token_id: Some(auth.token.id),
        repo_id: repo.id,
        pattern: &branch,
        require_pull_request: input.required_pull_request_reviews.is_some(),
        dismiss_stale_reviews: false,
        enforce_admins: input.enforce_admins.unwrap_or(false),
        require_linear_history: input.required_linear_history,
        block_force_pushes: !input.allow_force_pushes.unwrap_or(false),
        block_deletions: !input.allow_deletions,
        require_conversation_resolution: input.required_conversation_resolution,
        restrict_pushes: input.restrictions.is_some(),
        ..Default::default()
    };
    if let Some(checks) = input.required_status_checks {
        upsert.require_status_checks = true;
        upsert.require_up_to_date_branch = checks.strict;
        settings.checks = check_bindings(&state, repo.id, &checks.contexts, &checks.checks, &[])
            .await
            .map_err(Error::simple)?;
        upsert.required_status_checks = checks.contexts;
        upsert
            .required_status_checks
            .extend(checks.checks.into_iter().map(|check| check.context));
        upsert.required_status_checks.sort();
        upsert.required_status_checks.dedup();
    }
    if let Some(reviews) = input.required_pull_request_reviews {
        let reviews = review_update(&state, repo.id, reviews)
            .await
            .map_err(Error::simple)?;
        upsert.dismiss_stale_reviews = reviews.dismiss_stale_reviews.unwrap_or(false);
        upsert.require_code_owner_review = reviews.require_code_owner_reviews.unwrap_or(false);
        upsert.required_approving_reviews = reviews.required_approving_review_count.unwrap_or(0);
        settings.require_last_push_approval = reviews.require_last_push_approval.unwrap_or(false);
        settings.dismissal_restrictions = reviews.dismissal_restrictions;
        settings.bypass_pull_request_allowances = reviews.bypass_pull_request_allowances;
    }
    if let Some(actors) = input.restrictions {
        upsert.push_allowances = user_ids(&state, &actors.users)
            .await
            .map_err(Error::simple)?;
        upsert.push_team_allowances =
            db::repositories::push_teams::resolve_names(&state.pool, repo.id, &actors.teams)
                .await
                .map_err(|error| write_error(error).simple())?;
        settings.push_apps = app_ids(&state, repo.id, &actors.apps)
            .await
            .map_err(Error::simple)?;
    }
    upsert.github_settings = Some(settings);
    let rule = protection::upsert_branch_protection_rule(&state.pool, upsert)
        .await
        .map_err(|error| write_error(error).simple())?;
    let url = format!(
        "{}/branches/{}/protection",
        repo_url(&owner, &name),
        crate::webui::encode_component(&branch)
    );
    Ok(Json(document(&state, &rule, &url).await?))
}

async fn reviews(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
    Json(input): Json<Reviews>,
) -> Result<Json<Value>> {
    let (rule, actor, url) = protected(&state, &headers, &owner, &name, &branch, true).await?;
    let update = review_update(&state, rule.repo_id, input).await?;
    let rule = protection::update_branch_protection_rule(
        &state.pool,
        actor,
        rule.repo_id,
        &branch,
        ProtectionUpdate::Reviews(update),
    )
    .await
    .map_err(write_error)?;
    Ok(Json(review_document(&rule, &url)))
}

async fn signatures(
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
            &branch,
            ProtectionUpdate::RequiredSignatures(method == Method::POST),
        )
        .await
        .map_err(write_error)?;
    }
    if method == Method::DELETE {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(Json(json!({"url":format!("{url}/required_signatures"),"enabled":rule.github_settings.required_signatures})).into_response())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Apps {
    Array(Vec<String>),
    Object { apps: Vec<String> },
}

async fn apps(
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
    if !rule.restrict_pushes {
        return Err(Error::missing());
    }
    if !read_method(&method) {
        let Json(input) = Json::<Apps>::from_request(request, &state)
            .await
            .map_err(|_| Error::invalid("Expected app slugs"))?;
        let (Apps::Array(names) | Apps::Object { apps: names }) = input;
        let ids = if method == Method::DELETE {
            valid_names(&names)?;
            let mut ids = Vec::new();
            for name in names {
                if let Some(app) = db::github_apps::app(&state.pool, &name)
                    .await
                    .map_err(Error::unavailable)?
                {
                    ids.push(app.id);
                }
            }
            ids
        } else {
            app_ids(&state, rule.repo_id, &names).await?
        };
        let update = match method {
            Method::POST => ActorListUpdate::Add(ids),
            Method::PUT => ActorListUpdate::Set(ids),
            _ => ActorListUpdate::Remove(ids),
        };
        rule = protection::update_branch_protection_rule(
            &state.pool,
            actor,
            rule.repo_id,
            &branch,
            ProtectionUpdate::PushApps(update),
        )
        .await
        .map_err(write_error)?;
    }
    Ok(Json(json!(
        app_documents(&state, &rule.github_settings.push_apps).await?
    )))
}

async fn rules(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let rules =
        crate::backend::repositories::protection::matching_rules(&state.pool, &repo, &branch)
            .await
            .map_err(Error::unavailable)?;
    let mut values = Vec::new();
    for rule in rules {
        for (enabled, kind) in [
            (rule.block_deletions, "deletion"),
            (rule.block_force_pushes, "non_fast_forward"),
            (rule.require_linear_history, "required_linear_history"),
            (
                rule.github_settings.required_signatures,
                "required_signatures",
            ),
            (rule.github_settings.block_creations, "creation"),
            (rule.github_settings.lock_branch, "update"),
        ] {
            if enabled {
                values.push(json!({"type":kind}));
            }
        }
        if protection::has_review_requirements(&rule) || rule.require_conversation_resolution {
            values.push(json!({"type":"pull_request","parameters":{
                "dismiss_stale_reviews_on_push":rule.dismiss_stale_reviews,"require_code_owner_review":rule.require_code_owner_review,
                "require_last_push_approval":rule.github_settings.require_last_push_approval,"required_approving_review_count":rule.required_approving_reviews,
                "required_review_thread_resolution":rule.require_conversation_resolution}}));
        }
        if rule.require_status_checks {
            let checks: Vec<_> = rule
                .required_status_checks
                .iter()
                .map(|context| {
                    let mut check = json!({"context":context});
                    if let Some(id) = rule
                        .github_settings
                        .checks
                        .iter()
                        .find(|check| check["context"] == *context)
                        .and_then(|check| check["app_id"].as_i64())
                    {
                        check["integration_id"] = json!(id);
                    }
                    check
                })
                .collect();
            values.push(json!({"type":"required_status_checks","parameters":{"strict_required_status_checks_policy":rule.require_up_to_date_branch,
                "required_status_checks":checks}}));
        }
    }
    Ok(Json(json!(values)))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/update-branch-protection", put(update)),
        (
            "repos/update-pull-request-review-protection",
            patch(reviews),
        ),
        ("repos/get-commit-signature-protection", get(signatures)),
        ("repos/create-commit-signature-protection", post(signatures)),
        (
            "repos/delete-commit-signature-protection",
            delete(signatures),
        ),
        ("repos/get-apps-with-access-to-protected-branch", get(apps)),
        ("repos/add-app-access-restrictions", post(apps)),
        ("repos/set-app-access-restrictions", put(apps)),
        ("repos/remove-app-access-restrictions", delete(apps)),
        ("repos/get-branch-rules", get(rules)),
    ]
}

/// Producer bindings for required checks. An explicit `app_id` pins that known App and -1 allows
/// any producer. An omitted one, and a deprecated `contexts` entry without an existing binding,
/// pins the most recent App producer, or allows any producer when no App has reported it.
pub(super) async fn check_bindings(
    state: &AppState,
    repo: i64,
    contexts: &[String],
    checks: &[Check],
    existing: &[Value],
) -> Result<Vec<Value>> {
    if contexts.len() + checks.len() > 100 {
        return Err(Error::invalid("Supply at most 100 status checks"));
    }
    let mut bindings: Vec<Value> = Vec::new();
    let bound = |bindings: &[Value], context: &str| {
        bindings
            .iter()
            .chain(existing)
            .any(|binding| binding["context"] == context)
    };
    for check in checks {
        let app = match check.app_id {
            Some(-1) => None,
            Some(id) => {
                db::github_apps::app(&state.pool, &id.to_string())
                    .await
                    .map_err(Error::unavailable)?
                    .filter(|app| app.id == id)
                    .ok_or_else(|| Error::invalid("Unknown app"))?;
                Some(id)
            }
            None => recent_producer(state, repo, &check.context).await?,
        };
        bindings.push(json!({"context": check.context, "app_id": app}));
    }
    for context in contexts {
        if bound(&bindings, context) {
            continue;
        }
        if let Some(app) = recent_producer(state, repo, context).await? {
            bindings.push(json!({"context": context, "app_id": app}));
        }
    }
    protection::normalize_check_bindings(&mut bindings).map_err(write_error)?;
    Ok(bindings)
}

async fn recent_producer(state: &AppState, repo: i64, context: &str) -> Result<Option<i64>> {
    db::checks::recent_app(&state.pool, repo, context)
        .await
        .map_err(Error::unavailable)
}
