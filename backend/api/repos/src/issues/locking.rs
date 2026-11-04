use crate::discussion::kind;
use crate::discussion::repository;
use crate::discussion::unavailable;
use crate::discussion::ResultPage;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::repos::forms::authorize_post;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_app::app::AppState;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) async fn native_lock(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let kind = kind(&uri);
    let item = crate::issues::assignees::assigned_item(&ctx, &repo, kind, number).await?;
    let locked = match field(&form, "locked").as_str() {
        "true" => true,
        "false" => false,
        _ => return Err(ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid lock state.")),
    };
    let reason = field(&form, "lock_reason");
    codeyard_db_repos::discussions::set_lock(
        &ctx.state.pool,
        ctx.database_actor(ctx.require_user()?.id),
        repo.record.id,
        item,
        locked,
        (!reason.is_empty()).then_some(reason.as_str()),
    )
    .await
    .map_err(|error| {
        match error.downcast_ref::<codeyard_db_repos::discussions::LockRefusal>() {
            Some(codeyard_db_repos::discussions::LockRefusal::Denied) => ctx.error(
                StatusCode::FORBIDDEN,
                "Triage access is required to lock this conversation.",
            ),
            Some(codeyard_db_repos::discussions::LockRefusal::Missing) => {
                ctx.error(StatusCode::NOT_FOUND, "Discussion not found.")
            }
            Some(codeyard_db_repos::discussions::LockRefusal::InvalidReason) => {
                ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid lock reason.")
            }
            None => unavailable(&ctx),
        }
    })?;
    Ok(ctx.redirect(&format!("{}/{number}", repo.view.family_url(kind))))
}
