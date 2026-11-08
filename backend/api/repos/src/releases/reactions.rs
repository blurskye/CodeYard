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
use codeyard_identity::repository_access::AccessError;
use codeyard_repos_core::repositories::releases::ReleaseError;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) async fn react(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id)): Path<(String, String, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let user = ctx.require_user()?;
    let loaded =
        codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await?;
    let detail = codeyard_repos_pulls::repositories::releases::detail_by_id(
        &ctx.state.pool,
        Some(user.id),
        loaded.repository.id,
        id,
    )
    .await
    .map_err(|error| match error {
        ReleaseError::Access(AccessError::NotFound) | ReleaseError::NotFound => {
            ctx.error(StatusCode::NOT_FOUND, "Release not found.")
        }
        ReleaseError::Unavailable | ReleaseError::Access(AccessError::Unavailable) => ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The release could not be loaded.",
        ),
        _ => ctx.error(StatusCode::FORBIDDEN, "Release access denied."),
    })?;
    let content = field(&form, "content");
    if !codeyard_db_repos::reactions::valid_content(
        codeyard_db_repos::reactions::Subject::Release,
        &content,
    ) {
        return Err(ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Unknown reaction."));
    }
    codeyard_db_repos::reactions::toggle_reaction(
        &ctx.state.pool,
        user.id,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(
            &codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default(),
        ),
        detail.repository.id,
        id,
        codeyard_db_repos::reactions::Subject::Release,
        id,
        &content,
    )
    .await
    .map_err(|error| {
        if error.is::<codeyard_db_repos::reactions::ReactionRefusal>() {
            ctx.error(StatusCode::NOT_FOUND, "Release not found.")
        } else if error.is::<codeyard_db_core::queries::discussions::DiscussionWriteRefusal>() {
            ctx.error(StatusCode::FORBIDDEN, "Reaction mutation denied.")
        } else {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The reaction could not be saved.",
            )
        }
    })?;
    Ok(ctx.redirect(&format!(
        "/{}/{}/releases/tag/{}",
        codeyard_types::paths::encode_component(&owner),
        codeyard_types::paths::encode_component(&name),
        codeyard_types::paths::encode_component(&detail.release.release.tag_name)
    )))
}
