//! Settings pages for fine-grained personal access tokens. Every change goes through
//! `identity::fine_grained`, which checks the session, the owner and the policy.
use crate::backend::api::http::{context::PageContext, session::extract_session_id};
use crate::backend::api::user::responses::{field, mutation_guard, require_account, Fields};
use crate::backend::app::AppState;
use crate::backend::db::{
    self,
    fine_grained_tokens::Token,
    models::User,
    organizations::{self, OrganizationOutcome, OrganizationPage},
};
use crate::backend::identity::{
    fine_grained::{self, catalog, Access, Group, Owner, Request, TokenError, PERMISSIONS},
    tokens,
};
use crate::webui::app::Page;
use crate::webui::pages::{
    account,
    settings::fine_grained::{Choice, Choices, Detail, Draft, Model, Row, View, BASE},
};
use axum::extract::{rejection::FormRejection, Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{Map, Value};
use std::{collections::HashMap, sync::Arc};

pub(super) fn router() -> Router<Arc<AppState>> {
    let back = || get(|| async { crate::backend::api::http::render::redirect_response(BASE) });
    Router::new()
        .route(BASE, get(list).post(create))
        .route("/settings/personal-access-tokens/new", get(new))
        .route("/settings/personal-access-tokens/:id", get(show))
        .route(
            "/settings/personal-access-tokens/:id/regenerate",
            back().post(regenerate),
        )
        .route(
            "/settings/personal-access-tokens/:id/delete",
            back().post(delete),
        )
}

const UNAVAILABLE: &str = "Token settings are temporarily unavailable.";
const MISSING: &str = "Token not found or not owned by you.";

fn respond(ctx: &PageContext, status: StatusCode, view: View, message: Option<&str>) -> Response {
    ctx.page_model_status(
        status,
        "Fine-grained tokens",
        Page::Identity(account::Page::FineGrainedTokens {
            model: Box::new(Model {
                csrf_token: ctx.csrf_token.clone(),
                view,
                message: message.map(str::to_owned),
                is_error: !status.is_success(),
            }),
        }),
    )
}

fn refusal(error: TokenError) -> (StatusCode, &'static str) {
    match error {
        TokenError::Invalid(message) => (StatusCode::BAD_REQUEST, message),
        TokenError::Denied => (
            StatusCode::FORBIDDEN,
            "Sign in again, and check that you still belong to the organization.",
        ),
        TokenError::Missing => (StatusCode::NOT_FOUND, MISSING),
        TokenError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
    }
}

/// The signed-in account, sent to the password page first when it must change its password.
#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
fn account(ctx: &PageContext) -> Result<User, Response> {
    let user = require_account(ctx, false)?;
    if user.must_change_password {
        return Err(ctx.redirect("/settings?tab=password"));
    }
    Ok(user.clone())
}

/// A day's last second in UTC, as for classic tokens.
fn end_of(day: NaiveDate) -> Option<DateTime<Utc>> {
    day.and_hms_opt(23, 59, 59).map(|at| at.and_utc())
}

/// The form's expiry: a number of days from today, a custom day, or `never`.
fn expiry(choice: &str, date: &str, now: DateTime<Utc>) -> Option<Option<DateTime<Utc>>> {
    match choice {
        "never" => Some(None),
        "custom" => end_of(NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?).map(Some),
        "7" | "30" | "60" | "90" => {
            end_of(now.date_naive() + Duration::days(choice.parse().ok()?)).map(Some)
        }
        _ => None,
    }
}

fn expires(at: Option<DateTime<Utc>>) -> String {
    at.map_or_else(|| "never".to_owned(), |at| at.to_rfc3339())
}

fn row(token: &Token, owner: String) -> Row {
    Row {
        id: token.token_id,
        name: token.name.clone(),
        owner,
        status: token.status.clone(),
        expires: expires(token.expires_at),
        last_used: token.last_used_at.map(|at| at.to_rfc3339()),
        last_ip: token.last_used_ip.clone(),
    }
}

fn group_label(group: Group) -> &'static str {
    match group {
        Group::Repository => "Repository",
        Group::Organization => "Organization",
        Group::Account => "Account",
    }
}

async fn owner_name(ctx: &PageContext, user: &User, org: Option<i64>) -> anyhow::Result<String> {
    Ok(match org {
        None => user.username.clone(),
        Some(org) => organizations::get_public_organization_by_id(&ctx.state.pool, org)
            .await?
            .map_or_else(|| "Deleted organization".to_owned(), |org| org.name),
    })
}

