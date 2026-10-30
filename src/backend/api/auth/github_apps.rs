//! Native registration and account-owner consent for GitHub Apps.
mod manifests;
use crate::backend::{
    api::{
        http::{context::PageContext, middleware, session},
        user::responses::field,
    },
    app::AppState,
    db::{self, github_apps::App},
    identity::{github_apps as apps, oauth_apps as oauth, tokens::token_hash},
};
use crate::webui::{
    app::Page,
    pages::{account, auth::apps::Model},
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    middleware::from_fn,
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Form, Router,
};
use manifests::{manifest_approve, manifest_preview, manifest_start};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
type Fields = HashMap<String, String>;
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/settings/apps", get(list))
        .route("/settings/apps/installations", get(managed_installations))
        .route(
            "/settings/apps/installations/:id/delete",
            axum::routing::post(uninstall),
        )
        .route("/settings/apps/new", get(new).post(register))
        .route("/settings/apps/:slug/regenerate", axum::routing::post(regenerate))
        .route("/apps/:slug", get(install_get))
        .route(
            "/apps/:slug/installations/new",
            get(install_get).post(install_post),
        )
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .layer(from_fn(middleware::private_sensitive_response))
}
fn model(mode: &str) -> Model {
    Model {
        mode: mode.into(),
        name: String::new(),
        slug: String::new(),
        permissions: String::new(),
        redirect: String::new(),
        applications: vec![],
        credentials: None,
    }
}
fn page(ctx: &PageContext, model: Model) -> Response {
    ctx.page_model(
        "GitHub Apps",
        Page::Identity(account::Page::GitHubApps {
            csrf_token: ctx.csrf_token.clone(),
            model,
        }),
    )
}
fn login(ctx: &PageContext) -> Option<Response> {
    ctx.user.is_none().then(|| {
        Redirect::to(&format!(
            "/enter?redirect={}",
            crate::webui::encode_component(&ctx.location)
        ))
        .into_response()
    })
}
async fn list(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(r) => return r,
    };
    if let Some(r) = login(&ctx) {
        return r;
    }
    let mut model = model("list");
    let session = session::extract_session_id(&ctx.headers).unwrap_or_default();
    match db::github_apps::apps(&ctx.state.pool, ctx.user.as_ref().unwrap().id, &session).await {
        Ok(apps) => model.applications = apps.into_iter().map(|app| (app.name, app.slug)).collect(),
        Err(_) => return ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Apps unavailable"),
    }
    page(&ctx, model)
}
async fn managed_installations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Some(response) = login(&ctx) {
        return response;
    }
    let mut model = model("installations");
    let session = session::extract_session_id(&ctx.headers).unwrap_or_default();
    match db::github_apps::managed_installations(&ctx.state.pool, ctx.user.as_ref().unwrap().id, &session)
        .await
    {
        Ok(rows) => {
            model.applications = rows
                .into_iter()
                .map(|row| {
                    (
                        format!(
                            "{} on {} ({})",
                            row.app_name,
                            row.account_name,
                            if row.all_repos {
                                "all repositories"
                            } else {
                                "selected repositories"
                            }
                        ),
                        row.id.to_string(),
                    )
                })
                .collect()
        }
        Err(_) => return ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Installations unavailable"),
    }
    page(&ctx, model)
}
async fn uninstall(
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
    if let Some(response) = login(&ctx) {
        return response;
    }
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let Some(session) = session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in again");
    };
    match db::github_apps::uninstall(&ctx.state.pool, ctx.user.as_ref().unwrap().id, &session, id)
        .await
    {
        Ok(true) => Redirect::to("/settings/apps/installations").into_response(),
        Ok(false) => ctx.error(
            StatusCode::FORBIDDEN,
            "Only the account owner may uninstall this app",
        ),
        Err(_) => ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Installation removal unavailable",
        ),
    }
}
fn key_failure(ctx: &PageContext, error: apps::RegistrationKeyError) -> Response {
    let status = match error {
        apps::RegistrationKeyError::Limited => StatusCode::TOO_MANY_REQUESTS,
        apps::RegistrationKeyError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    };
    ctx.error(status, "App credential generation is temporarily unavailable")
}

