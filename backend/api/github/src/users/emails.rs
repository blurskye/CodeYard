//! Account email privacy never makes an unverified address public.

use axum::extract::OriginalUri;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_db_identity::users::profiles::ProfilePatch;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("users/list-emails-for-authenticated-user", get(list)),
        ("users/add-email-for-authenticated-user", post(add)),
        ("users/delete-email-for-authenticated-user", delete(remove)),
        (
            "users/list-public-emails-for-authenticated-user",
            get(public),
        ),
        (
            "users/set-primary-email-visibility-for-authenticated-user",
            patch(visibility),
        ),
    ]
}
async fn documents(
    state: &AppState,
    user: &codeyard_db_core::models::User,
    public: bool,
) -> Result<Vec<Value>> {
    let settings =
        codeyard_db_identity::users::profiles::get_profile_settings(&state.pool, user.id)
            .await
            .map_err(Error::unavailable)?;
    let mut values = Vec::new();
    if let Some(email) = user
        .email
        .as_deref()
        .filter(|_| !public || (settings.primary_visibility == "public" && user.email_verified))
    {
        values.push(json!({"email":email,"primary":true,"verified":user.email_verified,"visibility":settings.primary_visibility}));
    }
    for address in codeyard_db_identity::users::emails::list_emails(&state.pool, user.id)
        .await
        .map_err(Error::unavailable)?
    {
        let visible = address.verified
            && settings
                .public_email()
                .is_some_and(|email| email.eq_ignore_ascii_case(&address.email));
        if !public || visible {
            values.push(json!({"email":address.email,"primary":false,"verified":address.verified,"visibility":if visible {Some("public")} else {None}}));
        }
    }
    Ok(values)
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    read(&state, &headers, page, uri, false).await
}
async fn public(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    read(&state, &headers, page, uri, true).await
}
async fn read(
    state: &AppState,
    headers: &HeaderMap,
    page: Pagination,
    uri: axum::http::Uri,
    public: bool,
) -> Result<Response> {
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let values = documents(state, &actor.user, public).await?;
    let (offset, limit) = page.window()?;
    let more = values.len() > offset + limit;
    page.response(
        &uri,
        values.into_iter().skip(offset).take(limit).collect(),
        more,
    )
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Addresses {
    Object { emails: Vec<String> },
    List(Vec<String>),
    Single(String),
}
impl Addresses {
    fn emails(self) -> Vec<String> {
        match self {
            Self::Object { emails } | Self::List(emails) => emails,
            Self::Single(email) => vec![email],
        }
    }
}
fn email_failure(error: codeyard_identity::emails::EmailError) -> Error {
    match error {
        codeyard_identity::emails::EmailError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Account is restricted")
        }
        codeyard_identity::emails::EmailError::Unavailable => Error::unavailable(error),
        codeyard_identity::emails::EmailError::Taken => {
            Error::validation("Email", "email", "already_exists")
        }
        _ => Error::validation("Email", "emails", "invalid"),
    }
}
async fn add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    input: std::result::Result<Json<Addresses>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Vec<Value>>)> {
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    actor.require_write()?;
    let Json(input) = input.map_err(|_| Error::validation("Email", "emails", "invalid"))?;
    let added = codeyard_identity::emails::change(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        &input.emails(),
        true,
    )
    .await
    .map_err(email_failure)?;
    let user = codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, actor.user.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let values = documents(&state, &user, false)
        .await?
        .into_iter()
        .filter(|value| {
            value["email"]
                .as_str()
                .is_some_and(|email| added.iter().any(|added| added.eq_ignore_ascii_case(email)))
        })
        .collect();
    Ok((StatusCode::CREATED, Json(values)))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    input: std::result::Result<Json<Addresses>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode> {
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    actor.require_write()?;
    let Json(input) = input.map_err(|_| Error::validation("Email", "emails", "invalid"))?;
    codeyard_identity::emails::change(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        &input.emails(),
        false,
    )
    .await
    .map_err(email_failure)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Visibility {
    visibility: String,
}
async fn visibility(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    input: std::result::Result<Json<Visibility>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Vec<Value>>> {
    let actor = codeyard_repos_core::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    actor.require_write()?;
    let Json(input) = input.map_err(|_| Error::validation("Email", "visibility", "invalid"))?;
    if !matches!(input.visibility.as_str(), "public" | "private") {
        return Err(Error::validation("Email", "visibility", "invalid"));
    }
    let user = codeyard_identity::profiles::save(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        &ProfilePatch {
            primary_visibility: Some(&input.visibility),
            ..ProfilePatch::default()
        },
    )
    .await
    .map_err(|error| match error {
        codeyard_identity::profiles::ProfileError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Account is restricted")
        }
        _ => Error::unavailable(error),
    })?;
    Ok(Json(documents(&state, &user, false).await?))
}
