//! JWT discovery and installation management, backed by native account consent.
use super::super::{api_url, auth, pagination::Pagination, repos, Error, Result};
use super::{access_token, application};
use crate::backend::{
    app::AppState,
    db::{
        self,
        github_apps::{App, Installation},
    },
    identity::{github_apps as apps, oauth_apps, tokens::token_hash},
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

pub(super) async fn jwt(state: &AppState, headers: &HeaderMap) -> Result<App> {
    let value = auth::credential(headers).ok_or_else(auth::required)?;
    apps::authenticate(&state.pool, value)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(auth::required)
}
async fn account(state: &AppState, installation: &Installation) -> Result<Value> {
    if let Some(user) = installation.user_id {
        return Ok(payloads::user(
            &db::users::accounts::get_user_by_id(&state.pool, user)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?,
        ));
    }
    let org = db::organizations::profiles::get_public_organization_by_id(
        &state.pool,
        installation.org_id.ok_or_else(Error::missing)?,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(Error::missing)?;
    Ok(
        json!({"login":org.name,"id":org.id,"node_id":super::super::node_id::encode("O",org.id),"avatar_url":format!("{}/orgs/{}/avatar",crate::backend::api::http::config::link_origin(),org.name),"gravatar_id":"","url":api_url(&format!("/orgs/{}",org.name)),"html_url":format!("{}/{}",crate::backend::api::http::config::link_origin(),org.name),"followers_url":api_url(&format!("/users/{}/followers",org.name)),"following_url":api_url(&format!("/users/{}/following{{/other_user}}",org.name)),"gists_url":api_url(&format!("/users/{}/gists{{/gist_id}}",org.name)),"starred_url":api_url(&format!("/users/{}/starred{{/owner}}{{/repo}}",org.name)),"subscriptions_url":api_url(&format!("/users/{}/subscriptions",org.name)),"organizations_url":api_url(&format!("/users/{}/orgs",org.name)),"repos_url":api_url(&format!("/orgs/{}/repos",org.name)),"events_url":api_url(&format!("/users/{}/events{{/privacy}}",org.name)),"received_events_url":api_url(&format!("/users/{}/received_events",org.name)),"type":"Organization","site_admin":false}),
    )
}
pub(in crate::backend::api::github) async fn app_document(
    state: &AppState,
    app: &App,
) -> Result<Value> {
    let owner = db::users::accounts::get_user_by_id(&state.pool, app.owner_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let installations = db::github_apps::installations(&state.pool, Some(app.id), None)
        .await
        .map_err(Error::unavailable)?
        .len();
    app_value(app, &owner, installations)
}
/// An app's document from rows already loaded, for responses that cannot read the database.
pub(in crate::backend::api::github) fn app_value(
    app: &App,
    owner: &db::models::User,
    installations: usize,
) -> Result<Value> {
    Ok(
        json!({"id":app.id,"slug":app.slug,"node_id":super::super::node_id::encode("APP",app.id),"owner":payloads::user(owner),"name":app.name,"description":app.description,"external_url":app.homepage,"html_url":format!("{}/apps/{}",crate::backend::api::http::config::link_origin(),app.slug),"created_at":app.created_at,"updated_at":app.created_at,"permissions":serde_json::from_str::<Value>(&app.permissions).map_err(Error::unavailable)?,"events":app.events.split(',').filter(|s|!s.is_empty()).collect::<Vec<_>>(),"installations_count":installations,"client_id":app.client_id}),
    )
}
pub(in crate::backend::api::github) async fn document(
    state: &AppState,
    installation: &Installation,
) -> Result<Value> {
    let app = db::github_apps::app(&state.pool, &installation.app_id.to_string())
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(
        json!({"id":installation.id,"account":account(state,installation).await?,"repository_selection":if installation.all_repos{"all"}else{"selected"},"access_tokens_url":api_url(&format!("/app/installations/{}/access_tokens",installation.id)),"repositories_url":api_url("/installation/repositories"),"html_url":format!("{}/settings/apps/installations",crate::backend::api::http::config::link_origin()),"app_id":app.id,"app_slug":app.slug,"target_id":installation.user_id.or(installation.org_id),"target_type":if installation.org_id.is_some(){"Organization"}else{"User"},"permissions":serde_json::from_str::<Value>(&app.permissions).map_err(Error::unavailable)?,"events":app.events.split(',').filter(|s|!s.is_empty()).collect::<Vec<_>>(),"created_at":installation.created_at,"updated_at":installation.updated_at,"single_file_name":null,"has_multiple_single_files":false,"single_file_paths":[],"suspended_at":installation.suspended_at,"suspended_by":if let Some(id)=installation.suspended_by{db::users::accounts::get_user_by_id(&state.pool,id).await.map_err(Error::unavailable)?.map(|user|payloads::user(&user))}else{None}}),
    )
}
async fn owned(state: &AppState, app: i64, id: i64) -> Result<Installation> {
    db::github_apps::installation(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .filter(|i| i.app_id == app)
        .ok_or_else(Error::missing)
}
pub(super) async fn installation_requests(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let app = jwt(&state, &headers).await?;
    let rows = db::github_apps::requests(&state.pool, app.id)
        .await
        .map_err(Error::unavailable)?;
    let (offset, limit) = page.window()?;
    let has_next = rows.len() > offset + limit;
    let mut values = vec![];
    for request in rows.into_iter().skip(offset).take(limit) {
        let installation = Installation {
            id: 0,
            app_id: request.app_id,
            user_id: None,
            org_id: Some(request.org_id),
            all_repos: false,
            suspended_at: None,
            suspended_by: None,
            created_at: request.created_at,
            updated_at: request.created_at,
        };
        let requester = db::users::accounts::get_user_by_id(&state.pool, request.user_id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        values.push(json!({"id":request.id,"node_id":super::super::node_id::encode("AIR",request.id),"account":account(&state,&installation).await?,"requester":payloads::user(&requester),"created_at":request.created_at}));
    }
    page.response(&uri, values, has_next)
}

pub(super) async fn get_authenticated(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let app = jwt(&state, &headers).await?;
    Ok(Json(app_document(&state, &app).await?))
}
pub(super) async fn get_by_slug(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Result<Json<Value>> {
    let is_jwt = auth::credential(&headers).is_some_and(|value| value.split('.').count() == 3);
    let jwt_app = if is_jwt {
        Some(jwt(&state, &headers).await?)
    } else {
        None
    };
    let authenticated = if is_jwt {
        None
    } else {
        auth::authenticate(&state, &headers, false).await?
    };
    let app = db::github_apps::app(&state.pool, &slug)
        .await
        .map_err(Error::unavailable)?
        .filter(|app| app.slug == slug)
        .ok_or_else(Error::missing)?;
    if !app.is_public
        && !jwt_app.as_ref().is_some_and(|jwt| jwt.id == app.id)
        && !authenticated
            .as_ref()
            .is_some_and(|auth| auth.user.id == app.owner_id)
    {
        return Err(Error::missing());
    }
    Ok(Json(app_document(&state, &app).await?))
}
pub(super) async fn list_installations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let app = jwt(&state, &headers).await?;
    let rows = db::github_apps::installations(&state.pool, Some(app.id), None)
        .await
        .map_err(Error::unavailable)?;
    let (offset, limit) = page.window()?;
    let has_next = rows.len() > offset + limit;
    let mut values = vec![];
    for row in rows.into_iter().skip(offset).take(limit) {
        values.push(document(&state, &row).await?);
    }
    page.response(&uri, values, has_next)
}
pub(super) async fn get_installation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    let app = jwt(&state, &headers).await?;
    Ok(Json(
        document(&state, &owned(&state, app.id, id).await?).await?,
    ))
}
pub(super) async fn find_installation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let app = jwt(&state, &headers).await?;
    let repo = if let (Some(owner), Some(repo)) = (params.get("owner"), params.get("repo")) {
        Some(
            db::repositories::records::get_repository(&state.pool, owner, repo)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?,
        )
    } else {
        None
    };
    for row in db::github_apps::installations(&state.pool, Some(app.id), None)
        .await
        .map_err(Error::unavailable)?
    {
        let found = if let Some(repo) = &repo {
            db::github_apps::has_repository(&state.pool, row.id, repo.id, false)
                .await
                .map_err(Error::unavailable)?
        } else {
            let account = account(&state, &row).await?;
            if let Some(org) = params.get("org") {
                row.org_id.is_some()
                    && account["login"]
                        .as_str()
                        .is_some_and(|name| name == org.as_str())
            } else {
                // The generated router canonicalizes this username parameter to user_id.
                params.get("user_id").is_some_and(|user| {
                    row.user_id.is_some()
                        && account["login"]
                            .as_str()
                            .is_some_and(|name| name == user.as_str())
                })
            }
        };
        if found {
            return Ok(Json(document(&state, &row).await?));
        }
    }
    Err(Error::missing())
}
async fn change(
    state: &AppState,
    headers: &HeaderMap,
    id: i64,
    action: &str,
) -> Result<StatusCode> {
    let app = jwt(state, headers).await?;
    if db::github_apps::change_installation(&state.pool, app.id, id, action)
        .await
        .map_err(Error::unavailable)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::missing())
    }
}
pub(super) async fn delete_installation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    change(&state, &headers, id, "delete").await
}
pub(super) async fn suspend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    change(&state, &headers, id, "suspend").await
}
pub(super) async fn unsuspend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    change(&state, &headers, id, "unsuspend").await
}

