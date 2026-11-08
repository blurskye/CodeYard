//! OIDC settings share the issuer's stored templates and native administration checks.
use axum::response::IntoResponse;

use axum::extract::Path;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_database::queries::oidc::Refusal;
use codeyard_database::queries::oidc::Scope;
use codeyard_database::queries::oidc::Settings;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

type Params = HashMap<String, String>;

fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => Error::new(StatusCode::FORBIDDEN, "OIDC administration required"),
        Some(Refusal::Missing) => Error::missing(),
        Some(Refusal::Invalid | Refusal::Duplicate) => {
            Error::validation("OIDC", "configuration", "invalid")
        }
        None => Error::unavailable(error),
    }
}

async fn scope(
    state: &AppState,
    headers: &HeaderMap,
    params: &Params,
    write: bool,
) -> Result<(codeyard_identity::github_auth::Authentication, Scope)> {
    if let Some(org) = params.get("org") {
        let authentication = codeyard_identity::github_auth::authenticate(state, headers, write)
            .await?
            .ok_or_else(codeyard_identity::github_auth::required)?;
        if write {
            authentication.require_write()?;
        }
        let org =
            codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, org)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        Ok((authentication, Scope::Organization(org.id)))
    } else {
        let (repo, authentication) =
            repository(state, headers, &params["owner"], &params["repo"], write).await?;
        Ok((
            authentication.ok_or_else(codeyard_identity::github_auth::required)?,
            Scope::Repository(repo.id),
        ))
    }
}

#[derive(Deserialize)]
struct Input {
    use_default: Option<bool>,
    include_claim_keys: Option<Vec<String>>,
    #[serde(default)]
    use_immutable_subject: bool,
}

async fn template(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    request: Request,
) -> Result<Response> {
    let read = crate::auth::read_method(request.method());
    let (actor, scope) = scope(&state, &headers, &params, !read).await?;
    if read {
        let settings = codeyard_db_actions::oidc::settings(&state.pool, actor.user.id, scope)
            .await
            .map_err(error)?;
        let document = match scope {
            Scope::Repository(_) => serde_json::to_value(settings).map_err(Error::unavailable)?,
            Scope::Organization(_) => {
                json!({"include_claim_keys":settings.include_claim_keys.unwrap_or_else(|| vec!["repo".into(),"context".into()])})
            }
        };
        return Ok(Json(document).into_response());
    }
    let invalid = |message| match scope {
        Scope::Repository(_) => Error::invalid_simple(message),
        Scope::Organization(_) => Error::invalid(message),
    };
    let bytes = axum::body::to_bytes(request.into_body(), 16384)
        .await
        .map_err(|_| invalid("Invalid OIDC template"))?;
    let input: Input =
        serde_json::from_slice(&bytes).map_err(|_| invalid("Invalid OIDC template"))?;
    if input.use_immutable_subject {
        return Err(invalid("Immutable subjects are not supported"));
    }
    let use_default = match scope {
        Scope::Repository(_) => input
            .use_default
            .ok_or_else(|| invalid("use_default is required"))?,
        Scope::Organization(_) => {
            if input.include_claim_keys.is_none() {
                return Err(Error::invalid("include_claim_keys is required"));
            }
            false
        }
    };
    codeyard_db_actions::oidc::save_settings(
        &state.pool,
        actor.database_actor(),
        scope,
        &Settings {
            use_default,
            include_claim_keys: input.include_claim_keys,
        },
    )
    .await
    .map_err(|e| match scope {
        Scope::Repository(_)
            if matches!(
                e.downcast_ref::<Refusal>(),
                Some(Refusal::Invalid | Refusal::Duplicate)
            ) =>
        {
            Error::invalid_simple("Invalid OIDC configuration")
        }
        _ => error(e),
    })?;
    Ok((StatusCode::CREATED, Json(json!({}))).into_response())
}

#[derive(Deserialize)]
struct Property {
    custom_property_name: String,
}

async fn properties(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    request: Request,
) -> Result<Response> {
    let read = crate::auth::read_method(request.method());
    let (actor, Scope::Organization(org)) = scope(&state, &headers, &params, !read).await? else {
        return Err(Error::missing());
    };
    if read {
        return Ok(Json(
            codeyard_db_actions::oidc::inclusions(&state.pool, actor.user.id, org)
                .await
                .map_err(error)?,
        )
        .into_response());
    }
    let remove = request.method() == Method::DELETE;
    let name = if remove {
        params["custom_property_name"].clone()
    } else {
        let bytes = axum::body::to_bytes(request.into_body(), 4096)
            .await
            .map_err(|_| Error::invalid("Invalid property inclusion"))?;
        serde_json::from_slice::<Property>(&bytes)
            .map_err(|_| Error::invalid("Invalid property inclusion"))?
            .custom_property_name
    };
    codeyard_db_actions::oidc::change_inclusion(
        &state.pool,
        actor.database_actor(),
        org,
        &name,
        remove,
    )
    .await
    .map_err(error)?;
    if remove {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((
            StatusCode::CREATED,
            Json(json!({"custom_property_name":name,"inclusion_source":"organization"})),
        )
            .into_response())
    }
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/get-custom-oidc-sub-claim-for-repo", get(template)),
        ("actions/set-custom-oidc-sub-claim-for-repo", put(template)),
        ("oidc/get-oidc-custom-sub-template-for-org", get(template)),
        (
            "oidc/update-oidc-custom-sub-template-for-org",
            put(template),
        ),
        (
            "oidc/list-oidc-custom-property-inclusions-for-org",
            get(properties),
        ),
        (
            "oidc/create-oidc-custom-property-inclusion-for-org",
            post(properties),
        ),
        (
            "oidc/delete-oidc-custom-property-inclusion-for-org",
            delete(properties),
        ),
    ]
}
