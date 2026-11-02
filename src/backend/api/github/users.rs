//! Accounts and follow edges use the same identity operations as the profile pages.
use super::{api_url, auth, pagination::Pagination, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        models::User,
        users::{accounts, ssh_keys},
    },
    identity::{following, keys},
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("users/get-authenticated", get(authenticated)),
        ("users/update-authenticated", patch(update)),
        ("users/get-by-username", get(by_username)),
        ("users/get-by-id", get(by_id)),
        ("users/list", get(list)),
        ("users/list-followers-for-user", get(followers)),
        ("users/list-following-for-user", get(following)),
        (
            "users/list-followers-for-authenticated-user",
            get(own_followers),
        ),
        (
            "users/list-followed-by-authenticated-user",
            get(own_following),
        ),
        ("users/check-following-for-user", get(check_following)),
        (
            "users/check-person-is-followed-by-authenticated",
            get(check_own_following),
        ),
        ("users/follow", put(follow)),
        ("users/unfollow", delete(unfollow)),
        ("users/get-context-for-user", get(hovercard)),
        ("users/list-public-keys-for-user", get(public_keys)),
        (
            "users/list-public-ssh-keys-for-authenticated-user",
            get(own_keys),
        ),
        ("users/get-public-ssh-key-for-authenticated-user", get(key)),
        (
            "users/create-public-ssh-key-for-authenticated-user",
            post(create_key),
        ),
        (
            "users/delete-public-ssh-key-for-authenticated-user",
            delete(delete_key),
        ),
    ]
}

pub(super) async fn target(state: &AppState, username: &str) -> Result<User> {
    accounts::get_user_by_username(&state.pool, username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)
}

pub(super) async fn document(state: &AppState, user: &User, own: bool) -> Result<Value> {
    let repos = db::repositories::count_user_repositories(&state.pool, user.id, false)
        .await
        .map_err(Error::unavailable)?;
    let status = following::status(&state.pool, user.id, None)
        .await
        .map_err(Error::unavailable)?;
    let profile = db::users::profiles::get_profile_settings(&state.pool, user.id)
        .await
        .map_err(Error::unavailable)?;
    // The private statistics scan every readable private repository, so only the owner's own
    // document pays for them.
    let (public_gists, statistics) = if own {
        let statistics = db::users::profiles::account_statistics(&state.pool, user.id)
            .await
            .map_err(Error::unavailable)?;
        (statistics.public_gists, Some(statistics))
    } else {
        let public = db::gists::count_owned(&state.pool, user.id, false, None)
            .await
            .map_err(Error::unavailable)?;
        (public, None)
    };
    Ok(payloads::public_user(
        user,
        &profile,
        repos,
        status.followers_count,
        status.following_count,
        public_gists,
        statistics.as_ref(),
    ))
}

async fn authenticated(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    // As on GitHub, an OAuth token needs the user scope for the private fields. An App
    // credential acts with its installation's permissions, which do not include them.
    let own = match db::oauth_apps::token(&state.pool, actor.token.id)
        .await
        .map_err(Error::unavailable)?
    {
        Some(token) => {
            crate::backend::identity::oauth_apps::has_scope(&token.scopes, "user")
                || crate::backend::identity::oauth_apps::has_scope(&token.scopes, "read:user")
        }
        None => crate::backend::identity::github_apps::authority(&state.pool, actor.token.id)
            .await
            .map_err(Error::unavailable)?
            .is_none(),
    };
    Ok(Json(document(&state, &actor.user, own).await?))
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    input: std::result::Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>> {
    use db::users::{accounts::AccountCredential, profiles::ProfilePatch};
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let Json(input) = input.map_err(|_| Error::validation("User", "body", "invalid"))?;
    let object = input
        .as_object()
        .ok_or_else(|| Error::validation("User", "body", "invalid"))?;
    let text = |field: &str| -> Result<Option<&str>> {
        match object.get(field) {
            None => Ok(None),
            Some(Value::Null) if field == "twitter_username" || field == "email" => Ok(Some("")),
            Some(Value::String(value)) => Ok(Some(value)),
            _ => Err(Error::validation("User", field, "invalid")),
        }
    };
    let hireable = object
        .get("hireable")
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| Error::validation("User", "hireable", "invalid"))
        })
        .transpose()?;
    let user = crate::backend::identity::profiles::save(
        &state.pool,
        actor.user.id,
        &AccountCredential::Token(actor.token.id),
        &ProfilePatch {
            display_name: text("name")?,
            bio: text("bio")?,
            location: text("location")?,
            website: text("blog")?,
            company: text("company")?,
            twitter_username: text("twitter_username")?,
            email: text("email")?,
            hireable,
            ..ProfilePatch::default()
        },
    )
    .await
    .map_err(|error| match error {
        crate::backend::identity::profiles::ProfileError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Account is restricted")
        }
        crate::backend::identity::profiles::ProfileError::Unavailable => Error::unavailable(error),
        crate::backend::identity::profiles::ProfileError::Email => {
            Error::validation("User", "email", "invalid")
        }
        _ => Error::validation("User", "profile", "invalid"),
    })?;
    Ok(Json(document(&state, &user, true).await?))
}