async fn regenerate(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri, Path(slug): Path<String>, Form(form): Form<Fields>) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    ctx.return_to = Some("/settings/apps".into());
    if let Some(response) = login(&ctx) { return response; }
    if let Err(response) = ctx.check_csrf(&form) { return response; }
    let actor = ctx.user.as_ref().unwrap().id;
    let Some(session) = session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in again");
    };
    match db::github_apps::app(&ctx.state.pool, &slug).await {
        Ok(Some(app)) if app.owner_id == actor && app.slug == slug => {},
        Ok(_) => return ctx.error(StatusCode::NOT_FOUND, "App not found"),
        Err(_) => return ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Apps unavailable"),
    }
    let (private, public) = match apps::registration_key(&ctx.state.pool, actor).await {
        Ok(keys) => keys,
        Err(error) => return key_failure(&ctx, error),
    };
    let secret = oauth::random();
    match db::github_apps::rotate_credentials(&ctx.state.pool, actor, &session, &slug, &public, &token_hash(&secret)).await {
        Ok(Some(client)) => {
            let mut model = model("new");
            model.credentials = Some((client, secret, private));
            page(&ctx, model)
        }
        Ok(None) => ctx.error(StatusCode::NOT_FOUND, "App not found"),
        Err(error) if error.is::<db::users::accounts::AccountRestricted>() => ctx.error(StatusCode::FORBIDDEN, "Account unavailable"),
        Err(_) => ctx.error(StatusCode::SERVICE_UNAVAILABLE, "App credential replacement unavailable"),
    }
}