async fn list(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let user = match account(&ctx) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let before = ctx.query.get("before").and_then(|value| value.parse::<i64>().ok()).unwrap_or(0);
    let Ok(rows) = db::fine_grained_tokens::list(&ctx.state.pool, user.id, before).await else {
        return ctx.error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE);
    };
    let next_before = (rows.len() == 100).then(|| rows.last().map(|row| row.token_id)).flatten();
    let mut names = HashMap::new();
    let mut tokens = Vec::new();
    for token in rows {
        let name = match names.get(&token.owner_org_id) {
            Some(name) => String::clone(name),
            None => match owner_name(&ctx, &user, token.owner_org_id).await {
                Ok(name) => {
                    names.insert(token.owner_org_id, name.clone());
                    name
                }
                Err(_) => return ctx.error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
            },
        };
        tokens.push(row(&token, name));
    }
    let message = ctx
        .query
        .contains_key("deleted")
        .then_some("Token deleted.");
    respond(&ctx, StatusCode::OK, View::List { tokens, next_before }, message)
}

/// The organizations the user belongs to, which can own their tokens.
async fn organizations_of(
    ctx: &PageContext,
    user: &User,
) -> anyhow::Result<Vec<organizations::OrganizationMetadata>> {
    let page = OrganizationPage::new(0, 100).map_err(|_| anyhow::anyhow!("page"))?;
    Ok(
        match organizations::list_actor_organizations(&ctx.state.pool, user.id, page).await? {
            OrganizationOutcome::Applied(orgs) => orgs,
            _ => Vec::new(),
        },
    )
}

/// The owner a draft names, when it is the user or one of their organizations.
fn owner_of(
    draft: &Draft,
    user: &User,
    orgs: &[organizations::OrganizationMetadata],
) -> Option<Owner> {
    if draft.owner == "user" || draft.owner.is_empty() {
        return Some(Owner::User(user.id));
    }
    let id: i64 = draft.owner.parse().ok()?;
    orgs.iter()
        .any(|org| org.id == id)
        .then_some(Owner::Organization(id))
}

/// What the create form offers for the draft's owner and search.
async fn choices(ctx: &PageContext, user: &User, draft: &Draft) -> anyhow::Result<Choices> {
    let pool = &ctx.state.pool;
    let orgs = organizations_of(ctx, user).await?;
    let mut owners = vec![("user".to_owned(), user.username.clone())];
    owners.extend(
        orgs.iter()
            .map(|org| (org.id.to_string(), org.name.clone())),
    );
    let owner = owner_of(draft, user, &orgs).unwrap_or(Owner::User(user.id));
    let query = draft.query.trim().to_lowercase();
    let mut repositories = Vec::new();
    let mut more_repositories = false;
    let prefix = match owner {
        Owner::User(_) => user.username.clone(),
        Owner::Organization(org) => orgs
            .iter()
            .find(|candidate| candidate.id == org)
            .map(|org| org.name.clone())
            .unwrap_or_default(),
    };
    let never_allowed = match owner {
        Owner::User(_) => {
            let (repos, total) = db::repositories::records::list_user_repository_page(
                pool, user.id, true, &query, 100, 0,
            )
            .await?;
            more_repositories = total > 100;
            repositories.extend(
                repos
                    .into_iter()
                    .map(|repo| (repo.id, format!("{prefix}/{}", repo.name))),
            );
            false
        }
        Owner::Organization(org) => {
            // Search the first thousand repositories the user can read.
            for offset in (0..1000).step_by(100) {
                let page =
                    OrganizationPage::new(offset, 100).map_err(|_| anyhow::anyhow!("page"))?;
                let repos = organizations::list_organization_profile_repositories(
                    pool,
                    org,
                    Some(user.id),
                    page,
                )
                .await?;
                let done = repos.len() < 100;
                for repo in repos {
                    if query.is_empty() || repo.name.to_lowercase().contains(&query) {
                        if repositories.len() == 100 {
                            more_repositories = true;
                            break;
                        }
                        repositories.push((repo.id, format!("{prefix}/{}", repo.name)));
                    }
                }
                if done || more_repositories {
                    break;
                }
            }
            fine_grained::max_lifetime(
                owner,
                Some(&db::fine_grained_tokens::policy(pool, org).await?),
            )
            .is_none()
        }
    };
    // Keep repositories the draft already selected, even when the search hides them.
    for id in draft
        .repositories
        .iter()
        .take(fine_grained::MAX_REPOSITORIES)
    {
        if repositories.iter().any(|(listed, _)| listed == id) {
            continue;
        }
        if let Some(repo) = db::repositories::records::get_repository_by_id(pool, *id).await? {
            let owned = match owner {
                Owner::User(user) => repo.owner_kind == "user" && repo.owner_user_id == Some(user),
                Owner::Organization(org) => {
                    repo.owner_kind == "org" && repo.owner_org_id == Some(org)
                }
            };
            if owned
                && crate::backend::repositories::access::can_read(pool, Some(user), &repo).await?
            {
                repositories.insert(0, (repo.id, format!("{prefix}/{}", repo.name)));
            }
        }
    }
    let permissions = PERMISSIONS
        .iter()
        .filter(|permission| permission.key != "metadata")
        .filter(|permission| match permission.group {
            Group::Repository => true,
            Group::Organization => matches!(owner, Owner::Organization(_)),
            Group::Account => matches!(owner, Owner::User(_)),
        })
        .map(|permission| Choice {
            key: permission.key.to_owned(),
            name: permission.name.to_owned(),
            group: group_label(permission.group).to_owned(),
            levels: permission
                .levels
                .iter()
                .map(|level| (*level).to_owned())
                .collect(),
        })
        .collect();
    Ok(Choices {
        owners,
        repositories,
        more_repositories,
        permissions,
        never_allowed,
    })
}

