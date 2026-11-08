use crate::user::responses::mutation_guard;
use crate::user::responses::require_account;
use axum::extract::Form;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::user::responses::field;
use codeyard_api_http::shared::user::responses::Fields;
use codeyard_app::app::AppState;
use codeyard_database::queries::users::accounts::AccountCredential;
use codeyard_db_identity::interactions::InteractionRefusal;
use codeyard_db_identity::interactions::InteractionScope;
use codeyard_db_identity::interactions::NewRestriction;
use codeyard_db_repos::pull_caps::CapUpdate;
use codeyard_db_repos::pull_caps::PullCapRefusal;
use codeyard_db_repos::pull_caps::PullCapScope;
use codeyard_types::credentials::AccountRestricted;
use std::sync::Arc;

pub(crate) async fn post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, false) {
        return response;
    }
    let actor = match require_account(&ctx, false) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Some(scope) = form_scope(&form) else {
        return ctx.error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid interaction scope.",
        );
    };
    let input = if field(&form, "remove") == "yes" {
        None
    } else {
        let value = serde_json::json!({"limit":field(&form,"limit"),"expiry":if field(&form,"expiry").is_empty() {"one_day"} else {field(&form,"expiry")}});
        match serde_json::from_value::<NewRestriction>(value) {
            Ok(input) => Some(input),
            Err(_) => {
                return ctx.error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Choose a valid interaction limit and duration.",
                )
            }
        }
    };
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    if let Err(error) = codeyard_db_identity::interactions::set_restriction(
        &ctx.state.pool,
        actor.id,
        &AccountCredential::Session(&session),
        scope,
        input,
    )
    .await
    {
        let status = match error.downcast_ref::<InteractionRefusal>() {
            Some(InteractionRefusal::Conflict) => StatusCode::CONFLICT,
            Some(InteractionRefusal::Denied) => StatusCode::FORBIDDEN,
            _ if error.is::<AccountRestricted>() => StatusCode::FORBIDDEN,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        return ctx.error(
            status,
            if status == StatusCode::SERVICE_UNAVAILABLE {
                "Interaction settings are temporarily unavailable."
            } else if status == StatusCode::CONFLICT {
                "An owner-level interaction limit overrides this repository."
            } else {
                "Administration permission is required."
            },
        );
    }
    ctx.redirect(&scope_target(&ctx, scope).await)
}
fn form_scope(form: &Fields) -> Option<InteractionScope> {
    let id = field(form, "scope_id")
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)?;
    match field(form, "scope") {
        "user" => Some(InteractionScope::User(id)),
        "organization" => Some(InteractionScope::Organization(id)),
        "repository" => Some(InteractionScope::Repository(id)),
        _ => None,
    }
}
async fn scope_target(ctx: &PageContext, scope: InteractionScope) -> String {
    match scope {
        InteractionScope::User(_) => "/settings?tab=blocked".to_owned(),
        InteractionScope::Organization(id) => format!("/settings/organizations/{id}"),
        InteractionScope::Repository(id) => {
            match (
                codeyard_db_repos::repositories::records::get_repository_by_id(&ctx.state.pool, id)
                    .await,
                codeyard_db_repos::repositories::records::repository_owner_name(
                    &ctx.state.pool,
                    id,
                )
                .await,
            ) {
                (Ok(Some(repo)), Ok(Some(owner))) => format!(
                    "/{}/{}/settings",
                    codeyard_types::paths::encode_component(&owner),
                    codeyard_types::paths::encode_component(&repo.name)
                ),
                _ => "/".into(),
            }
        }
    }
}
fn cap_failure(ctx: &PageContext, error: anyhow::Error) -> Response {
    match error.downcast_ref::<PullCapRefusal>() {
        Some(PullCapRefusal::Invalid | PullCapRefusal::Limit | PullCapRefusal::MissingUser) => {
            ctx.error(StatusCode::UNPROCESSABLE_ENTITY, &error.to_string())
        }
        Some(PullCapRefusal::Denied) => ctx.error(
            StatusCode::FORBIDDEN,
            "Administration permission is required.",
        ),
        None if error.is::<AccountRestricted>() => ctx.error(
            StatusCode::FORBIDDEN,
            "This account cannot change interaction settings.",
        ),
        None => ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Interaction settings are temporarily unavailable.",
        ),
    }
}
pub(crate) async fn post_cap(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, false) {
        return response;
    }
    let actor = match require_account(&ctx, false) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let scope = match form_scope(&form) {
        Some(InteractionScope::Repository(id)) => PullCapScope::Repository(id),
        Some(InteractionScope::Organization(id)) => PullCapScope::Organization(id),
        _ => {
            return ctx.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Choose a repository or organization.",
            )
        }
    };
    let value = serde_json::json!({"enabled":field(&form,"enabled").parse::<bool>().ok(),"max_open_pull_requests":if field(&form,"max_open_pull_requests").is_empty() {None} else {match field(&form,"max_open_pull_requests").parse::<i32>() {Ok(value)=>Some(value),Err(_)=>return ctx.error(StatusCode::UNPROCESSABLE_ENTITY,"Choose a maximum between 1 and 1000.")}},"include_drafts":if field(&form,"include_drafts").is_empty() {None} else {match field(&form,"include_drafts").parse::<bool>() {Ok(value)=>Some(value),Err(_)=>return ctx.error(StatusCode::UNPROCESSABLE_ENTITY,"Choose whether drafts are counted.")}}});
    let input = match serde_json::from_value::<CapUpdate>(value) {
        Ok(input) => input,
        Err(_) => {
            return ctx.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Choose whether the cap is enabled.",
            )
        }
    };
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    if let Err(error) = codeyard_db_repos::pull_caps::update_cap(
        &ctx.state.pool,
        actor.id,
        &AccountCredential::Session(&session),
        scope,
        &input,
    )
    .await
    {
        return cap_failure(&ctx, error);
    }
    let scope = match scope {
        PullCapScope::Organization(id) => InteractionScope::Organization(id),
        PullCapScope::Repository(id) => InteractionScope::Repository(id),
    };
    ctx.redirect(&scope_target(&ctx, scope).await)
}
pub(crate) async fn post_bypass(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, false) {
        return response;
    }
    let actor = match require_account(&ctx, false) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let repo = match field(&form, "repo_id").parse::<i64>() {
        Ok(id) if id > 0 => id,
        _ => return ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Choose a repository."),
    };
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    if let Err(error) = codeyard_db_repos::pull_caps::change_bypass(
        &ctx.state.pool,
        actor.id,
        &AccountCredential::Session(&session),
        repo,
        &[field(&form, "username").to_owned()],
        field(&form, "remove") != "yes",
    )
    .await
    {
        return cap_failure(&ctx, error);
    }
    ctx.redirect(&scope_target(&ctx, InteractionScope::Repository(repo)).await)
}
