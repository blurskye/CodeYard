//! Posting a manifest saves only a short-lived draft; approval is a separate CSRF form.
use super::{field, model, page, Fields};
use crate::backend::{
    api::http::{context::PageContext, render::public_error, session},
    app::AppState,
    db,
    identity::{admission, github_apps, tokens::token_hash},
};
use axum::{
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
};
use codeyard_webui::{app::Page, pages::account};
use serde_json::Value;
use std::sync::Arc;
pub(super) async fn manifest_start(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    form: &Fields,
) -> Response {
    match db::node_config::get_node_configuration(&state.pool).await {
        Ok(Some(config)) if config.setup_completed => {}
        Ok(_) => return Redirect::to("/setup").into_response(),
        Err(_) => {
            return public_error(StatusCode::SERVICE_UNAVAILABLE, "Configuration unavailable")
        }
    }
    // Keyed by client, so one client cannot spend everyone's manifest budget.
    let client = session::client_address(&headers);
    match admission::admit_operation(
        &state.pool,
        client,
        "app-manifest",
        client.unwrap_or_default(),
    )
    .await
    {
        Ok(_) => {}
        Err(admission::AdmissionError::Limited) => {
            return public_error(StatusCode::TOO_MANY_REQUESTS, "Too many manifest requests")
        }
        Err(_) => {
            return public_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Manifest approval unavailable",
            )
        }
    }
    let value = serde_json::from_str::<Value>(field(form, "manifest")).ok();
    if value.as_ref().and_then(github_apps::manifest).is_none() {
        return public_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid app manifest or unsupported permissions, events or setup automation",
        );
    }
    let state_value = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    if state_value.len() > 1024 {
        return public_error(StatusCode::UNPROCESSABLE_ENTITY, "State is too long");
    }
    let code = crate::backend::crypto::random_hex();
    if db::github_app_manifests::save(
        &state.pool,
        &token_hash(&code),
        &value.unwrap().to_string(),
        &state_value,
    )
    .await
    .is_err()
    {
        return public_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Manifest approval unavailable",
        );
    }
    Redirect::to(&format!("/settings/apps/new?manifest={code}")).into_response()
}
pub(super) async fn manifest_preview(ctx: &PageContext, code: &str) -> Response {
    let draft =
        match db::github_app_manifests::draft(&ctx.state.pool, &token_hash(code), false).await {
            Ok(Some(draft)) => draft,
            _ => return ctx.error(StatusCode::NOT_FOUND, "Manifest expired or not found"),
        };
    let Some(manifest) = serde_json::from_str::<Value>(&draft.manifest)
        .ok()
        .as_ref()
        .and_then(github_apps::manifest)
    else {
        return ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid manifest");
    };
    let target = manifest.redirect.clone();
    let mut model = model("manifest");
    model.name = manifest.name;
    model.slug = code.into();
    model.permissions = manifest.permissions.to_string();
    model.redirect = format!(
        "Visibility: {}. Manifest redirect: {}. OAuth callbacks: {}. Webhook: {}.",
        if manifest.is_public {
            "public"
        } else {
            "private (your account only)"
        },
        manifest
            .redirect
            .unwrap_or_else(|| "one-time code shown here".into()),
        manifest.callbacks.join(", "),
        manifest.hook_url.unwrap_or_else(|| "not configured".into())
    );
    match target {
        Some(target) => ctx.page_model_with_form_target(
            "GitHub Apps",
            Page::Identity(account::Page::GitHubApps {
                csrf_token: ctx.csrf_token.clone(),
                model,
            }),
            &target,
        ),
        None => page(ctx, model),
    }
}
pub(super) async fn manifest_approve(ctx: &PageContext, form: &Fields) -> Response {
    let hash = token_hash(field(form, "manifest_code"));
    let draft = match db::github_app_manifests::draft(&ctx.state.pool, &hash, false).await {
        Ok(Some(draft)) => draft,
        _ => return ctx.error(StatusCode::NOT_FOUND, "Manifest expired or not found"),
    };
    let Some(manifest) = serde_json::from_str::<Value>(&draft.manifest)
        .ok()
        .as_ref()
        .and_then(github_apps::manifest)
    else {
        return ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid manifest");
    };
    let Some(session) = session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in again");
    };
    let code = crate::backend::crypto::random_hex();
    match db::github_app_manifests::approve(
        &ctx.state.pool,
        &hash,
        ctx.user.as_ref().unwrap().id,
        &session,
        &token_hash(&code),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            return ctx.error(
                StatusCode::FORBIDDEN,
                "Manifest approval is no longer available",
            )
        }
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Manifest approval unavailable",
            )
        }
    }
    if let Some(redirect) = manifest.redirect {
        let mut redirect = url::Url::parse(&redirect).expect("validated manifest redirect");
        redirect
            .query_pairs_mut()
            .append_pair("code", &code)
            .append_pair("state", &draft.state);
        Redirect::to(redirect.as_str()).into_response()
    } else {
        let mut model = model("manifest-ready");
        model.slug = code;
        page(ctx, model)
    }
}
