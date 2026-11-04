use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_support::credentials::AccountRestricted;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("users/list-social-accounts-for-user", get(public)),
        (
            "users/list-social-accounts-for-authenticated-user",
            get(own),
        ),
        ("users/add-social-account-for-authenticated-user", post(add)),
        (
            "users/delete-social-account-for-authenticated-user",
            delete(remove),
        ),
    ]
}

async fn public(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    codeyard_app::api_shared::github::auth::authenticate(&state, &headers, false).await?;
    let user = crate::users::target(&state, &username).await?;
    list(&state, user.id, page, uri).await
}
async fn own(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = codeyard_app::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    list(&state, actor.user.id, page, uri).await
}
async fn list(
    state: &AppState,
    user: i64,
    page: Pagination,
    uri: axum::http::Uri,
) -> Result<Response> {
    let (offset, limit) = page.window()?;
    let accounts =
        codeyard_db_identity::users::social_accounts::list_social_accounts(&state.pool, user)
            .await
            .map_err(Error::unavailable)?;
    let more = accounts.len() > offset + limit;
    page.response(
        &uri,
        accounts
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|account| json!(account))
            .collect(),
        more,
    )
}
#[derive(Deserialize)]
struct Input {
    account_urls: Vec<String>,
}

fn failure(error: anyhow::Error) -> Error {
    if error.is::<codeyard_db_identity::users::social_accounts::TooManyAccounts>() {
        Error::validation("SocialAccount", "account_urls", "invalid")
    } else if error.is::<AccountRestricted>() {
        Error::new(StatusCode::FORBIDDEN, "Account is restricted")
    } else {
        Error::unavailable(error)
    }
}
async fn change(
    state: &AppState,
    headers: &HeaderMap,
    input: std::result::Result<Json<Input>, axum::extract::rejection::JsonRejection>,
    add: bool,
) -> Result<Vec<codeyard_db_identity::users::social_accounts::SocialAccount>> {
    let actor = codeyard_app::api_shared::github::auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    actor.require_write()?;
    let Json(input) = input.map_err(codeyard_model::github_error::json_error)?;
    if input.account_urls.is_empty() || input.account_urls.len() > 4 {
        return Err(Error::validation(
            "SocialAccount",
            "account_urls",
            "invalid",
        ));
    }
    let accounts = input
        .account_urls
        .iter()
        .map(|url| codeyard_identity::profiles::social_account(url))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Error::validation("SocialAccount", "account_urls", "invalid"))?;
    codeyard_db_identity::users::social_accounts::change_social_accounts(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        &accounts,
        add,
    )
    .await
    .map_err(failure)
}
async fn add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    input: std::result::Result<Json<Input>, axum::extract::rejection::JsonRejection>,
) -> Result<(
    StatusCode,
    Json<Vec<codeyard_db_identity::users::social_accounts::SocialAccount>>,
)> {
    Ok((
        StatusCode::CREATED,
        Json(change(&state, &headers, input, true).await?),
    ))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    input: std::result::Result<Json<Input>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode> {
    change(&state, &headers, input, false).await?;
    Ok(StatusCode::NO_CONTENT)
}
