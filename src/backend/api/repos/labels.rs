use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::{
    check_repo_admin_access, find_repository, readable_repository,
};
use crate::backend::api::repos::discussion::{label, page, repository, unavailable, ResultPage};
use crate::backend::api::repos::responses::storage_unavailable;
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
        Some(db::labels::LabelWriteRefusal::DuplicateName) => (
            StatusCode::CONFLICT,
            "duplicate_label",
            "A label with this name already exists. Choose another name.",
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "Label storage unavailable",
        ),
    };
    (status, Json(ApiError::new(code, message)))
}

/// A `#rrggbb` color, given with or without its `#`.
pub(crate) fn parse_label_color(color: &str) -> Option<String> {
    let hex = color.trim_start_matches('#');
    (hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| format!("#{hex}"))
}

pub async fn list_labels(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<LabelResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;

    let labels = db::labels::list_repo_labels(&state.pool, repository.id)
        .await
        .map_err(|_| storage_unavailable())?;

    Ok(Json(labels.into_iter().map(LabelResponse::from).collect()))
}

pub async fn create_label(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<CreateLabelRequest>,
) -> Result<(StatusCode, Json<LabelResponse>), (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &actor).await?;

    if let Err(message) =
        crate::backend::issues::validate_label(&req.name, req.description.as_deref())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("validation_error", message)),
        ));
    }

    let Some(color) = parse_label_color(&req.color) else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Color must be a valid 6-character hex code",
            )),
        ));
    };

    let label = db::labels::create_label(
        &state.pool,
        actor.id,
        repository.id,
        &req.name,
        &color,
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
    let actor = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &actor).await?;

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
    let (offset, limit) = super::paging::page_window(page_number, 100)
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
        super::paging::has_next_page(page_number, items.len(), limit, super::paging::MAX_PAGE);
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