async fn issue(
    state: &AppState,
    app: &App,
    installation: &Installation,
    actor: Option<i64>,
    body: &Value,
    parent: Option<&db::github_apps::Token>,
) -> Result<Value> {
    if installation.suspended_at.is_some() {
        return Err(Error::new(StatusCode::FORBIDDEN, "Installation suspended"));
    }
    if !body.is_object() {
        return Err(Error::invalid("Expected a token configuration object"));
    }
    let grant = serde_json::from_str::<Value>(parent.map_or(&app.permissions, |t| &t.permissions))
        .map_err(Error::unavailable)?;
    let permissions = body
        .get("permissions")
        .map_or_else(|| Some(grant.clone()), |value| apps::subset(value, &grant))
        .ok_or_else(|| Error::invalid("Permissions exceed the app grant"))?;
    let parent_repos: Vec<i64> = parent
        .map(|t| serde_json::from_str(&t.repositories))
        .transpose()
        .map_err(Error::unavailable)?
        .unwrap_or_default();
    if body.get("repository_ids").is_some() && body.get("repositories").is_some() {
        return Err(Error::invalid("Choose repository names or IDs, not both"));
    }
    let mut ids = if let Some(ids) = body.get("repository_ids") {
        serde_json::from_value::<Vec<i64>>(ids.clone())
            .map_err(|_| Error::invalid("Invalid repository IDs"))?
    } else if let Some(names) = body.get("repositories") {
        let names = serde_json::from_value::<Vec<String>>(names.clone())
            .map_err(|_| Error::invalid("Invalid repository names"))?;
        if names.len() > 500 {
            return Err(Error::invalid("Select at most 500 repositories"));
        }
        let owner = account(state, installation).await?;
        let owner = owner["login"].as_str().ok_or_else(Error::missing)?;
        let mut ids = vec![];
        for name in names {
            let repo = db::repositories::records::get_repository(&state.pool, owner, &name)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(|| Error::invalid("Repository is outside this installation"))?;
            ids.push(repo.id);
        }
        ids
    } else {
        parent_repos.clone()
    };
    if ids.len() > 500 {
        return Err(Error::invalid("Select at most 500 repositories"));
    }
    ids.sort_unstable();
    ids.dedup();
    if ids
        .iter()
        .any(|id| !parent_repos.is_empty() && !parent_repos.contains(id))
    {
        return Err(Error::invalid("Repositories exceed the installation grant"));
    }
    if (body.get("repository_ids").is_some() || body.get("repositories").is_some())
        && ids.is_empty()
    {
        return Err(Error::invalid("Select at least one repository"));
    }
    let value = format!(
        "{}_{}",
        if actor.is_some() { "ghu" } else { "ghs" },
        oauth_apps::random()
    );
    let source = body
        .get("access_token")
        .and_then(Value::as_str)
        .map(token_hash);
    let token = db::github_apps::issue(
        &state.pool,
        app.id,
        installation.id,
        actor,
        source.as_deref(),
        &token_hash(&value),
        &permissions.to_string(),
        &serde_json::to_string(&ids).map_err(Error::unavailable)?,
        apps::write(&permissions),
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(|| Error::invalid("Installation changed or repositories are unavailable"))?;
    let mut response = json!({"token":value,"expires_at":token.expires_at,"permissions":permissions,"repository_selection":if installation.all_repos&&ids.is_empty(){"all"}else{"selected"}});
    if !ids.is_empty() {
        let mut repositories = vec![];
        for repo in db::repositories::records::get_repositories_by_ids(&state.pool, &ids)
            .await
            .map_err(Error::unavailable)?
        {
            if repo.owner_user_id == installation.user_id && installation.user_id.is_some()
                || repo.owner_org_id == installation.org_id && installation.org_id.is_some()
            {
                repositories.push(repos::document(state, &repo, None).await?);
            }
        }
        response["repositories"] = json!(repositories);
    }
    Ok(response)
}
pub(super) async fn create_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    body: Option<Json<Value>>,
) -> Result<(StatusCode, Json<Value>)> {
    let app = jwt(&state, &headers).await?;
    let installation = owned(&state, app.id, id).await?;
    Ok((
        StatusCode::CREATED,
        Json(
            issue(
                &state,
                &app,
                &installation,
                None,
                &body.map_or_else(|| json!({}), |body| body.0),
                None,
            )
            .await?,
        ),
    ))
}

