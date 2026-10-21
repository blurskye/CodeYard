//! The site settings page and the site-wide AI crawler setting, changed from the page or the
//! admin API.
use super::accounts::require_admin_user;
use crate::backend::api::auth::errors::database_failure;
use crate::backend::api::crawlers::{self, BLOCK};
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::user::responses::{field, mutation_guard, require_account, Fields};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::webui::app::Page;
use crate::webui::pages::{account, admin::AdminSettings};
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
pub struct AiCrawlersSetting {
    /// `block` (default) or `allow`.
    pub ai_crawlers: String,
}

pub async fn admin_get_ai_crawlers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<AiCrawlersSetting>, (StatusCode, Json<ApiError>)> {
    require_admin_user(&state, &headers).await?;
    let config = db::node_config::get_node_configuration(&state.pool)
        .await
        .map_err(|_| database_failure())?;
    Ok(Json(AiCrawlersSetting {
        ai_crawlers: config.map_or_else(|| BLOCK.to_owned(), |config| config.ai_crawlers),
    }))
}

pub async fn admin_set_ai_crawlers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AiCrawlersSetting>,
) -> Result<Json<AiCrawlersSetting>, (StatusCode, Json<ApiError>)> {
    let actor = require_admin_user(&state, &headers).await?;
    let value = crawlers::setting_value(&body.ai_crawlers).ok_or((
        StatusCode::BAD_REQUEST,
        Json(ApiError::new(
            "validation_error",
            "ai_crawlers must be allow or block.",
        )),
    ))?;
    let session =
        crate::backend::api::http::session::extract_session_id(&headers).unwrap_or_default();
    let updated = db::node_config::set_ai_crawlers(&state.pool, actor.id, &session, value)
        .await
        .map_err(|_| database_failure())?;
    if !updated {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiError::new(
                "not_set_up",
                "Finish setup before changing this setting.",
            )),
        ));
    }
    Ok(Json(AiCrawlersSetting {
        ai_crawlers: value.to_owned(),
    }))
}

/// The site settings page. A refused save shows the page again with the message.
async fn settings_response(
    ctx: &PageContext,
    status: StatusCode,
    message: Option<&str>,
) -> Response {
    let mut ctx = ctx.clone();
    ctx.return_to = Some("/admin/settings".to_owned());
    let ctx = &ctx;
    if let Err(response) = require_account(ctx, true) {
        return response;
    }
    let config = match db::node_config::get_node_configuration(&ctx.state.pool).await {
        Ok(config) => config,
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load the site settings.",
            )
        }
    };
    let private = config.as_ref().is_some_and(|config| config.is_private());
    let ai_crawlers = config.map_or_else(|| BLOCK.to_owned(), |config| config.ai_crawlers);
    ctx.page_model_status(
        status,
        "Site settings",
        Page::Identity(account::Page::AdminSettings {
            model: AdminSettings {
                private,
                ai_crawlers,
            },
            csrf_token: ctx.csrf_token.clone(),
            message: message.map(str::to_owned),
            is_error: !status.is_success(),
        }),
    )
}

pub(crate) async fn settings_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let message = match ctx.query.get("saved").map(String::as_str) {
        Some("crawlers") => Some("AI crawler setting saved."),
        _ => None,
    };
    settings_response(&ctx, StatusCode::OK, message).await
}

/// The settings form. Without JavaScript it posts here and is redirected back.
pub(crate) async fn settings_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, true) {
        return response;
    }
    let Some(value) = crawlers::setting_value(field(&form, "ai_crawlers")) else {
        return settings_response(
            &ctx,
            StatusCode::BAD_REQUEST,
            Some("Choose allow or block for AI crawlers."),
        )
        .await;
    };
    let session =
        crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    match db::node_config::set_ai_crawlers(
        &ctx.state.pool,
        ctx.user.as_ref().expect("admin guard").id,
        &session,
        value,
    )
    .await
    {
        Ok(true) => ctx.redirect("/admin/settings?saved=crawlers"),
        Ok(false) => {
            settings_response(
                &ctx,
                StatusCode::CONFLICT,
                Some("Finish setup before changing this setting."),
            )
            .await
        }
        Err(_) => {
            settings_response(
                &ctx,
                StatusCode::SERVICE_UNAVAILABLE,
                Some("Unable to save the AI crawler setting."),
            )
            .await
        }
    }
}
