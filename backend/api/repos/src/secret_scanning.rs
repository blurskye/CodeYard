//! Native secret scanning pages use the same transactions as REST.
use axum::response::IntoResponse;

use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Redirect;
use axum::response::Response;
use axum::routing::get;
use axum::Form;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_webui::app::Page;
use codeyard_webui::pages::repos::page::Page as RepoPage;
use codeyard_webui::pages::repos::secret_scanning as view;
use std::collections::HashMap;
use std::sync::Arc;

/// The last page a list or location view serves, so a link never points past it.
const MAX_PAGE: i64 = 10000;

pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/security/secret-scanning", get(list))
        .route(
            "/:owner/:repo/security/secret-scanning/:number",
            get(alert).post(resolve),
        )
        .route(
            "/:owner/:repo/security/secret-scanning/unblock-secret/:id",
            get(bypass_page).post(bypass_form),
        )
}

fn failure(
    ctx: &PageContext,
    loaded: &codeyard_api_http::shared::repos::chrome::Loaded,
    error: anyhow::Error,
) -> Response {
    match error.downcast_ref::<codeyard_db_core::queries::secret_scanning::Refusal>() {
        Some(codeyard_db_core::queries::secret_scanning::Refusal::Missing) => ctx.repo_error(
            &loaded.chrome,
            StatusCode::NOT_FOUND,
            "Secret scanning resource not found",
        ),
        Some(codeyard_db_core::queries::secret_scanning::Refusal::Denied) => ctx.repo_error(
            &loaded.chrome,
            StatusCode::FORBIDDEN,
            "Repository administration required",
        ),
        Some(codeyard_db_core::queries::secret_scanning::Refusal::Invalid) => ctx.repo_error(
            &loaded.chrome,
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid secret scanning submission",
        ),
        _ => codeyard_api_http::shared::repos::chrome::unavailable(ctx),
    }
}

async fn open(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: &Uri,
    params: &HashMap<String, String>,
) -> Result<
    (
        PageContext,
        codeyard_api_http::shared::repos::chrome::Loaded,
    ),
    Response,
> {
    let ctx = PageContext::load(state, headers, uri).await?;
    let loaded = codeyard_api_http::shared::repos::chrome::load_database(
        &ctx,
        &params["owner"],
        &params["repo"],
    )
    .await?;
    ctx.require_user()?;
    Ok((ctx, loaded))
}

fn show(
    ctx: &PageContext,
    loaded: codeyard_api_http::shared::repos::chrome::Loaded,
    page: view::Page,
) -> Response {
    ctx.page_model(
        "Secret scanning",
        Page::Repositories(RepoPage::SecretSecurity {
            repo: loaded.chrome,
            page,
        }),
    )
}

fn summary(alert: codeyard_db_security::secret_scanning::Alert) -> view::Alert {
    view::Alert {
        number: alert.number,
        name: alert.display_name,
        secret_type: alert.secret_type,
        state: alert.state,
        resolution: alert.resolution.unwrap_or_default(),
    }
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<HashMap<String, String>>,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &params).await {
        Ok(open) => open,
        Err(response) => return response,
    };
    let state = ctx
        .query
        .get("state")
        .filter(|state| state.as_str() == "resolved")
        .cloned()
        .unwrap_or_else(|| "open".into());
    let secret_type = ctx.query.get("secret_type").cloned().unwrap_or_default();
    let page = ctx
        .query
        .get("page")
        .and_then(|page| page.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_PAGE);
    let filter = codeyard_db_security::secret_scanning::Filter {
        state: Some(state.clone()),
        secret_type: (!secret_type.is_empty()).then_some(secret_type.clone()),
        offset: (page - 1) * 30,
        limit: 31,
        ..Default::default()
    };
    let mut rows = match codeyard_db_security::secret_scanning::alerts(
        &ctx.state.pool,
        ctx.user.as_ref().expect("signed in").id,
        loaded.repository.id,
        &filter,
    )
    .await
    {
        Ok(rows) => rows,
        Err(error)
            if matches!(
                error.downcast_ref::<codeyard_db_core::queries::secret_scanning::Refusal>(),
                Some(codeyard_db_core::queries::secret_scanning::Refusal::Missing)
            ) =>
        {
            return show(&ctx, loaded, view::Page::Disabled)
        }
        Err(error) => return failure(&ctx, &loaded, error),
    };
    let next = (rows.len() > 30 && page < MAX_PAGE).then(|| {
        format!(
            "{}/security/secret-scanning?state={state}&secret_type={}&page={}",
            loaded.chrome.base(),
            codeyard_webui::encode_component(&secret_type),
            page + 1
        )
    });
    rows.truncate(30);
    show(
        &ctx,
        loaded,
        view::Page::Alerts {
            rows: rows.into_iter().map(summary).collect(),
            state,
            secret_type,
            next,
        },
    )
}

