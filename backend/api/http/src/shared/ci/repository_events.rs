//! The native repository-event form calls the same admission service as REST.
use crate::context::PageContext;
use crate::shared::repos::chrome::Loaded;
use crate::shared::repos::forms::authorize_post;
use crate::shared::repos::forms::field;
use crate::shared::repos::forms::with_status;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_app::app::AppState;
use codeyard_app::ci::events::valid_dispatch;
use codeyard_webui::app::Page;
use codeyard_webui::pages::repos::actions::RepositoryEventForm;
use codeyard_webui::pages::repos::page::Page as RepoPage;
use std::collections::HashMap;
use std::sync::Arc;
async fn open(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: &Uri,
    owner: &str,
    name: &str,
) -> Result<(PageContext, Loaded), Response> {
    let ctx = PageContext::load(state, headers, uri).await?;
    let loaded = crate::shared::repos::chrome::load_database(&ctx, owner, name).await?;
    if ctx.user.is_none() {
        return Err(ctx.error(
            StatusCode::UNAUTHORIZED,
            "Sign in to send a repository event.",
        ));
    }
    if !loaded.chrome.can_write {
        return Err(ctx.error(
            StatusCode::FORBIDDEN,
            "Repository write access is required.",
        ));
    }
    Ok((ctx, loaded))
}

fn show(
    ctx: &PageContext,
    loaded: Loaded,
    event_type: String,
    client_payload: String,
    error: Option<String>,
    status: StatusCode,
) -> Response {
    with_status(
        ctx.page_model(
            "Send repository event",
            Page::Repositories(RepoPage::RepositoryEvent {
                repo: loaded.chrome,
                page: Box::new(RepositoryEventForm {
                    event_type,
                    client_payload,
                    error,
                }),
            }),
        ),
        status,
    )
}

pub(super) async fn form(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    show(
        &ctx,
        loaded,
        String::new(),
        "{}".into(),
        None,
        StatusCode::OK,
    )
}

pub(super) async fn send(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &form) {
        return response;
    }
    if field(&form, "repository_id").parse::<i64>().ok() != Some(loaded.repository.id) {
        return ctx.error(
            StatusCode::CONFLICT,
            "The repository changed. Reload before sending an event.",
        );
    }
    let event_type = field(&form, "event_type");
    let client_payload = field(&form, "client_payload");
    let parsed = if client_payload.trim().is_empty() {
        Ok(serde_json::json!({}))
    } else {
        serde_json::from_str(&client_payload)
    };
    let Ok(payload) = parsed else {
        return show(
            &ctx,
            loaded,
            event_type,
            client_payload,
            Some("Client payload must be valid JSON.".into()),
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    };
    if !valid_dispatch(&event_type, &payload) {
        return show(
            &ctx,
            loaded,
            event_type,
            client_payload,
            Some("The event type needs 1 to 100 characters, and the client payload must be a JSON object with at most ten properties and 64 KiB.".into()),
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    }
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(
            StatusCode::UNAUTHORIZED,
            "Sign in to send a repository event.",
        );
    };
    let Some(session) = crate::session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "A live session is required.");
    };
    let result = ctx
        .state
        .ci_runs
        .dispatch_repository_event(
            &loaded.repository,
            actor,
            actor,
            event_type.clone(),
            payload,
            codeyard_db_core::queries::ci::CiCredential::Session(session),
        )
        .await;
    match result {
        Ok(_) => ctx.redirect(&format!(
            "{}/actions?event=repository_dispatch",
            loaded.chrome.base()
        )),
        Err(error) => {
            let message = error.to_string();
            let status = crate::shared::ci::admission_error(error).0;
            show(
                &ctx,
                loaded,
                event_type,
                client_payload,
                Some(message),
                status,
            )
        }
    }
}