async fn by_username(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> Result<Json<Value>> {
    auth::authenticate(&state, &headers, false).await?;
    if let Some(user) = accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(Error::unavailable)?
    {
        return Ok(Json(document(&state, &user, false).await?));
    }
    // GitHub's user lookup also resolves organization accounts for clients such as gh.
    let org = super::orgs::target(&state, &username).await?;
    let mut value = super::orgs::document(&state, &org).await?;
    for field in [
        "archived_at",
        "description",
        "has_organization_projects",
        "has_repository_projects",
        "hooks_url",
        "is_verified",
        "issues_url",
        "members_url",
        "public_members_url",
    ] {
        value.as_object_mut().unwrap().remove(field);
    }
    let url = api_url(&format!(
        "/users/{}",
        crate::webui::encode_component(&org.name)
    ));
    value["url"] = json!(url);
    for (field, suffix) in [
        ("followers_url", "/followers"),
        ("following_url", "/following{/other_user}"),
        ("gists_url", "/gists{/gist_id}"),
        ("starred_url", "/starred{/owner}{/repo}"),
        ("subscriptions_url", "/subscriptions"),
        ("organizations_url", "/orgs"),
        ("received_events_url", "/received_events"),
    ] {
        value[field] = json!(format!("{url}{suffix}"));
    }
    for field in ["gravatar_id", "company", "location", "email", "hireable"] {
        value[field] = Value::Null;
    }
    value["bio"] = json!(org.description);
    value["blog"] = json!("");
    value["site_admin"] = json!(false);
    Ok(Json(value))
}

async fn by_id(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    auth::authenticate(&state, &headers, false).await?;
    let user = accounts::get_user_by_id(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(Json(document(&state, &user, false).await?))
}

#[derive(Deserialize)]
struct UserList {
    since: Option<i64>,
    per_page: Option<usize>,
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<UserList>,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    let limit = query.per_page.unwrap_or(30).clamp(1, 100);
    let mut users = accounts::list_users_since(
        &state.pool,
        query.since.unwrap_or(0).max(0),
        (limit + 1) as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = users.len() > limit;
    users.truncate(limit);
    let mut headers = HeaderMap::new();
    if has_next {
        let since = users.last().unwrap().id;
        headers.insert(
            header::LINK,
            format!(
                "<{}?since={since}&per_page={limit}>; rel=\"next\"",
                api_url("/users")
            )
            .parse()
            .map_err(Error::unavailable)?,
        );
    }
    Ok((
        headers,
        Json(
            users
                .iter()
                .map(|user| payloads::person(&payloads::Person::of(user)))
                .collect::<Vec<_>>(),
        ),
    )
        .into_response())
}

async fn connections(
    state: &AppState,
    user: i64,
    followers: bool,
    pagination: Pagination,
    uri: axum::http::Uri,
) -> Result<Response> {
    let (offset, limit) = pagination.window()?;
    let users = db::following::list_connections(
        &state.pool,
        user,
        followers,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = users.len() > limit;
    pagination.response(
        &uri,
        users
            .iter()
            .map(|user| payloads::person(&payloads::Person::of(user)))
            .collect(),
        has_next,
    )
}

async fn followers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    connections(&state, target(&state, &username).await?.id, true, page, uri).await
}

async fn following(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    connections(
        &state,
        target(&state, &username).await?.id,
        false,
        page,
        uri,
    )
    .await
}

async fn own_followers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    connections(&state, actor.user.id, true, page, uri).await
}

async fn own_following(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    connections(&state, actor.user.id, false, page, uri).await
}

async fn check(state: &AppState, actor: i64, username: &str) -> Result<StatusCode> {
    let Some(user) = accounts::get_user_by_username(&state.pool, username)
        .await
        .map_err(Error::unavailable)?
    else {
        return Ok(StatusCode::NOT_FOUND);
    };
    if db::following::is_user_following(&state.pool, actor, user.id)
        .await
        .map_err(Error::unavailable)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Ok(StatusCode::NOT_FOUND)
    }
}

async fn check_following(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((username, target_user)): Path<(String, String)>,
) -> Result<StatusCode> {
    auth::authenticate(&state, &headers, false).await?;
    let Some(user) = accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(Error::unavailable)?
    else {
        return Ok(StatusCode::NOT_FOUND);
    };
    check(&state, user.id, &target_user).await
}

async fn check_own_following(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> Result<StatusCode> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    check(&state, actor.user.id, &username).await
}

async fn change_follow(
    state: &AppState,
    headers: &HeaderMap,
    username: &str,
    follow: bool,
) -> Result<StatusCode> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    if !follow && actor.user.username == username {
        return Ok(StatusCode::NO_CONTENT);
    }
    following::change(
        &state.pool,
        &state.notification_broadcaster,
        actor.user.id,
        username,
        follow,
    )
    .await
    .map_err(|error| match error {
        following::FollowError::Missing => Error::missing(),
        following::FollowError::SelfFollow | following::FollowError::SelfUnfollow => {
            Error::validation("User", "login", "invalid")
        }
        following::FollowError::Restricted => {
            Error::new(StatusCode::FORBIDDEN, "Account is restricted")
        }
        _ => Error::unavailable(error),
    })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn follow(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> Result<StatusCode> {
    change_follow(&state, &headers, &username, true).await
}
async fn unfollow(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> Result<StatusCode> {
    change_follow(&state, &headers, &username, false).await
}

#[derive(Deserialize)]
struct Context {
    subject_type: Option<String>,
    subject_id: Option<String>,
}
async fn hovercard(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(query): Query<Context>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let user = target(&state, &username).await?;
    if query.subject_type.as_deref().is_some_and(|kind| {
        !matches!(
            kind,
            "organization" | "repository" | "issue" | "pull_request"
        )
    }) || query
        .subject_id
        .as_ref()
        .is_some_and(|id| id.parse::<i64>().is_err())
    {
        return Err(Error::validation("Hovercard", "subject_type", "invalid"));
    }
    let contexts = if db::following::is_user_following(&state.pool, user.id, actor.user.id)
        .await
        .map_err(Error::unavailable)?
    {
        vec![json!({"message":"Follows you", "octicon":"person"})]
    } else {
        Vec::new()
    };
    Ok(Json(json!({"contexts":contexts})))
}

// gh compares the key without its optional OpenSSH comment when avoiding duplicate uploads.
fn key_text(key: &db::models::SshKey) -> String {
    key.public_key
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

fn key_document(key: &db::models::SshKey) -> Value {
    json!({"id":key.id,"key":key_text(key),"url":api_url(&format!("/user/keys/{}", key.id)),"title":key.name,"created_at":key.created_at,"verified":key.last_used_at.is_some(),"read_only":false,"last_used":key.last_used_at})
}

async fn public_keys(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    let user = target(&state, &username).await?;
    key_list(&state, user.id, page, uri, false).await
}
async fn own_keys(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    key_list(&state, actor.user.id, page, uri, true).await
}
async fn key_list(
    state: &AppState,
    user: i64,
    page: Pagination,
    uri: axum::http::Uri,
    own: bool,
) -> Result<Response> {
    let (offset, limit) = page.window()?;
    // One row past the page says whether another page follows.
    let keys = ssh_keys::user_keys_page(&state.pool, user, offset as i64, limit as i64 + 1)
        .await
        .map_err(Error::unavailable)?;
    let has_next = keys.len() > limit;
    let values = keys.iter().take(limit).map(|key| if own {key_document(key)} else {json!({"id":key.id,"key":key_text(key),"created_at":key.created_at,"last_used":key.last_used_at})}).collect();
    page.response(&uri, values, has_next)
}
async fn key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let key = ssh_keys::get_user_key(&state.pool, actor.user.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(Json(key_document(&key)))
}
#[derive(Deserialize)]
struct KeyInput {
    title: Option<String>,
    key: String,
}
async fn create_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    input: std::result::Result<Json<KeyInput>, axum::extract::rejection::JsonRejection>,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let Json(input) = input.map_err(|_| Error::validation("Key", "key", "invalid"))?;
    let title = input
        .title
        .as_deref()
        .filter(|title| !title.is_empty())
        .unwrap_or("SSH key");
    keys::validate_name(title).map_err(|_| Error::validation("Key", "title", "invalid"))?;
    let key = ssh_keys::add_ssh_key_for_token(
        &state.pool,
        actor.user.id,
        actor.token.id,
        title,
        &input.key,
    )
    .await
    .map_err(
        |error| match error.downcast_ref::<ssh_keys::SshKeyWriteRefusal>() {
            Some(ssh_keys::SshKeyWriteRefusal::InvalidKey) => {
                Error::validation("Key", "key", "invalid")
            }
            Some(ssh_keys::SshKeyWriteRefusal::AlreadyUsed) => {
                Error::validation("Key", "key", "already_exists")
            }
            Some(_) => Error::new(StatusCode::FORBIDDEN, "Account is restricted"),
            None => Error::unavailable(error),
        },
    )?;
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, api_url(&format!("/user/keys/{}", key.id)))],
        Json(key_document(&key)),
    )
        .into_response())
}
async fn delete_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let deleted =
        ssh_keys::delete_user_ssh_key_for_token(&state.pool, actor.user.id, actor.token.id, id)
            .await
            .map_err(
                |error| match error.downcast_ref::<ssh_keys::SshKeyWriteRefusal>() {
                    Some(ssh_keys::SshKeyWriteRefusal::AccountRestricted) => {
                        Error::new(StatusCode::FORBIDDEN, "Account is restricted")
                    }
                    _ => Error::unavailable(error),
                },
            )?;
    if !deleted {
        return Err(Error::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}