pub(super) async fn installation_token(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(auth::Authentication, db::github_apps::Token)> {
    let auth = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let token = db::github_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?
        .filter(|token| !token.user_token)
        .ok_or_else(auth::required)?;
    Ok((auth, token))
}
pub(super) async fn list_repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (auth, token) = installation_token(&state, &headers).await?;
    repository_list(
        &state,
        token.installation_id,
        Some(&token),
        Some(&auth.user),
        &uri,
        &page,
    )
    .await
}
async fn repository_list(
    state: &AppState,
    id: i64,
    token: Option<&db::github_apps::Token>,
    user: Option<&db::models::User>,
    uri: &axum::http::Uri,
    page: &Pagination,
) -> Result<Response> {
    let selected: Vec<i64> = token
        .map(|t| serde_json::from_str(&t.repositories))
        .transpose()
        .map_err(Error::unavailable)?
        .unwrap_or_default();
    let installation = db::github_apps::installation(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let all_repositories = installation.all_repos && selected.is_empty();
    let mut rows = db::github_apps::repositories(&state.pool, id)
        .await
        .map_err(Error::unavailable)?;
    rows.retain(|repo| selected.is_empty() || selected.contains(&repo.id));
    if let Some(user) = user {
        // Check read access a page of repositories at a time rather than one by one.
        let mut readable = std::collections::HashSet::new();
        for chunk in rows.chunks(100) {
            let ids: Vec<i64> = chunk.iter().map(|repo| repo.id).collect();
            readable.extend(
                db::repositories::access::readable_repository_ids(&state.pool, Some(user.id), &ids)
                    .await
                    .map_err(Error::unavailable)?,
            );
        }
        rows.retain(|repo| readable.contains(&repo.id));
    }
    let total = rows.len();
    let (offset, limit) = page.window()?;
    let mut values = vec![];
    for repo in rows.iter().skip(offset).take(limit) {
        values.push(repos::document(state, repo, user).await?);
    }
    Ok((page.headers(uri,total>offset+limit)?,Json(json!({"total_count":total,"repository_selection":if all_repositories{"all"}else{"selected"},"repositories":values}))).into_response())
}
pub(super) async fn revoke_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<StatusCode> {
    let (auth, _) = installation_token(&state, &headers).await?;
    db::github_apps::revoke(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn user_app(state: &AppState, auth: &auth::Authentication) -> Result<Option<i64>> {
    if let Some(token) = db::github_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?
    {
        return Ok(
            db::github_apps::installation(&state.pool, token.installation_id)
                .await
                .map_err(Error::unavailable)?
                .map(|i| i.app_id),
        );
    }
    if let Some(token) = db::oauth_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?
    {
        return Ok(db::github_apps::app(&state.pool, &token.client_id)
            .await
            .map_err(Error::unavailable)?
            .map(|app| app.id));
    }
    Ok(None)
}
pub(super) async fn user_installations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let auth = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let app = user_app(&state, &auth).await?;
    let mut rows = db::github_apps::installations(&state.pool, app, Some(auth.user.id))
        .await
        .map_err(Error::unavailable)?;
    if let Some(token) = db::github_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?
    {
        rows.retain(|row| row.id == token.installation_id);
    }
    let total = rows.len();
    let (offset, limit) = page.window()?;
    let mut values = vec![];
    for row in rows.into_iter().skip(offset).take(limit) {
        values.push(document(&state, &row).await?);
    }
    Ok((
        page.headers(&uri, total > offset + limit)?,
        Json(json!({"total_count":total,"installations":values})),
    )
        .into_response())
}
pub(super) async fn user_repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let auth = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let app = user_app(&state, &auth).await?;
    if !db::github_apps::installations(&state.pool, app, Some(auth.user.id))
        .await
        .map_err(Error::unavailable)?
        .iter()
        .any(|i| i.id == id && i.suspended_at.is_none())
    {
        return Err(Error::missing());
    }
    let token = db::github_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?;
    if token
        .as_ref()
        .is_some_and(|token| token.installation_id != id)
    {
        return Err(Error::missing());
    }
    repository_list(&state, id, token.as_ref(), Some(&auth.user), &uri, &page).await
}
async fn change_repository(
    state: &AppState,
    headers: &HeaderMap,
    id: i64,
    repo: i64,
    add: bool,
) -> Result<StatusCode> {
    let auth = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    auth.require_write()?;
    // A narrowed app token may not widen its own repository grant.
    if db::github_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?
        .is_some()
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Use an account authorization to change installation access",
        ));
    }
    if let Some(app) = user_app(state, &auth).await? {
        owned(state, app, id).await?;
    }
    if !db::github_apps::change_repository(&state.pool, auth.user.id, id, repo, add)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}