async fn alert(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<HashMap<String, String>>,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &params).await {
        Ok(open) => open,
        Err(response) => return response,
    };
    let number = params["number"].parse().unwrap_or(0);
    let actor = ctx.user.as_ref().expect("signed in").id;
    let alert = match codeyard_db_security::secret_scanning::alerts(
        &ctx.state.pool,
        actor,
        loaded.repository.id,
        &codeyard_db_security::secret_scanning::Filter {
            number: Some(number),
            limit: 1,
            ..Default::default()
        },
    )
    .await
    {
        Ok(mut alerts) if !alerts.is_empty() => alerts.remove(0),
        Ok(_) => {
            return failure(
                &ctx,
                &loaded,
                codeyard_db_core::queries::secret_scanning::Refusal::Missing.into(),
            )
        }
        Err(error) => return failure(&ctx, &loaded, error),
    };
    let page = ctx
        .query
        .get("page")
        .and_then(|page| page.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_PAGE);
    let mut locations = match codeyard_db_security::secret_scanning::locations(
        &ctx.state.pool,
        actor,
        loaded.repository.id,
        number,
        (page - 1) * 30,
        31,
    )
    .await
    {
        Ok(locations) => locations,
        Err(error) => return failure(&ctx, &loaded, error),
    };
    let next = (locations.len() > 30 && page < MAX_PAGE).then(|| {
        format!(
            "{}/security/secret-scanning/{number}?page={}",
            loaded.chrome.base(),
            page + 1
        )
    });
    locations.truncate(30);
    show(
        &ctx,
        loaded,
        view::Page::Alert {
            alert: summary(alert),
            locations: locations
                .into_iter()
                .map(|location| view::Location {
                    path: location.path,
                    commit: location.commit_sha,
                    line: location.start_line,
                })
                .collect(),
            next,
        },
    )
}

async fn resolve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<HashMap<String, String>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &params).await {
        Ok(open) => open,
        Err(response) => return response,
    };
    if let Err(response) = codeyard_api_http::shared::repos::forms::authorize_post(&ctx, &form) {
        return response;
    }
    if form
        .get("repository_id")
        .and_then(|id| id.parse::<i64>().ok())
        != Some(loaded.repository.id)
    {
        return failure(
            &ctx,
            &loaded,
            codeyard_db_core::queries::secret_scanning::Refusal::Missing.into(),
        );
    }
    let number = params["number"].parse().unwrap_or(0);
    let state = codeyard_api_http::shared::repos::forms::field(&form, "state");
    let input = codeyard_db_security::secret_scanning::AlertUpdate {
        state: Some(&state),
        reason: form.get("resolution").map(String::as_str),
        ..Default::default()
    };
    let alert = match codeyard_db_security::secret_scanning::update_alert(
        &ctx.state.pool,
        ctx.user.as_ref().expect("signed in").id,
        loaded.repository.id,
        number,
        &input,
    )
    .await
    {
        Ok(alert) => alert,
        Err(error) => return failure(&ctx, &loaded, error),
    };
    if let Err(error) = codeyard_app::repositories::secret_scanning::jobs::emit_alert(
        &ctx.state,
        &loaded.repository,
        &alert,
        if state == "open" {
            "reopened"
        } else {
            "resolved"
        },
        ctx.user.as_ref().expect("signed in").id,
    )
    .await
    {
        tracing::warn!(repo_id = loaded.repository.id, number, %error, "saved secret scanning update notification failed");
    }
    Redirect::to(&format!(
        "{}/security/secret-scanning/{number}",
        loaded.chrome.base()
    ))
    .into_response()
}

async fn bypass_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<HashMap<String, String>>,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &params).await {
        Ok(open) => open,
        Err(response) => return response,
    };
    let placeholder = match codeyard_db_security::secret_scanning::bypass(
        &ctx.state.pool,
        ctx.user.as_ref().expect("signed in").id,
        loaded.repository.id,
        &params["id"],
        None,
    )
    .await
    {
        Ok(placeholder) => placeholder,
        Err(error) => return failure(&ctx, &loaded, error),
    };
    show(
        &ctx,
        loaded,
        view::Page::Bypass {
            id: placeholder.id,
            token_type: placeholder.token_type,
            allowed: placeholder.reason.is_some(),
        },
    )
}

async fn bypass_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<HashMap<String, String>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &params).await {
        Ok(open) => open,
        Err(response) => return response,
    };
    if let Err(response) = codeyard_api_http::shared::repos::forms::authorize_post(&ctx, &form) {
        return response;
    }
    if form
        .get("repository_id")
        .and_then(|id| id.parse::<i64>().ok())
        != Some(loaded.repository.id)
    {
        return failure(
            &ctx,
            &loaded,
            codeyard_db_core::queries::secret_scanning::Refusal::Missing.into(),
        );
    }
    let reason = codeyard_api_http::shared::repos::forms::field(&form, "reason");
    match codeyard_db_security::secret_scanning::bypass(
        &ctx.state.pool,
        ctx.user.as_ref().expect("signed in").id,
        loaded.repository.id,
        &params["id"],
        Some(&reason),
    )
    .await
    {
        Ok(_) => Redirect::to(&format!(
            "{}/security/secret-scanning/unblock-secret/{}",
            loaded.chrome.base(),
            params["id"]
        ))
        .into_response(),
        Err(error) => failure(&ctx, &loaded, error),
    }
}