async fn form_page(
    ctx: &PageContext,
    user: &User,
    draft: Draft,
    status: StatusCode,
    message: Option<&str>,
) -> Response {
    match choices(ctx, user, &draft).await {
        Ok(choices) => respond(ctx, status, View::New { draft, choices }, message),
        Err(_) => ctx.error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
    }
}

async fn new(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let user = match account(&ctx) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let draft = Draft {
        owner: "user".to_owned(),
        expiry: "30".to_owned(),
        access: "public".to_owned(),
        ..Draft::default()
    };
    form_page(&ctx, &user, draft, StatusCode::OK, None).await
}

/// The form's fields. Repositories and permissions repeat, so the form is a list of pairs.
fn draft(pairs: &[(String, String)]) -> Draft {
    let mut draft = Draft::default();
    for (key, value) in pairs {
        match key.as_str() {
            "name" => draft.name.clone_from(value),
            "description" => draft.description.clone_from(value),
            "owner" => draft.owner.clone_from(value),
            "expiry" => draft.expiry.clone_from(value),
            "expires_at" => draft.expires_at.clone_from(value),
            "access" => draft.access.clone_from(value),
            "query" => draft.query.clone_from(value),
            "repositories" => {
                if let Ok(id) = value.parse() {
                    draft.repositories.push(id);
                }
            }
            key => {
                if let Some(permission) = key.strip_prefix("permission.") {
                    if value != "none" && catalog(permission).is_some() {
                        draft
                            .permissions
                            .insert(permission.to_owned(), value.clone());
                    }
                }
            }
        }
    }
    draft
}

/// The token request a draft describes. Permissions that do not fit the owner or the
/// repository access are left out, as the form hides them.
fn request(draft: &Draft, owner: Owner, now: DateTime<Utc>) -> Result<Request, &'static str> {
    let access =
        Access::parse(&draft.access).ok_or("Choose which repositories the token reaches.")?;
    let expires_at = expiry(&draft.expiry, &draft.expires_at, now)
        .ok_or("Choose an expiration, and a valid date for a custom one.")?;
    let permissions: Map<String, Value> = draft
        .permissions
        .iter()
        .filter(|(key, _)| {
            catalog(key).is_some_and(|permission| match permission.group {
                Group::Repository => access != Access::Public,
                Group::Organization => matches!(owner, Owner::Organization(_)),
                Group::Account => matches!(owner, Owner::User(_)),
            })
        })
        .map(|(key, level)| (key.clone(), Value::String(level.clone())))
        .collect();
    Ok(Request {
        name: draft.name.clone(),
        description: draft.description.clone(),
        owner,
        access,
        repositories: if access == Access::Selected {
            draft.repositories.clone()
        } else {
            Vec::new()
        },
        permissions,
        expires_at,
    })
}