async fn new(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(r) => return r,
    };
    if let Some(r) = login(&ctx) {
        return r;
    }
    if let Some(code) = ctx.query.get("manifest") {
        return manifest_preview(&ctx, code).await;
    }
    page(&ctx, model("new"))
}
async fn register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    if form.contains_key("manifest") {
        return manifest_start(state, headers, uri, &form).await;
    }
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(r) => return r,
    };
    if let Some(r) = login(&ctx) {
        return r;
    }
    if let Err(r) = ctx.check_csrf(&form) {
        return r;
    }
    if field(&form, "decision") == "approve_manifest" {
        return manifest_approve(&ctx, &form).await;
    }
    let slug = field(&form, "slug");
    let name = field(&form, "name");
    let homepage = field(&form, "homepage");
    let url = url::Url::parse(homepage).ok();
    let permissions = serde_json::from_str::<Value>(field(&form, "permissions"))
        .ok()
        .and_then(|value| apps::permissions(&value))
        .map(Value::Object);
    let chosen: Vec<String> = field(&form, "events")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    // An app that only polls the API subscribes to no events, as a manifest may.
    let events = if chosen.is_empty() {
        Ok(String::new())
    } else {
        crate::backend::webhooks::events::subscription(&chosen)
    };
    if !apps::valid_slug(slug)
        || name.trim().is_empty()
        || name.len() > 100
        || field(&form, "description").len() > 1000
        || homepage.len() > 2048
        || !url.is_some_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
        })
        || permissions.is_none()
        || events.is_err()
        || events.as_ref().is_ok_and(|events| {
            events == "*"
                || events
                    .split(',')
                    .filter(|event| !event.is_empty())
                    .any(|event| {
                        !apps::event_permission(permissions.as_ref().unwrap_or(&Value::Null), event)
                    })
        })
    {
        return ctx.error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid app fields or unsupported permissions/events",
        );
    }
    let Some(session) = session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in again");
    };
    let (private, public) = match apps::registration_key(&ctx.state.pool, ctx.user.as_ref().unwrap().id).await {
        Ok(keys) => keys,
        Err(error) => return key_failure(&ctx, error),
    };
    let permissions = permissions.unwrap();
    let client = format!("Iv1.{}", &oauth::random()[..16]);
    let secret = oauth::random();
    let app = App {
        id: 0,
        owner_id: ctx.user.as_ref().unwrap().id,
        bot_id: 0,
        is_public: true,
        client_id: client.clone(),
        slug: slug.into(),
        name: name.trim().into(),
        description: field(&form, "description").into(),
        homepage: homepage.into(),
        public_key: public,
        permissions: permissions.to_string(),
        events: events.unwrap(),
        access_level: if apps::allows(&permissions, "administration", true) {
            5
        } else if apps::write(&permissions) {
            3
        } else {
            1
        },
        created_at: chrono::Utc::now(),
    };
    match db::github_apps::register(
        &ctx.state.pool,
        app.owner_id,
        &session,
        &app,
        &token_hash(&secret),
    )
    .await
    {
        Ok(Some(_)) => {
            let mut model = model("new");
            model.credentials = Some((client, secret, private));
            page(&ctx, model)
        }
        Ok(None) => ctx.error(StatusCode::FORBIDDEN, "Account unavailable"),
        Err(error) => {
            tracing::warn!(%error,"GitHub App registration failed");
            ctx.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Registration failed; name or slug may already exist",
            )
        }
    }
}
async fn install_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(slug): Path<String>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(r) => return r,
    };
    if let Some(r) = login(&ctx) {
        return r;
    }
    match db::github_apps::app(&ctx.state.pool, &slug).await {
        Ok(Some(app))
            if app.is_public
                || ctx
                    .user
                    .as_ref()
                    .is_some_and(|user| user.id == app.owner_id) =>
        {
            let mut model = model("install");
            model.name = app.name;
            model.slug = app.slug;
            model.permissions = app.permissions;
            page(&ctx, model)
        }
        _ => ctx.error(StatusCode::NOT_FOUND, "App not found"),
    }
}
async fn install_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(slug): Path<String>,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(r) => return r,
    };
    if let Some(r) = login(&ctx) {
        return r;
    }
    if let Err(r) = ctx.check_csrf(&form) {
        return r;
    }
    let app = match db::github_apps::app(&ctx.state.pool, &slug).await {
        Ok(Some(app)) => app,
        _ => return ctx.error(StatusCode::NOT_FOUND, "App not found"),
    };
    let Some(session) = session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in again");
    };
    let org = if field(&form, "organization_id").is_empty() {
        None
    } else {
        match field(&form, "organization_id").parse::<i64>() {
            Ok(id) => Some(id),
            Err(_) => {
                return ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid organization ID")
            }
        }
    };
    let repos = field(&form, "repository_ids")
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().parse::<i64>())
        .collect::<Result<Vec<_>, _>>();
    let repos = match repos {
        Ok(repos) if repos.len() <= 500 => repos,
        _ => return ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid repository IDs"),
    };
    let actor = ctx.user.as_ref().unwrap().id;
    if field(&form, "decision") == "request" {
        let Some(org) = org else {
            return ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Choose an organization");
        };
        return match db::github_apps::request_installation(
            &ctx.state.pool,
            actor,
            &session,
            app.id,
            org,
        )
        .await
        {
            Ok(true) => Redirect::to("/settings/apps").into_response(),
            Ok(false) => ctx.error(
                StatusCode::FORBIDDEN,
                "Only organization members may request installation",
            ),
            Err(_) => ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Installation request unavailable",
            ),
        };
    }
    match db::github_apps::install(
        &ctx.state.pool,
        actor,
        &session,
        app.id,
        org.is_none().then_some(actor),
        org,
        if field(&form, "all_repositories") == "yes" {
            None
        } else {
            Some(&repos)
        },
    )
    .await
    {
        Ok(Some(_)) => Redirect::to("/settings/apps").into_response(),
        Ok(None) => ctx.error(
            StatusCode::FORBIDDEN,
            "You must own the selected account and repositories",
        ),
        Err(error) => {
            tracing::warn!(%error,"GitHub App installation failed");
            ctx.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "App may already be installed",
            )
        }
    }
}
