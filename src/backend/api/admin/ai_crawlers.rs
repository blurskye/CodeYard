//! The site-wide AI crawler setting, changed from the admin page or the admin API.
use super::accounts::{admin_response, require_admin_user};
use crate::backend::api::auth::errors::database_failure;
use crate::backend::api::crawlers::{self, BLOCK};
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::user::responses::{field, mutation_guard, Fields};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::webui::pages::admin::normalize_days;
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

/// The admin page form. Without JavaScript it posts here and is redirected back.
pub(crate) async fn ai_crawlers_post(
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
        return admin_response(
            &ctx,
            normalize_days(None),
            Some(&form),
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
        Ok(true) => ctx.redirect("/admin?saved=crawlers"),
        Ok(false) => {
            admin_response(
                &ctx,
                normalize_days(None),
                Some(&form),
                StatusCode::CONFLICT,
                Some("Finish setup before changing this setting."),
            )
            .await
        }
        Err(_) => {
            admin_response(
                &ctx,
                normalize_days(None),
                Some(&form),
                StatusCode::SERVICE_UNAVAILABLE,
                Some("Unable to save the AI crawler setting."),
            )
            .await
        }
    }
}
