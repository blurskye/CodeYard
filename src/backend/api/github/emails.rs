//! Account email privacy never makes an unverified address public.
use super::{auth, pagination::Pagination, Error, Result};
use crate::backend::{
    app::AppState,
    db::users::{
        accounts::AccountCredential,
        profiles::{self, ProfilePatch},
    },
    identity,
};
use axum::{
    extract::{OriginalUri, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{delete, get, patch, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
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
    user: &crate::backend::db::models::User,
    public: bool,
) -> Result<Vec<Value>> {
    let settings = profiles::get_profile_settings(&state.pool, user.id)
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
    for address in crate::backend::db::users::emails::list_emails(&state.pool, user.id)
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
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
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
fn email_failure(error: identity::emails::EmailError) -> Error {
    match error {
        identity::emails::EmailError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Account is restricted")
        }
        identity::emails::EmailError::Unavailable => Error::unavailable(error),
        identity::emails::EmailError::Taken => {
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
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let Json(input) = input.map_err(|_| Error::validation("Email", "emails", "invalid"))?;
    let added = identity::emails::change(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id),
        &input.emails(),
        true,
    )
    .await
    .map_err(email_failure)?;
    let user = crate::backend::db::users::accounts::get_user_by_id(&state.pool, actor.user.id)
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
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let Json(input) = input.map_err(|_| Error::validation("Email", "emails", "invalid"))?;
    identity::emails::change(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id),
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
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let Json(input) = input.map_err(|_| Error::validation("Email", "visibility", "invalid"))?;
    if !matches!(input.visibility.as_str(), "public" | "private") {
        return Err(Error::validation("Email", "visibility", "invalid"));
    }
    let user = identity::profiles::save(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id),
        &ProfilePatch {
            primary_visibility: Some(&input.visibility),
            ..ProfilePatch::default()
        },
    )
    .await
    .map_err(|error| match error {
        identity::profiles::ProfileError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Account is restricted")
        }
        _ => Error::unavailable(error),
    })?;
    Ok(Json(documents(&state, &user, false).await?))
}
