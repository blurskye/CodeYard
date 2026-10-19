use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::check_repo_admin_access;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::pulls::pull::{label, page, repository, unavailable, ResultPage};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::webui::pages::repos::issues as view;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
#[derive(Serialize, Clone)]
pub struct LabelResponse {
    pub id: i64,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
}

impl From<db::models::Label> for LabelResponse {
    fn from(label: db::models::Label) -> Self {
        Self {
            id: label.id,
            name: label.name,
            color: label.color,
            description: label.description,
        }
    }
}

#[derive(Deserialize)]
pub struct CreateLabelRequest {
    pub name: String,
    pub color: String,
    pub description: Option<String>,
}

pub(crate) fn label_write_error(error: anyhow::Error) -> (StatusCode, Json<ApiError>) {
    let (status, code, message) = match error.downcast_ref::<db::labels::LabelWriteRefusal>() {
        Some(db::labels::LabelWriteRefusal::Denied) => (
            StatusCode::FORBIDDEN,
            "label_write_denied",
            "Label mutation denied",
        ),
        Some(db::labels::LabelWriteRefusal::InvalidLabels) => (
            StatusCode::CONFLICT,
            "invalid_labels",
            "Invalid label scope or batch",
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "Label storage unavailable",
        ),
    };
    (status, Json(ApiError::new(code, message)))
}

pub async fn list_labels(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<LabelResponse>>, (StatusCode, Json<ApiError>)> {
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

    let labels = db::labels::list_repo_labels(&state.pool, repository.id)
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

    Ok(Json(labels.into_iter().map(LabelResponse::from).collect()))
}

pub async fn create_label(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<CreateLabelRequest>,
) -> Result<(StatusCode, Json<LabelResponse>), (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(label_write_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_admin_access(&state.pool, &repository, &headers).await?;
    let actor = get_current_user(&state.pool, &headers).await?;

    if let Err(message) =
        crate::backend::issues::validate_label(&req.name, req.description.as_deref())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("validation_error", message)),
        ));
    }

    let color = req.color.trim_start_matches('#');
    if color.len() != 6 || !color.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Color must be a valid 6-character hex code",
            )),
        ));
    }

    let label = db::labels::create_label(
        &state.pool,
        actor.id,
        repository.id,
        &req.name,
        &format!("#{}", color),
        req.description.as_deref(),
    )
    .await
    .map_err(label_write_error)?;

    Ok((StatusCode::CREATED, Json(LabelResponse::from(label))))
}

pub async fn delete_label(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, label_id)): Path<(String, String, i64)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(label_write_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_admin_access(&state.pool, &repository, &headers).await?;
    let actor = get_current_user(&state.pool, &headers).await?;

    db::labels::delete_label(&state.pool, actor.id, repository.id, label_id)
        .await
        .map_err(label_write_error)?;

    Ok(StatusCode::NO_CONTENT)
}

/// The search text and sort order of the labels page. Both are optional.
#[derive(Deserialize)]
pub(crate) struct LabelsQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    sort: String,
    page: Option<usize>,
}

/// The labels page. Anyone who can read the repository sees its labels; the forms that change them
/// are for administrators and are checked again when they are posted.
pub(crate) async fn native_index(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<LabelsQuery>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    let repo = repository(&ctx, &owner, &name).await?;
    let page_number = query.page.unwrap_or(1);
    let (offset, limit) = super::chrome::page_window(page_number, 100)
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid label page."))?;
    let mut items: Vec<_> = db::labels::list_repo_labels_page(
        &ctx.state.pool,
        repo.record.id,
        &query.q,
        &query.sort,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(|_| unavailable(&ctx))?
    .into_iter()
    .map(label)
    .collect();
    let has_next =
        super::chrome::has_next_page(page_number, items.len(), limit, super::chrome::MAX_PAGE);
    items.truncate(limit);
    let usage: std::collections::BTreeMap<i64, i64> =
        db::labels::list_label_usage(&ctx.state.pool, repo.record.id)
            .await
            .map_err(|_| unavailable(&ctx))?
            .into_iter()
            .filter(|(id, _)| items.iter().any(|item| item.id == *id))
            .collect();
    Ok(ctx.page_model(
        "Labels",
        page(view::Page::Labels {
            repository: repo.view,
            items,
            usage,
            query: query.q,
            sort: query.sort,
            page_number,
            has_next,
        }),
    ))
}

pub fn page_router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    axum::Router::new().route("/:owner/:repo/labels", axum::routing::get(native_index))
}

pub fn router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use axum::{
        routing::{delete, get},
        Router,
    };
    Router::new()
        .route(
            "/api/repos/:owner/:repo/labels",
            get(list_labels).post(create_label),
        )
        .route(
            "/api/repos/:owner/:repo/labels/:label_id",
            delete(delete_label),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_errors_are_typed_and_sanitized() {
        for (refusal, status) in [
            (db::labels::LabelWriteRefusal::Denied, StatusCode::FORBIDDEN),
            (
                db::labels::LabelWriteRefusal::InvalidLabels,
                StatusCode::CONFLICT,
            ),
        ] {
            let (actual, Json(body)) =
                label_write_error(anyhow::Error::new(refusal).context("private driver context"));
            assert_eq!(actual, status);
            assert!(!serde_json::to_string(&body).unwrap().contains("private"));
        }
        let (status, Json(body)) =
            label_write_error(anyhow::anyhow!("private label / driver details"));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!serde_json::to_string(&body).unwrap().contains("private"));
    }
}