fn bounded(pairs: &[(String, String)]) -> bool {
    pairs.len() <= 200
        && pairs
            .iter()
            .all(|(key, value)| key.len() <= 64 && value.len() <= 4096)
}

/// The single-valued fields, for the CSRF check.
fn singles(pairs: &[(String, String)]) -> Fields {
    pairs
        .iter()
        .filter(|(key, _)| key != "repositories")
        .cloned()
        .collect()
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    form: Result<Form<Vec<(String, String)>>, FormRejection>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let pairs = match form {
        Ok(Form(pairs)) if bounded(&pairs) => pairs,
        _ => {
            return ctx.error(
                StatusCode::BAD_REQUEST,
                "The token form is too large or malformed.",
            )
        }
    };
    let fields = singles(&pairs);
    if let Err(response) = mutation_guard(&ctx, &fields, false) {
        return response;
    }
    let user = match account(&ctx) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let draft = draft(&pairs);
    if field(&fields, "action") == "refresh" {
        return form_page(&ctx, &user, draft, StatusCode::OK, None).await;
    }
    let orgs = match organizations_of(&ctx, &user).await {
        Ok(orgs) => orgs,
        Err(_) => return ctx.error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
    };
    let Some(owner) = owner_of(&draft, &user, &orgs) else {
        return form_page(
            &ctx,
            &user,
            draft,
            StatusCode::BAD_REQUEST,
            Some("Choose yourself or one of your organizations as the owner."),
        )
        .await;
    };
    let request = match request(&draft, owner, Utc::now()) {
        Ok(request) => request,
        Err(message) => {
            return form_page(&ctx, &user, draft, StatusCode::BAD_REQUEST, Some(message)).await
        }
    };
    let session = extract_session_id(&ctx.headers);
    match fine_grained::create(&ctx.state.pool, user.id, session.as_deref(), request).await {
        Ok(created) => match detail(&ctx, &user, created.id).await {
            Ok(token) => {
                let message = (created.status == "pending").then_some(
                    "The organization must approve this token before it reaches private resources.",
                );
                respond(
                    &ctx,
                    StatusCode::OK,
                    View::Show {
                        token,
                        value: Some(created.value),
                    },
                    message,
                )
            }
            Err(_) => ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Your token was created, but its page could not be loaded. If its value was not shown, delete it and create another."),
        },
        Err(error) => {
            let (status, message) = refusal(error);
            form_page(&ctx, &user, draft, status, Some(message)).await
        }
    }
}

/// One of the user's tokens with its owner, repositories and permissions.
async fn detail(ctx: &PageContext, user: &User, id: i64) -> Result<Detail, TokenError> {
    let pool = &ctx.state.pool;
    let unavailable = |_| TokenError::Unavailable;
    let token = db::fine_grained_tokens::get(pool, id)
        .await
        .map_err(unavailable)?
        .filter(|token| token.user_id == user.id)
        .ok_or(TokenError::Missing)?;
    let owner = owner_name(ctx, user, token.owner_org_id)
        .await
        .map_err(unavailable)?;
    let never_allowed = match token.owner_org_id {
        Some(org) => fine_grained::max_lifetime(
            Owner::Organization(org),
            Some(
                &db::fine_grained_tokens::policy(pool, org)
                    .await
                    .map_err(unavailable)?,
            ),
        )
        .is_none(),
        None => false,
    };
    let repositories = if token.repository_access == "selected" {
        db::fine_grained_tokens::repositories(pool, token.token_id)
            .await
            .map_err(unavailable)?
            .into_iter()
            .map(|(_, name)| name)
            .collect()
    } else {
        Vec::new()
    };
    let stored: Map<String, Value> = serde_json::from_str(&token.permissions).unwrap_or_default();
    let permissions = PERMISSIONS
        .iter()
        .filter_map(|permission| {
            let level = stored.get(permission.key)?.as_str()?;
            Some((
                group_label(permission.group).to_owned(),
                permission.name.to_owned(),
                level.to_owned(),
            ))
        })
        .collect();
    Ok(Detail {
        row: row(&token, owner),
        description: token.description.clone(),
        access: token.repository_access.clone(),
        repositories,
        permissions,
        reason: token.reason.clone(),
        created: token.created_at.to_rfc3339(),
        never_allowed,
    })
}

