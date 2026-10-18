use crate::backend::api::admin::accounts::require_admin_user;
use crate::backend::api::auth::errors::database_failure;
use crate::backend::api::http::error::ApiError;
use crate::backend::app::AppState;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct AdminActivityChartQuery {
    pub days: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct AdminActivityPointResponse {
    pub date: String,
    pub daily_contributions: i64,
    pub cumulative_contributions: i64,
    pub daily_storage_bytes: i64,
    pub cumulative_storage_bytes: i64,
}

pub async fn admin_activity_chart(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<AdminActivityChartQuery>,
) -> Result<Json<Vec<AdminActivityPointResponse>>, (StatusCode, Json<ApiError>)> {
    let _admin = require_admin_user(&state, &headers).await?;
    let points = crate::backend::db::users::accounts::list_admin_activity_points(
        &state.pool,
        query.days.unwrap_or(180),
    )
    .await
    .map_err(|_| database_failure())?;
    Ok(Json(
        points
            .into_iter()
            .map(|point| AdminActivityPointResponse {
                date: point.day.to_string(),
                daily_contributions: point.daily_contributions,
                cumulative_contributions: point.cumulative_contributions,
                daily_storage_bytes: point.daily_storage_bytes,
                cumulative_storage_bytes: point.cumulative_storage_bytes,
            })
            .collect(),
    ))
}