pub(super) async fn add_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, repo)): Path<(i64, i64)>,
) -> Result<StatusCode> {
    change_repository(&state, &headers, id, repo, true).await
}
pub(super) async fn remove_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, repo)): Path<(i64, i64)>,
) -> Result<StatusCode> {
    change_repository(&state, &headers, id, repo, false).await
}
pub(super) async fn scope_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(client): Path<String>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>)> {
    let oauth_app = application(&state, &headers, &client).await?;
    let source = super::owned_token(&state, &client, &token_hash(access_token(&body)?)).await?;
    let app = db::github_apps::app(&state.pool, &client)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let parent = db::github_apps::token(&state.pool, source.id)
        .await
        .map_err(Error::unavailable)?;
    let mut target = None;
    for installation in
        db::github_apps::installations(&state.pool, Some(app.id), Some(source.user_id))
            .await
            .map_err(Error::unavailable)?
    {
        let account = account(&state, &installation).await?;
        let id = body.get("target_id").or_else(|| body.get("account_id"));
        let name = body.get("target").or_else(|| body.get("account"));
        if (id.is_some() || name.is_some())
            && id.is_none_or(|id| *id == account["id"])
            && name.is_none_or(|name| *name == account["login"])
            && parent
                .as_ref()
                .is_none_or(|parent| parent.installation_id == installation.id)
        {
            if target.is_some() {
                return Err(Error::invalid(
                    "Account IDs overlap; select the target by login",
                ));
            }
            target = Some(installation);
        }
    }
    let installation =
        target.ok_or_else(|| Error::invalid("Choose an accessible target account"))?;
    if parent
        .as_ref()
        .is_some_and(|parent| parent.installation_id != installation.id)
    {
        return Err(Error::invalid("Target exceeds the original grant"));
    }
    let issued = issue(
        &state,
        &app,
        &installation,
        Some(source.user_id),
        &body,
        parent.as_ref(),
    )
    .await?;
    let value = issued["token"]
        .as_str()
        .ok_or_else(|| Error::unavailable(()))?;
    let token = db::users::tokens::find_token(&state.pool, &token_hash(value))
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok((
        StatusCode::OK,
        Json(super::document(&state, &oauth_app, &token, value).await?),
    ))
}