async fn show(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<i64>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let user = match account(&ctx) {
        Ok(user) => user,
        Err(response) => return response,
    };
    match detail(&ctx, &user, id).await {
        Ok(token) => respond(
            &ctx,
            StatusCode::OK,
            View::Show { token, value: None },
            None,
        ),
        Err(error) => {
            let (status, message) = refusal(error);
            ctx.error(status, message)
        }
    }
}

async fn regenerate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<i64>,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, false) {
        return response;
    }
    let user = match account(&ctx) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let session = extract_session_id(&ctx.headers);
    let result = match expiry(
        field(&form, "expiry"),
        field(&form, "expires_at"),
        Utc::now(),
    ) {
        Some(expires_at) => {
            fine_grained::regenerate(&ctx.state.pool, user.id, session.as_deref(), id, expires_at)
                .await
        }
        None => Err(TokenError::Invalid(
            "Choose an expiration, and a valid date for a custom one.",
        )),
    };
    let (status, value, message) = match result {
        Ok(value) => (StatusCode::OK, Some(value), None),
        Err(error) => {
            let (status, message) = refusal(error);
            (status, None, Some(message))
        }
    };
    match detail(&ctx, &user, id).await {
        Ok(token) => respond(&ctx, status, View::Show { token, value }, message),
        Err(error) => {
            let (status, message) = refusal(error);
            ctx.error(status, message)
        }
    }
}

async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<i64>,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, false) {
        return response;
    }
    let user = match account(&ctx) {
        Ok(user) => user,
        Err(response) => return response,
    };
    if field(&form, "confirm") != "yes" {
        return ctx.error(
            StatusCode::BAD_REQUEST,
            "Confirm the deletion before the token is revoked.",
        );
    }
    // Only the user's own fine-grained token; classic tokens are deleted from their own list.
    match db::fine_grained_tokens::get(&ctx.state.pool, id).await {
        Ok(Some(token)) if token.user_id == user.id => {}
        Ok(_) => return ctx.error(StatusCode::NOT_FOUND, MISSING),
        Err(_) => return ctx.error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
    }
    let session = extract_session_id(&ctx.headers);
    match tokens::delete(&ctx.state.pool, user.id, session.as_deref(), id).await {
        Ok(()) => ctx.redirect(&format!("{BASE}?deleted=1")),
        Err(tokens::TokenError::Missing) => ctx.error(StatusCode::NOT_FOUND, MISSING),
        Err(tokens::TokenError::Denied) => {
            ctx.error(StatusCode::FORBIDDEN, "Sign in again to delete this token.")
        }
        Err(_) => ctx.error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn fine_grained_expiry_choices_end_at_a_day_in_utc() {
        let now = Utc.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap();
        let day = |y, m, d| Some(Some(Utc.with_ymd_and_hms(y, m, d, 23, 59, 59).unwrap()));
        assert_eq!(expiry("30", "", now), day(2026, 11, 28));
        assert_eq!(expiry("7", "2027-01-01", now), day(2026, 11, 5));
        assert_eq!(expiry("custom", "2027-01-15", now), day(2027, 1, 15));
        assert_eq!(expiry("never", "", now), Some(None));
        for (choice, date) in [
            ("custom", ""),
            ("custom", "2027-02-30"),
            ("365", ""),
            ("", ""),
        ] {
            assert_eq!(expiry(choice, date, now), None, "{choice} {date}");
        }
    }

    #[test]
    fn fine_grained_form_keeps_repeated_fields_and_drops_misfit_permissions() {
        let pairs: Vec<(String, String)> = [
            ("name", "deploy"),
            ("owner", "user"),
            ("access", "public"),
            ("expiry", "30"),
            ("repositories", "4"),
            ("repositories", "9"),
            ("permission.contents", "write"),
            ("permission.gists", "write"),
            ("permission.members", "read"),
            ("permission.issues", "none"),
            ("permission.unknown", "read"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        let draft = draft(&pairs);
        assert_eq!(draft.repositories, vec![4, 9]);
        assert_eq!(draft.permissions.len(), 3);
        let request = request(&draft, Owner::User(1), Utc::now()).unwrap();
        // Public access takes no repository permissions and a user owner no organization ones.
        assert_eq!(
            Value::Object(request.permissions),
            serde_json::json!({"gists":"write"})
        );
        assert!(request.repositories.is_empty());
    }
}
