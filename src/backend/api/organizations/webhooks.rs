//! The webhooks section's forms on an organization settings page. Only owners may use them.
use crate::backend::api::http::context::PageContext;
use crate::backend::app::AppState;
use crate::backend::db::webhooks::HookOwner;

use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use std::collections::HashMap;
use std::sync::Arc;

pub(super) async fn post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((org, action)): Path<(i64, String)>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let form: HashMap<String, String> = pairs.iter().cloned().collect();
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let actor = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    if !action.starts_with("webhook-") {
        return crate::backend::api::organizations::organization::not_found(&ctx);
    }
    let base = format!(
        "{}/{org}?tab=webhooks",
        crate::backend::api::organizations::INDEX
    );
    let outcome = crate::backend::webhooks::panel::apply(
        &ctx.state.pool,
        ctx.state.secret_encryptor.as_deref(),
        actor,
        &crate::backend::db::users::accounts::AccountCredential::Session(
            &crate::backend::api::http::session::extract_session_id(&ctx.headers)
                .unwrap_or_default(),
        ),
        HookOwner::Organization(org),
        &action,
        &form,
        &pairs,
    )
    .await;
    match outcome {
        Ok(notice) => ctx.redirect(&format!("{base}&notice={notice}")),
        Err((error, draft)) => {
            // The organization page shows the refusal in its webhooks section.
            let (status, message) = crate::backend::webhooks::panel::refusal(&error);
            crate::backend::api::organizations::organization::respond_with_hook_draft(
                &ctx,
                Some(org),
                status,
                Some(message.to_owned()),
                Default::default(),
                draft,
            )
            .await
        }
    }
}
